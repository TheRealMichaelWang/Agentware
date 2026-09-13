#!/usr/bin/env python3
"""Measure what the cache design rests on: slot save and restore, whether a
restored slot is actually reused, and what a model load pins.

Host side, no guest. Starts llama-server itself, once per configuration, so
the memory a load pins can be read off `MemAvailable` before and after, which
is the honest number on this machine: the file's pages are reclaimable and do
not count, the KV cache and the weights the GPU holds are pinned and do.

For each server variant (default; the RAM prompt cache off; a single slot),
for each KV precision (f16, q8_0), one server at the full context:
  * the memory pinned by loading it
  * a conversation of about N tokens prefilled into slot 0, timed
  * the same conversation again: the plain cache hit, which is the baseline
    everything below is compared with
  * save the slot to disk, timed, and the file's size
  * erase the slot and send the conversation again, which must prefill
  * restore the slot from disk, timed, and send it again: the question
Every exchange reports what the server says it reused (`cache_n`) beside
what it prefilled (`prompt_n`), so a hit and a miss are told apart by the
server's own count and not by the clock alone.

The three-tier design in docs/OnDevice.md assumes a restore is a second or
two and that the restored conversation is then found. The first of those
was measured at 114k tokens: 0.3s to restore against 242s to prefill. The
second is what the repeat after the restore answers.

    tools/slotbench.py                         # everything, 20k tokens
    tools/slotbench.py --tokens 114000 --only f16 --variant default
"""

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

HOME = os.path.expanduser("~")
SERVER = os.path.join(HOME, "llm/llama.cpp/build-vulkan/bin/llama-server")
MODEL = os.path.join(HOME, "llm/models/Qwen3.6-35B-A3B-UD-Q5_K_XL.gguf")
URL = "http://127.0.0.1:8080"
CONTEXT = 131072

PRECISIONS = {
    "f16": [],
    "q8_0": ["-ctk", "q8_0", "-ctv", "q8_0"],
}

# What the server does with idle slots between requests is the thing under
# test, so it is varied. `--cache-idle-slots` (on by default, needs the RAM
# prompt cache) saves an idle slot to RAM on a new task and, with unified KV,
# clears it; a slot restored from disk may therefore be gone before it is
# matched. The RAM cache off, and a single slot, are the two ways of
# switching that off.
VARIANTS = {
    "default": [],
    "no-ram-cache": ["--cache-ram", "0"],
    "one-slot": ["-np", "1"],
}


def mem_available():
    """Kilobytes the kernel says are available, which is what a pinned
    allocation takes away."""
    with open("/proc/meminfo") as handle:
        for line in handle:
            if line.startswith("MemAvailable:"):
                return int(line.split()[1])
    raise RuntimeError("no MemAvailable in /proc/meminfo")


def post(path, body, timeout=1800):
    request = urllib.request.Request(
        f"{URL}{path}",
        data=json.dumps(body).encode(),
        headers={"content-type": "application/json"},
    )
    with urllib.request.urlopen(request, timeout=timeout) as response:
        return json.loads(response.read())


def healthy():
    try:
        with urllib.request.urlopen(f"{URL}/health", timeout=2) as response:
            return response.status == 200
    except (urllib.error.URLError, OSError):
        return False


class Server:
    """One llama-server, started for one configuration and stopped after."""

    def __init__(self, extra, slots_dir, log):
        self.command = [
            SERVER, "-m", MODEL, "-ngl", "99", "-c", str(CONTEXT), "-fa", "on",
            "--jinja", "--spec-type", "draft-mtp", "--host", "127.0.0.1", "--port", "8080",
            "--slot-save-path", slots_dir, *extra,
        ]
        self.log = log

    def __enter__(self):
        self.before = mem_available()
        self.process = subprocess.Popen(self.command, stdout=self.log, stderr=subprocess.STDOUT)
        started = time.monotonic()
        while not healthy():
            if self.process.poll() is not None:
                raise RuntimeError("llama-server exited while loading; see the log")
            if time.monotonic() - started > 600:
                raise RuntimeError("llama-server did not come up in ten minutes")
            time.sleep(2)
        self.load_seconds = time.monotonic() - started
        # The load settles for a moment after health answers.
        time.sleep(3)
        self.pinned_kb = self.before - mem_available()
        return self

    def __exit__(self, *_):
        self.process.terminate()
        try:
            self.process.wait(timeout=60)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait()


