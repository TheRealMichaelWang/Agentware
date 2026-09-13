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
import csv
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
import setkey      # noqa: E402  editing settings.xml inside an image, likewise
import padding     # noqa: E402  the earlier conversation a padded run begins with

# Where the padded conversation goes inside a run's state image, and where
# the guest therefore finds it: /state is the state volume mounted.
PAD_FILE = "pad.txt"
PAD_PATH = "/state/" + PAD_FILE
# The local model server, whose tokenizer sizes the padding.
SERVER_URL = "http://127.0.0.1:8080"

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
# The first exchange's own line: what the model was handed before it had
# done anything, which under a padded history is the size that was actually
# sent rather than the size that was asked for.
FIRST_EXCHANGE = re.compile(
    r"awagent: exchange 1: \d+ms, [^;]*; in (\d+) \(cache read (\d+), write \d+\)"
)


class Outcome:
    """What one run of one task left behind."""

    def __init__(self, log, state):
        self.log = log
        self.state = state
        self.metrics = {}
        self.reply = ""
        self._files = {}

        first = {}
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
            found = FIRST_EXCHANGE.search(line)
            if found and not first:
                first = {"first_input": int(found.group(1)),
                         "first_cached": int(found.group(2))}
        # Only a turn that ended has metrics at all; `finished` reads them.
        if self.metrics:
            self.metrics.update(first)

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

    def logged(self, pattern: str) -> bool:
        """Whether the serial log carries a line matching this.

        The applications write what they did to the kernel log, and that is
        the one witness a reply cannot forge: `awcalc: display 46` means the
        calculator showed 46, whatever the model then said about it.

        Matched a line at a time, and a serial console ends its lines in
        CRLF, so a pattern anchored with `$` must allow the carriage return:
        write `display 46\\s*$`.
        """
        return re.search(pattern, self.log, re.MULTILINE) is not None

    def cell(self, path: str, ref: str) -> str:
        """One cell of a saved CSV by its `D4` name, or "" if it is not there.

        Quoted cells are read as a spreadsheet writes them; a sheet with no
        quoting in it reads the same either way.
        """
        column = ord(ref[0].upper()) - ord("A")
        row = int(ref[1:]) - 1
        lines = self.file(path).splitlines()
        if row >= len(lines):
            return ""
        cells = next(csv.reader([lines[row]]))
        return cells[column].strip() if column < len(cells) else ""

    def listing(self, directory: str) -> list:
        """The names in a directory of the state volume, plain files only."""
        got = subprocess.run(
            ["debugfs", "-R", "ls -p %s" % directory, self.state],
            capture_output=True, text=True,
        )
        return self._entries(directory, "100")

    def folders(self, directory: str) -> list:
        """The names of the folders in a directory of the volume."""
        return [name for name in self._entries(directory, "40") if name not in (".", "..")]

    def _entries(self, directory, mode):
        got = subprocess.run(
            ["debugfs", "-R", "ls -p %s" % directory, self.state],
            capture_output=True, text=True,
        )
        names = []
        for line in got.stdout.splitlines():
            # /inode/mode/uid/gid/name/size/ ; a regular file's mode starts
            # 100, a directory's 40. debugfs prints the mode in octal with a
            # leading zero on a directory (040755) and none on a file
            # (100644), which is how a check for "40" matched no folder at
            # all and a list meant to hold 13 files held 3.
            parts = line.split("/")
            if len(parts) >= 6 and parts[2].lstrip("0").startswith(mode):
                names.append(parts[5])
        return names


class Task:
    def __init__(self, name, prompt, check, why, seconds=180):
        self.name = name
        self.prompt = prompt
        self.check = check
        # What the assertion is actually asking, for the report. A pass rate
        # nobody can interpret is a number, not a result.
        self.why = why
        self.seconds = seconds


# What /home holds before any run, so a task that has to create a file is
# judged on the files it created and not on one that was already there.
PRISTINE_HOME = None

# Every text file the machine ships, as (folder, name): the three under /home
# and each installed application's name and description. Read out of the two
# images at startup rather than written down here, so an application added
# to the machine is one more file the traverse task has to find.
MACHINE_TEXT_FILES = None

SYSTEM_IMG = os.path.join(ROOT, "system.img")


def machine_text_files():
    system = Outcome("", SYSTEM_IMG)
    found = []
    for app in system.folders("/apps"):
        for name in system.listing("/apps/" + app):
            if name.endswith(".txt"):
                found.append((app, name))
    for path in sorted(PRISTINE_HOME):
        if path.endswith(".txt"):
            folder, name = path.rsplit("/", 2)[-2:]
            found.append((folder, name))
    return found


