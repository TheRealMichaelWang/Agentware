#!/usr/bin/env python3
"""Put the agent's Anthropic API key on the state volume, without booting.

The key is a setting like any other, so it lives in `settings.xml` on the
state volume and is normally typed on the Settings app's Agent page. That is
the right way when a human is at the machine. It is the wrong way when the
machine is being driven by `tools/screenshot.py`, or when a fresh
`make cleanstate` has to come up already able to answer, because typing a
sixty-character secret through the QEMU monitor one `sendkey` at a time is
neither reliable nor private.

So this edits the image directly. `debugfs -w` writes into an ext4 filesystem
as an ordinary user, the same way `mkfs.ext4 -d` populates one, which is what
keeps sudo out of the build. Nothing is mounted and nothing is booted.

The machine must not be running: QEMU has the image open and would write its
own copy of the block over this one at any moment.

Usage: tools/setkey.py STATE.img [--key KEY]

The key comes from `--key`, else `$ANTHROPIC_API_KEY`, else a silent prompt.
Prefer the last two: an argument is visible in `ps` and in shell history.
"""

import argparse
import getpass
import os
import re
import subprocess
import sys
import tempfile

# The file, and the shape awproto::settings writes it in. Reproduced rather
# than imported because this runs on the host, outside the guest's Rust: a
# first run has no file at all, and something has to be able to write one.
SETTINGS = "settings.xml"
DEFAULT_WALLPAPER = "/default_wallpapers/dusk.svg"
DEFAULT_THEME = "/default_themes/dark.xml"


def escape(text):
    """The five XML entities, exactly as the settings module escapes them."""
    for character, entity in (("&", "&amp;"), ("<", "&lt;"), (">", "&gt;"),
                              ('"', "&quot;"), ("'", "&apos;")):
        text = text.replace(character, entity)
    return text


def debugfs(image, command, writable=False):
    """One debugfs command against the image, with its output."""
    argv = ["debugfs"]
    if writable:
        argv.append("-w")
    argv += ["-R", command, image]
    done = subprocess.run(argv, capture_output=True, text=True)
    if done.returncode != 0:
        raise SystemExit("debugfs %r failed: %s" % (command, done.stderr.strip()))
    return done.stdout


def read_settings(image):
    """The settings file as it stands, or None if the volume has none yet."""
    argv = ["debugfs", "-R", "cat /%s" % SETTINGS, image]
    done = subprocess.run(argv, capture_output=True, text=True)
    # debugfs reports a missing file on stderr and still exits zero, so the
    # complaint is what has to be checked rather than the status.
    if done.returncode != 0 or "File not found" in done.stderr:
        return None
    return done.stdout


def defaults_with(key):
    """A whole settings file, for a volume that has none yet."""
    return (
        "<settings>\n"
        "  <desktop>\n"
        "    <wallpaper>%s</wallpaper>\n"
        "    <theme>%s</theme>\n"
        "  </desktop>\n"
        "  <time>\n"
        "    <utc-offset>+00:00</utc-offset>\n"
        "  </time>\n"
        "  <agent>\n"
        "    <anthropic-key>%s</anthropic-key>\n"
        "  </agent>\n"
        "</settings>\n" % (DEFAULT_WALLPAPER, DEFAULT_THEME, escape(key))
    )


def with_key(text, key):
    """The same settings, with this key in them.

    Edited rather than rewritten, so a wallpaper, theme or time zone the human
    chose on the machine survives having a key put beside it. An older file
    with no `<agent>` section gains one; anything that does not look like the
    file the settings module writes is replaced with the defaults, which is
    what that module does with it too.
    """
    if "<settings>" not in text:
        return defaults_with(key)

    value = escape(key)
    element = r"<anthropic-key\s*/>|<anthropic-key>.*?</anthropic-key>"
    if re.search(element, text, re.S):
        # Replaced through a function so that a backslash in the key is a
        # backslash rather than a regex group reference.
        return re.sub(element, lambda _: "<anthropic-key>%s</anthropic-key>" % value,
                      text, count=1, flags=re.S)

    section = "  <agent>\n    <anthropic-key>%s</anthropic-key>\n  </agent>\n" % value
    if "<agent>" in text:
        # An agent section holding something else. Put the key first in it.
        return text.replace("<agent>", "<agent>\n    <anthropic-key>%s</anthropic-key>" % value, 1)
    return text.replace("</settings>", section + "</settings>", 1)


def write_settings(image, text):
    """Replace the settings file in the image.

    `rm` unlinks without freeing the old inode's blocks, which leaves the
    filesystem inconsistent in exactly the way `e2fsck` exists to repair, so
    the check afterwards is part of the write rather than a precaution. Its
    exit code 1 means it corrected something, which is the expected outcome.
    """
    scratch = tempfile.mkdtemp(prefix="agentware-key-")
    local = os.path.join(scratch, SETTINGS)
    try:
        with open(local, "w") as handle:
            handle.write(text)

        debugfs(image, "rm /%s" % SETTINGS, writable=True)
        debugfs(image, "write %s %s" % (local, SETTINGS), writable=True)

        done = subprocess.run(["e2fsck", "-fy", image], capture_output=True, text=True)
        if done.returncode > 1:
            raise SystemExit("e2fsck could not repair %s:\n%s" % (image, done.stdout))
    finally:
        if os.path.exists(local):
            os.remove(local)
        os.rmdir(scratch)


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("image", help="the state volume image, usually state.img")
    parser.add_argument("--key", default=None,
                        help="the key; otherwise $ANTHROPIC_API_KEY, otherwise a prompt")
    args = parser.parse_args()

    if not os.path.exists(args.image):
        raise SystemExit(
            "no state volume at %s. `make run` creates one on a first boot, and "
            "`make configure_anthropic_key` creates one before writing to it." % args.image
        )

    key = args.key or os.environ.get("ANTHROPIC_API_KEY") or ""
    if not key.strip():
        try:
            key = getpass.getpass("Anthropic API key (not echoed): ")
        except (EOFError, KeyboardInterrupt):
            # No terminal to ask at, which is every automated caller. Saying
            # what to pass beats a traceback about standard input.
            raise SystemExit(
                "\nno key given. Pass one as KEY=... or in $ANTHROPIC_API_KEY, "
                "or run this where it can prompt."
            )
    key = key.strip()
    if not key:
        raise SystemExit("no key given; nothing written")

    held = read_settings(args.image)
    text = with_key(held, key) if held is not None else defaults_with(key)
    write_settings(args.image, text)

    # The key itself is never printed: this runs in terminals people paste
    # into issues. The length and the tail are enough to tell one key from
    # another and enough to catch a truncated paste.
    print("==> key set on %s: %d characters ending %s" % (args.image, len(key), key[-4:]))
    if held is None:
        print("    (the volume had no settings file; wrote one with the defaults)")


if __name__ == "__main__":
    main()
