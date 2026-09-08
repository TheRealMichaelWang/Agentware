#!/usr/bin/env python3
"""Run agent tasks with checkable outcomes, and report a pass rate.

The only thing that can tell a fast model from a good one. Everything else
measured about the local path says how quickly a turn happens; nothing said
whether it did the right thing, so "faster" has been claimed against one run
of one task. This is the fixture the rest of the work argues with.

Each task is a prompt and an assertion. The assertion is made from outside
the guest, against two things the machine leaves behind:

* **The files it wrote**, read back out of the state volume with `debugfs`,
  the same no-sudo trick `make configure_anthropic_key` uses. A spreadsheet
  the agent saved is either there and right or it is not, and no amount of
  the agent saying so changes that.
* **The serial log**, which carries the agent's telemetry, its reply, and the
  `turn:` accounting line. That line is the end-of-turn marker a headless run
  needs, and it is why this could be written without touching the guest.

The model is nondeterministic, so a task is run several times and the result
is a rate, not a boolean.

    tools/tasksuite.py                       # every task, 3 runs each
    tools/tasksuite.py --only calculator     # one task
    tools/tasksuite.py --runs 5 --json out.json
"""

import argparse
import json
import os
import re
import shutil
import statistics
import subprocess
import sys
import tempfile
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import screenshot  # noqa: E402  the QEMU plumbing, rather than a second copy of it

ROOT = screenshot.ROOT
STATE_IMG = os.path.join(ROOT, "state.img")

# Where the composer sits, as a fraction of the display. The pane is a fixed
# proportion of the screen, so this holds at any resolution.
COMPOSER = (0.904, 0.907)

# The accounting line the harness prints at the end of every turn, however the
# turn ended. Parsed for the table as well as watched for as the finish line.
TURN_LINE = re.compile(
    r"awagent: turn: (\d+)ms = model (\d+)ms \+ acting (\d+)ms \+ reading (\d+)ms"
    r" \+ waiting (\d+)ms \+ harness (\d+)ms; (\d+) exchange\(s\), (\d+) action\(s\);"
    r" in (\d+) \(cache read (\d+), write (\d+)\) out (\d+)"
)
REPLY_LINE = re.compile(r"awagent: reply: (.*)")


class Outcome:
    """What one run of one task left behind."""

    def __init__(self, log, state):
        self.log = log
        self.state = state
        self.metrics = {}
        self.reply = ""
        self._files = {}

        for line in log.splitlines():
            found = TURN_LINE.search(line)
            if found:
                names = ("wall", "model", "acting", "reading", "waiting", "harness",
                         "exchanges", "actions", "input", "cache_read", "cache_write",
                         "output")
                self.metrics = dict(zip(names, (int(n) for n in found.groups())))
            found = REPLY_LINE.search(line)
            if found:
                self.reply = found.group(1)

    @property
    def finished(self) -> bool:
        """Whether a turn ran to an ending of its own rather than being cut off."""
        return bool(self.metrics)

    def file(self, path: str) -> str:
        """A file from the machine's state volume, or "" if it is not there.

        `/home` is a bind out of `/state/home`, so a path the agent saved to
        as /home/x is /home/x inside the volume too.
        """
        if path not in self._files:
            got = subprocess.run(
                ["debugfs", "-R", "cat %s" % path, self.state],
                capture_output=True, text=True,
            )
            self._files[path] = got.stdout if got.returncode == 0 else ""
        return self._files[path]

    def said(self, *words: str) -> bool:
        """Whether the reply mentions all of these, case insensitively."""
        low = self.reply.lower()
        return all(word.lower() in low for word in words)


class Task:
    def __init__(self, name, prompt, check, why, seconds=180):
        self.name = name
        self.prompt = prompt
        self.check = check
        # What the assertion is actually asking, for the report. A pass rate
        # nobody can interpret is a number, not a result.
        self.why = why
        self.seconds = seconds


def csv_names(outcome, path):
    """The set of bare filenames mentioned anywhere in a saved CSV."""
    return {cell.strip().strip('"').rsplit("/", 1)[-1]
            for line in outcome.file(path).splitlines()
            for cell in line.split(",")}