def lists_every_text_file(outcome, path):
    """Whether a saved sheet names every text file on the machine.

    A row counts for a file when it carries both the file's name and its
    folder, because five applications each have a `description.txt` and a
    row saying only that names none of them. This replaced a check for the
    three files under /home alone, which passed a run that had listed 3 of
    the machine's 13 and stopped, and so could not tell a run that had
    traversed the filesystem from one that had looked in one folder.
    """
    rows = outcome.file(path).splitlines()
    return all(any(folder in row and name in row for row in rows)
               for folder, name in MACHINE_TEXT_FILES)


def new_files(outcome):
    """Every file under /home (one level of folders deep) this run created.

    Whatever it is called. The spreadsheet writes CSV under any name, and a
    run that saved a correct sheet as `text_files.txt` was scored a failure
    for its extension, which is not the thing being measured.
    """
    found = []
    for directory in ("/home", "/home/notes"):
        for name in outcome.listing(directory):
            path = directory + "/" + name
            if path not in PRISTINE_HOME:
                found.append(path)
    return found


TASKS = [
    Task(
        name="calculator",
        prompt="Add 12 and 34 on the calculator app.",
        # The calculator's own log is the witness. A reply saying 46 proves
        # the model can add; the calculator logging the sum it computed
        # proves the machine did it, which is the thing being measured.
        why="the calculator computed 12 + 34 = 46 and the reply says so",
        check=lambda got: got.logged(r"awcalc: (12 \+ 34|34 \+ 12) = 46\s*$")
        and got.said("46"),
        seconds=120,
    ),
    Task(
        name="open-and-edit",
        prompt="Open quarter.csv in the spreadsheet and put 500 into cell D4, then save it.",
        # The cell, not the file: "500 somewhere in it" would pass a sheet
        # overwritten with nothing but 500, and the rest of the file still
        # being there is what says it was edited rather than replaced.
        why="quarter.csv has 500 in D4 and the rest of the sheet intact",
        check=lambda got: got.cell("/home/quarter.csv", "D4") == "500"
        and got.cell("/home/quarter.csv", "A11") == "Overseas",
        seconds=240,
    ),
    Task(
        name="new-sheet",
        prompt="Make a new spreadsheet with the word Hello in cell A1 and save it as "
               "greeting.csv in the home folder.",
        why="greeting.csv exists with Hello in A1",
        check=lambda got: got.cell("/home/greeting.csv", "A1").lower() == "hello",
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
        # The prompt itself contains "12", so the reply saying it proves
        # nothing; the calculator logging the division proves the agent
        # found the application and used it, which is the whole task. The
        # operator is matched loosely because it is a ÷ and a serial console
        # is not to be trusted with one.
        why="the calculator computed 144 / 12 = 12, the agent having found it itself",
        check=lambda got: got.logged(r"awcalc: 144 \S+ 12 = 12\s*$") and got.said("12"),
        seconds=120,
    ),
    Task(
        name="traverse",
        # The long-horizon one. Two applications, a directory walk that has to
        # descend and come back, and a result carried between them. This is
        # the task that found three real defects the first time it was run.
        # "Save it" is said, because the check reads a file and a sheet that
        # was filled in correctly and never saved is a run that did what it
        # was asked. Where to save it is said too, so the check knows where
        # to look; what to call it is not, so every file the run created is
        # read.
        prompt="Create a spreadsheet listing all the text files on this machine, and "
               "save it in the home folder. Traverse the filesystem manually with the "
               "file explorer; there is no search.",
        why="a file the run saved under /home names every text file on the machine, "
            "each with its folder",
        check=lambda got: any(lists_every_text_file(got, path) for path in new_files(got)),
        seconds=600,
    ),
]