def conversation(tokens):
    """A conversation of about this many tokens, sized with /tokenize.

    Distinct lines rather than one phrase repeated, so nothing about the
    prefill is easier than a real transcript would be. Built once at a
    guess, measured, and rebuilt to the target.
    """
    def lines(count):
        return "\n".join(
            f"Entry {i}: the explorer listed folder {i * 7 % 97} holding {i % 13} files, "
            f"and the sheet took row {i + 2}."
            for i in range(count)
        )

    guess = max(tokens // 30, 1)
    measured = len(post("/tokenize", {"content": lines(guess)})["tokens"])
    count = max(int(guess * tokens / measured), 1)
    text = lines(count)
    return text, len(post("/tokenize", {"content": text})["tokens"])


def exchange(text):
    """One request carrying the conversation as raw text, one token
    generated, timed; what the server says it prefilled and reused; and
    the token it generated, so a continuation can include it.

    The raw completion endpoint rather than chat, so the token sequence is
    exactly the text and nothing a template adds: what is being measured is
    whether a prefix is found, and a template wrapping each request would
    make every continuation diverge at its seams.
    """
    started = time.monotonic()
    answer = post("/completion", {
        "prompt": text,
        "n_predict": 1,
        "cache_prompt": True,
        "id_slot": 0,
        "temperature": 0,
    })
    seconds = time.monotonic() - started
    timings = answer.get("timings", {})
    return seconds, timings.get("prompt_n"), timings.get("cache_n"), answer.get("content", "")


def said(label, seconds, prefilled, reused, _generated=""):
    print(f"    {label:<32} {seconds:7.2f}s   prefilled {prefilled}   reused {reused}")
    return {"s": round(seconds, 2), "prefilled": prefilled, "reused": reused}


def measure(variant, precision, tokens, slots_dir, log):
    extra = VARIANTS[variant] + PRECISIONS[precision]
    with Server(extra, slots_dir, log) as server:
        row = {
            "variant": variant, "precision": precision, "context": CONTEXT,
            "load_s": round(server.load_seconds, 1),
            "pinned_gib": round(server.pinned_kb / 1024 / 1024, 2),
        }
        print(f"{variant} {precision}: load {row['load_s']}s, pinned {row['pinned_gib']} GiB")
        text, row["tokens"] = conversation(tokens)
        print(f"    conversation of {row['tokens']} tokens")

        # Three shapes of next request, because a hybrid model (this one:
        # its linear attention layers carry a recurrent state, which cannot
        # be rewound) tells them apart:
        #
        # * the same text again: the server re-evaluates the last token,
        #   which needs the state from *before* it, which only a checkpoint
        #   from the original prefill holds;
        # * the text with the sampled token replaced: a rewind to before the
        #   divergence, again a checkpoint;
        # * the text, the sampled token, and more: an exact continuation,
        #   which needs only the state at the end, which is what a saved
        #   slot is. This is the agent's case: every exchange resends the
        #   model's own reply verbatim and adds to it.
        more = "\nEntry more: the explorer listed one more folder, holding two files."
        last = "\nEntry last: and the sheet took the row after that."

        cold = exchange(text)
        row["cold"] = said("cold prefill", *cold)
        row["repeat"] = said("plain repeat", *exchange(text))
        continued = text + cold[3] + more
        went_on = exchange(continued)
        row["continued"] = said("plain, exact continuation", *went_on)
        # The slot now holds `continued` and the token sampled after it.
        further = continued + went_on[3] + last

        filename = f"slot-{variant}-{precision}.bin"
        started = time.monotonic()
        saved = post("/slots/0?action=save", {"filename": filename})
        row["save_s"] = round(time.monotonic() - started, 2)
        row["slot_gib"] = round(os.path.getsize(os.path.join(slots_dir, filename)) / 1024 ** 3, 3)
        print(f"    save: {row['save_s']}s, {saved.get('n_saved')} tokens, {row['slot_gib']} GiB")

        post("/slots/0?action=erase", {})
        row["after_erase"] = said("after erase, same", *exchange(continued))

        def restore():
            post("/slots/0?action=erase", {})
            started = time.monotonic()
            restored = post("/slots/0?action=restore", {"filename": filename})
            seconds = time.monotonic() - started
            print(f"    restore: {seconds:.2f}s, {restored.get('n_restored')} tokens")
            return round(seconds, 2)

        row["restore_s"] = restore()
        row["after_restore_continued"] = said("after restore, exact continuation", *exchange(further))
        restore()
        row["after_restore_same"] = said("after restore, same", *exchange(continued))
        restore()
        row["after_restore_diverged"] = said("after restore, diverged", *exchange(continued + last))
        return row


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--tokens", type=int, default=20000,
                        help="how long a conversation to save and restore")
    parser.add_argument("--only", action="append", default=[], choices=list(PRECISIONS))
    parser.add_argument("--variant", action="append", default=[], choices=list(VARIANTS))
    parser.add_argument("--json", default=None, help="write the rows here")
    args = parser.parse_args()

    if healthy():
        print("a server is already on port 8080; stop it first", file=sys.stderr)
        return 1
    if not os.path.exists(SERVER) or not os.path.exists(MODEL):
        print(f"need {SERVER} and {MODEL}", file=sys.stderr)
        return 1

    slots_dir = tempfile.mkdtemp(prefix="slotbench-")
    rows = []
    try:
        with open(os.path.join(slots_dir, "server.log"), "w") as log:
            for variant in VARIANTS:
                if args.variant and variant not in args.variant:
                    continue
                for precision in PRECISIONS:
                    if args.only and precision not in args.only:
                        continue
                    rows.append(measure(variant, precision, args.tokens, slots_dir, log))
                    sys.stdout.flush()
    finally:
        # The slot files are gigabytes; the log is kept alongside if asked for.
        for name in os.listdir(slots_dir):
            if name.endswith(".bin"):
                os.remove(os.path.join(slots_dir, name))
        if args.json:
            with open(args.json, "w") as handle:
                json.dump(rows, handle, indent=2)
            print(f"wrote {args.json}; server log in {slots_dir}")
        else:
            shutil.rmtree(slots_dir, ignore_errors=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