TASKS = [
    Task(
        name="calculator",
        prompt="Add 12 and 34 on the calculator.",
        why="the reply says 46",
        check=lambda got: got.said("46"),
        seconds=120,
    ),
    Task(
        name="open-and-edit",
        prompt="Open quarter.csv in the spreadsheet and put 500 into cell D4, then save it.",
        why="quarter.csv contains 500",
        check=lambda got: "500" in got.file("/home/quarter.csv"),
        seconds=240,
    ),
    Task(
        name="new-sheet",
        prompt="Make a new spreadsheet with the word Hello in cell A1 and save it as "
               "greeting.csv in the home folder.",
        why="greeting.csv exists and holds Hello",
        check=lambda got: "hello" in got.file("/home/greeting.csv").lower(),
        seconds=240,
    ),
    Task(
        name="write-text",
        prompt="Make a new text file called reminder.txt in the home folder that says "
               "Buy milk.",
        why="reminder.txt exists and holds the sentence",
        check=lambda got: "milk" in got.file("/home/reminder.txt").lower(),
        seconds=240,
    ),
    Task(
        name="find-app",
        # Nothing here names an application. The agent has to work out which
        # one from what it is being asked to do, which is what search_apps is
        # for and what the machine could not answer at all before it existed.
        prompt="What is 144 divided by 12? Work it out on this machine rather than in "
               "your head.",
        why="the reply says 12, having found the app itself",
        check=lambda got: got.said("12"),
        seconds=120,
    ),
    Task(
        name="traverse",
        # The long-horizon one. Two applications, a directory walk that has to
        # descend and come back, and a result carried between them. This is
        # the task that found three real defects the first time it was run.
        prompt="Create a spreadsheet listing all the text files on this machine. "
               "Traverse the filesystem manually with the file explorer; there is no "
               "search.",
        why="a saved CSV names all three text files under /home",
        check=lambda got: any(
            {"welcome.txt", "ideas.txt", "shopping.txt"} <= csv_names(got, path)
            for path in ("/home/sheet.csv", "/home/textfiles.csv", "/home/files.csv")
        ),
        seconds=600,
    ),
]


