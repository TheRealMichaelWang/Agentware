#!/usr/bin/env python3
"""Score kept task-suite runs again, under the checks as they are now.

A check that turns out to have been too lenient (or wrong) changes what
every earlier run meant, and the runs are still on disk: `--keep` leaves each
one's state image and serial log behind, and the suite's log says which
directory each result line came from. So this reads a suite log, finds each
run's directory, applies the current check for its task, and prints the old
and new verdicts side by side. With `--update` it rewrites the `passed`
field in the results JSON the same log produced, so a table drawn from the
JSON is drawn under one rule.

    tools/recheck.py docs/results/2026-09-12/moe-thinking-on.log
    tools/recheck.py --update docs/results/2026-09-12/*.log

It is also the cross-check that caught two assertion bugs: with `--all`,
every kept run is scored against every task, and a run that passes a task
it was not given is a check that is not checking anything.
"""

import argparse
import json
import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import tasksuite  # noqa: E402

KEPT = re.compile(r"^\s+kept (/\S+)")
RESULT = re.compile(r"^\s+(\d+)/(\d+) (pass|FAIL|NO TURN)")
TASK = re.compile(r"^([a-z-]+): ")


def runs_in(log_path):
    """(task, kept directory, old verdict) for each result line of a log."""
    task, kept, out = None, None, []
    with open(log_path) as handle:
        for line in handle:
            found = TASK.match(line)
            if found and found.group(1) in {t.name for t in tasksuite.TASKS}:
                task = found.group(1)
            found = KEPT.match(line)
            if found:
                kept = found.group(1)
            found = RESULT.match(line)
            if found and task and kept:
                out.append((task, kept, found.group(3) == "pass"))
                kept = None
    return out


def score(task, directory):
    serial = os.path.join(directory, "serial.log")
    state = os.path.join(directory, "state.img")
    if not os.path.exists(serial) or not os.path.exists(state):
        return None
    with open(serial, errors="replace") as handle:
        outcome = tasksuite.Outcome(handle.read(), state)
    if not outcome.finished:
        return False
    try:
        return bool(task.check(outcome))
    except Exception:                                   # a check must not
        return False                                    # take the tool down


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("logs", nargs="+", help="suite logs written with --keep")
    parser.add_argument("--update", action="store_true",
                        help="rewrite `passed` in the JSON beside each log")
    parser.add_argument("--all", action="store_true",
                        help="score every run against every task, for the cross-check")
    args = parser.parse_args()

    pristine = tasksuite.Outcome("", tasksuite.STATE_IMG)
    tasksuite.PRISTINE_HOME = {directory + "/" + name
                               for directory in ("/home", "/home/notes")
                               for name in pristine.listing(directory)}
    tasksuite.MACHINE_TEXT_FILES = tasksuite.machine_text_files()
    tasks = {task.name: task for task in tasksuite.TASKS}

    status = 0
    for log_path in args.logs:
        runs = runs_in(log_path)
        print(f"{log_path}: {len(runs)} kept run(s)")
        verdicts = []
        for name, directory, was in runs:
            if args.all:
                others = [other for other in tasks
                          if other != name and score(tasks[other], directory)]
                extra = f"  ALSO PASSES {', '.join(others)}" if others else ""
            else:
                extra = ""
            now = score(tasks[name], directory)
            if now is None:
                print(f"  {name:<14} {directory}  gone")
                verdicts.append(was)
                continue
            change = "" if now == was else "  CHANGED"
            print(f"  {name:<14} {directory}  was {'pass' if was else 'FAIL':<4} "
                  f"now {'pass' if now else 'FAIL'}{change}{extra}")
            verdicts.append(now)

        json_path = log_path[:-4] + ".json" if log_path.endswith(".log") else None
        if args.update and json_path and os.path.exists(json_path):
            with open(json_path) as handle:
                rows = json.load(handle)
            if len(rows) != len(verdicts):
                print(f"  {json_path}: {len(rows)} rows against {len(verdicts)} runs; not updated")
                status = 1
                continue
            for row, verdict in zip(rows, verdicts):
                row["passed"] = verdict
            with open(json_path, "w") as handle:
                json.dump(rows, handle, indent=2)
            won = sum(1 for row in rows if row["passed"])
            print(f"  updated {json_path}: {won}/{len(rows)}")
    return status


if __name__ == "__main__":
    sys.exit(main())
