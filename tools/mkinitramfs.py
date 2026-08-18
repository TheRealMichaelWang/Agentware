#!/usr/bin/env python3
"""Write the boot initramfs: the supervisor, a console, and nothing else.

The initramfs holds exactly what must exist before any disk does: /init (the
supervisor, PID 1) and the /dev/console node its early messages go to. The
kernel does not auto-mount devtmpfs for an initramfs, so the node has to be
in the archive; a device node in an archive is just a header saying 5:1, so
writing one needs no privileges, which is the whole reason this script exists
instead of `find | cpio` over a staging tree that would have needed sudo to
hold the node.

Usage: tools/mkinitramfs.py SUPERVISOR_BINARY OUT.cpio.gz
"""

import gzip
import sys


def entry(name, mode, body=b"", rdev=(0, 0), nlink=1):
    """One newc-format cpio record."""
    header = "070701" + "".join(
        "%08x" % field
        for field in (
            0,              # ino
            mode,
            0, 0,           # uid, gid: root
            nlink,
            0,              # mtime
            len(body),
            0, 0,           # dev
            rdev[0], rdev[1],
            len(name) + 1,
            0,              # check
        )
    )
    out = header.encode() + name.encode() + b"\0"
    out += b"\0" * (-len(out) % 4)
    out += body
    out += b"\0" * (-len(body) % 4)
    return out


def main():
    if len(sys.argv) != 3:
        print(__doc__.strip(), file=sys.stderr)
        return 2
    with open(sys.argv[1], "rb") as source:
        supervisor = source.read()

    archive = b"".join([
        entry("dev", 0o040755, nlink=2),
        entry("dev/console", 0o020600, rdev=(5, 1)),
        entry("init", 0o100755, body=supervisor),
        entry("TRAILER!!!", 0),
    ])
    with gzip.open(sys.argv[2], "wb", compresslevel=9) as out:
        out.write(archive)
    return 0


if __name__ == "__main__":
    sys.exit(main())