def run_once(task, backend, keep):
    """Boot, send the prompt, wait for the turn to end, and read what is left."""
    work = tempfile.mkdtemp(prefix="tasksuite-")
    state = os.path.join(work, "state.img")
    # Its own copy of the machine's disk, so a task that writes files is
    # checked against what it actually wrote and no run can see another's
    # leavings.
    shutil.copyfile(STATE_IMG, state)
    serial = os.path.join(work, "serial.log")
    monitor = os.path.join(work, "monitor.sock")
    qmp = os.path.join(work, "qmp.sock")

    # The backend is named rather than implied. It used to be decided by
    # whether an API key was set, which is right for a person at the machine
    # and useless for a measurement: the moment a key exists for testing the
    # hosted model, every local run silently becomes a hosted one.
    append = ("console=ttyS0,115200 printk.devkmsg=on"
              " ip=10.0.2.15::10.0.2.2:255.255.255.0:agentware:eth0:off"
              f" agentware.backend={backend}")
    guest = screenshot.qemu(monitor, qmp, serial, append, 1600, 1000, state, keep_state=True)
    try:
        for path in (monitor, qmp):
            for _ in range(200):
                if os.path.exists(path):
                    break
                time.sleep(0.05)
        time.sleep(8)  # the boot, before anything is typed at it

        screenshot.qmp_tablet(qmp, *COMPOSER, "click")
        time.sleep(0.4)
        for stroke in screenshot.keystrokes(task.prompt):
            screenshot.monitor_command(monitor, stroke)
            time.sleep(0.03)
        screenshot.monitor_command(monitor, "sendkey ret")

        # The turn is over when the harness prints its accounting, which it
        # does down every path it can end by.
        deadline = time.monotonic() + task.seconds
        while time.monotonic() < deadline:
            if os.path.exists(serial):
                with open(serial, errors="replace") as handle:
                    if TURN_LINE.search(handle.read()):
                        break
            if guest.poll() is not None:
                break
            time.sleep(1.0)

        # Shut the machine down rather than killing it, because this test
        # reads the disk afterwards and SIGKILL to QEMU is a power cut. A
        # save that had reached the guest's filesystem but not the image was
        # being read back as a save that never happened, and every
        # file-writing task failed while every reply-checking one passed.
        #
        # Ctrl-Alt-Del is the gesture PID 1 already answers: SIGINT, then the
        # orderly teardown that unmounts /state. QEMU carries -no-reboot, so
        # the reboot at the end of that is an exit. ACPI powerdown was tried
        # first and this guest does not handle it: nothing in the userland
        # listens for a power button.
        screenshot.monitor_command(monitor, "sendkey ctrl-alt-delete")
        for _ in range(200):
            if guest.poll() is not None:
                break
            time.sleep(0.1)
    finally:
        guest.kill()
        guest.wait()

    log = ""
    if os.path.exists(serial):
        with open(serial, errors="replace") as handle:
            log = handle.read()
    outcome = Outcome(log, state)
    if keep:
        print("    kept %s" % work)
    return outcome, work


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--runs", type=int, default=3, help="attempts per task")
    parser.add_argument("--backend", default="local",
                        help="which configuration from awproto::turn::BACKENDS answers; "
                             "a hosted one needs a key in state.img")
    parser.add_argument("--only", action="append", default=[],
                        choices=[task.name for task in TASKS])
    parser.add_argument("--json", default=None, help="write the raw results here")
    parser.add_argument("--keep", action="store_true",
                        help="keep each run's state image and serial log")
    args = parser.parse_args()

    if not os.path.exists(STATE_IMG):
        print("no state.img; run `make run` once so the machine has a disk", file=sys.stderr)
        return 1
    if shutil.which("debugfs") is None:
        print("debugfs is not installed; file assertions need e2fsprogs", file=sys.stderr)
        return 1

    tasks = [task for task in TASKS if not args.only or task.name in args.only]
    results = []
    print(f"backend: {args.backend}, {args.runs} run(s) per task")
    for task in tasks:
        print(f"\n{task.name}: {task.why}", flush=True)
        for attempt in range(args.runs):
            outcome, work = run_once(task, args.backend, args.keep)
            passed = False
            if outcome.finished:
                try:
                    passed = bool(task.check(outcome))
                except Exception as err:                      # a check must never
                    print(f"    check raised: {err}")         # take the suite down
            metrics = outcome.metrics
            mark = "pass" if passed else ("FAIL" if outcome.finished else "NO TURN")
            print(f"  {attempt + 1}/{args.runs} {mark:8} "
                  + (f"{metrics['wall'] / 1000:6.1f}s  "
                     f"{metrics['exchanges']:3d} exchanges  "
                     f"{metrics['actions']:3d} actions  "
                     f"out {metrics['output']:5d}" if metrics else "the turn never ended")
                  + (f"  | {outcome.reply[:60]}" if outcome.reply else ""), flush=True)
            results.append({"task": task.name, "passed": passed,
                            "finished": outcome.finished, "reply": outcome.reply,
                            **metrics})
            if not args.keep:
                shutil.rmtree(work, ignore_errors=True)

    print("\n" + "=" * 78)
    print(f"{'task':<16}{'pass':>10}{'median s':>11}{'exchanges':>11}{'actions':>9}{'out tok':>9}")
    print("-" * 78)
    for task in tasks:
        mine = [r for r in results if r["task"] == task.name]
        if not mine:
            continue
        won = [r for r in mine if r["passed"]]
        walls = [r["wall"] / 1000 for r in mine if r.get("wall")]
        med = lambda key: (statistics.median([r[key] for r in mine if key in r])
                           if any(key in r for r in mine) else 0)
        print(f"{task.name:<16}{len(won)}/{len(mine):<8}"
              f"{statistics.median(walls) if walls else 0:>10.1f}"
              f"{med('exchanges'):>11.0f}{med('actions'):>9.0f}{med('output'):>9.0f}")
    print("-" * 78)
    total = [r for r in results if r["passed"]]
    print(f"{'overall':<16}{len(total)}/{len(results)}")

    if args.json:
        with open(args.json, "w") as handle:
            json.dump(results, handle, indent=2)
        print(f"\nwrote {args.json}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