def run_once(task, backend, keep, settings=(), extra="", pad=None):
    """Boot, send the prompt, wait for the turn to end, and read what is left.

    `settings` are `path=value` pairs written into the run's own copy of
    settings.xml before it boots, so a machine can be measured in one
    configuration per run without anyone clicking through the Settings app;
    `extra` is added to the kernel command line; `pad` is the text of an
    earlier conversation, written into the image and named on the command
    line, so the turn begins with that much history behind the prompt.
    """
    work = tempfile.mkdtemp(prefix="tasksuite-")
    state = os.path.join(work, "state.img")
    # Its own copy of the machine's disk, so a task that writes files is
    # checked against what it actually wrote and no run can see another's
    # leavings.
    shutil.copyfile(STATE_IMG, state)
    # The machine's image carries whatever its journal held at the last
    # shutdown. Replay it before anything is written with debugfs, or the
    # fsck after the write replays it then and puts the old inode table
    # back over the new file: the directory entry stays and points at
    # inode 0, and the guest says the file does not exist. A padded run
    # measured with no padding at all before this line existed.
    setkey._repair(state)
    if settings:
        text = setkey.read_settings(state) or setkey.defaults_with("")
        for pair in settings:
            path, _, value = pair.partition("=")
            text = setkey.set_element(text, path, value)
        setkey.write_settings(state, text)
    if pad:
        local = os.path.join(work, PAD_FILE)
        with open(local, "w") as handle:
            handle.write(pad)
        setkey.debugfs(state, "write %s %s" % (local, PAD_FILE), writable=True)
        setkey._repair(state)
        extra = ("%s agentware.pad-history=%s" % (extra, PAD_PATH)).strip()
    serial = os.path.join(work, "serial.log")
    monitor = os.path.join(work, "monitor.sock")
    qmp = os.path.join(work, "qmp.sock")

    # The backend is named rather than implied. It used to be decided by
    # whether an API key was set, which is right for a person at the machine
    # and useless for a measurement: the moment a key exists for testing the
    # hosted model, every local run silently becomes a hosted one.
    append = ("console=ttyS0,115200 printk.devkmsg=on"
              " ip=10.0.2.15::10.0.2.2:255.255.255.0:agentware:eth0:off"
              f" agentware.backend={backend} {extra}").strip()
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
    parser.add_argument("--setting", action="append", default=[], metavar="PATH=VALUE",
                        help="a settings.xml element for every run, e.g. "
                             "agent/local-thinking=on")
    parser.add_argument("--append", default="", metavar="ARGS",
                        help="added to the kernel command line")
    parser.add_argument("--pad", type=int, default=0, metavar="TOKENS",
                        help="begin every turn with this much earlier conversation "
                             "(tools/padding.py), sized by the model server's tokenizer")
    args = parser.parse_args()

    if not os.path.exists(STATE_IMG):
        print("no state.img; run `make run` once so the machine has a disk", file=sys.stderr)
        return 1
    if shutil.which("debugfs") is None:
        print("debugfs is not installed; file assertions need e2fsprogs", file=sys.stderr)
        return 1

    # What the disk holds before any task runs, read once from the machine's
    # own image rather than assumed: a run is judged on the files it made.
    global PRISTINE_HOME
    pristine = Outcome("", STATE_IMG)
    PRISTINE_HOME = {directory + "/" + name
                     for directory in ("/home", "/home/notes")
                     for name in pristine.listing(directory)}
    global MACHINE_TEXT_FILES
    MACHINE_TEXT_FILES = machine_text_files()
    if not MACHINE_TEXT_FILES:
        print("no text files found in the images; is system.img built?", file=sys.stderr)
        return 1

    tasks = [task for task in TASKS if not args.only or task.name in args.only]
    results = []
    # The padding is the same text for every run, made once: the point is
    # that every run at one size begins with the same history.
    pad, pad_turns, pad_wrapped = None, 0, 0
    if args.pad:
        pad, pad_turns, pad_wrapped = padding.conversation(args.pad, SERVER_URL, log=print)
    print(f"backend: {args.backend}, {args.runs} run(s) per task"
          + (f", settings {' '.join(args.setting)}" if args.setting else "")
          + (f", kernel args {args.append}" if args.append else "")
          + (f", padded with {pad_turns} turns of conversation (about {args.pad} tokens, "
             f"fixture wrapped {pad_wrapped} time(s))" if pad else ""))
    for task in tasks:
        print(f"\n{task.name}: {task.why}", flush=True)
        for attempt in range(args.runs):
            outcome, work = run_once(task, args.backend, args.keep, args.setting, args.append,
                                     pad)
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
                  + (f"  first in {metrics['first_input']}" if "first_input" in metrics else "")
                  + (f"  | {outcome.reply[:60]}" if outcome.reply else ""), flush=True)
            # Each row says what it was measured under, so a results file
            # read months later does not need the command line that made it.
            results.append({"task": task.name, "passed": passed,
                            "finished": outcome.finished, "reply": outcome.reply,
                            "backend": args.backend, "settings": args.setting,
                            "kernel_args": args.append, "pad_tokens": args.pad,
                            "pad_turns": pad_turns, "pad_wrapped": pad_wrapped, **metrics})
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
