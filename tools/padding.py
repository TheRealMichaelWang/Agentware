#!/usr/bin/env python3
"""The earlier conversation a padded-history run begins with.

For measuring where a model's quality falls off with context, the task
suite puts a long conversation in front of the prompt. What that
conversation is made of decides what the measurement measures, and the
first attempt got it wrong: three hundred copies of one action report
taught the model to answer a new request with a report of work it had not
done, and the curve that came out was a curve of that, not of context
length (CLAUDE.md, gotchas).

So the conversation is fixed text, written by hand, in `fixtures/`: the
kind of afternoon a person might actually spend with this machine. Varied
requests across every application it has, corrections, questions about
what is open and what changed, small arithmetic, files made and moved and
deleted, and the agent's replies in the register of the real ones from the
kept runs: short, factual, naming what was done and where. Every reply is
true of the machine it describes. The parts are read in order; a size past
the end of them starts over from the beginning, and the log says so,
because a conversation that repeats itself is a weaker measurement and the
reader of the results should know at which size that began.

Sized in tokens against the model server's own tokenizer when one is
reachable, so a level is the number it says it is, and by a calibrated
characters-per-token figure otherwise. Cut at a turn boundary, never inside
one, so the history always ends with the agent's reply and the prompt that
follows is the human's next request.

    tools/padding.py --tokens 32768 > pad.txt
    tools/padding.py --tokens 32768 --url http://127.0.0.1:8080 --check

The agent reads the file it is pointed at (`agentware.pad-history=`) and
knows nothing else about it: awagent/src/padding.rs.
"""

import argparse
import glob
import json
import os
import re
import sys
import urllib.request

FIXTURES = os.path.join(os.path.dirname(os.path.abspath(__file__)), "fixtures")

# Characters per token of the fixture text against Qwen3.6's tokenizer,
# measured through /tokenize on 12 September 2026 (conversation-1: 14,565
# characters, 4,391 tokens). Used only when no server is reachable.
CHARS_PER_TOKEN = 3.32


def turns():
    """Every (asked, answered) pair in the fixture files, in order."""
    pairs = []
    for path in sorted(glob.glob(os.path.join(FIXTURES, "conversation-*.txt"))):
        asked = None
        with open(path) as handle:
            for line in handle:
                line = line.rstrip("\n")
                if line.startswith("#"):
                    continue
                if line.startswith("H: "):
                    asked = line[3:]
                elif line.startswith("A: ") and asked is not None:
                    pairs.append([asked, line[3:]])
                    asked = None
                elif line.startswith("  ") and pairs and asked is None:
                    # A continuation of the agent's entry: the actions it
                    # took stand above its reply, as in the desk's history.
                    pairs[-1][1] += "\n" + line[2:]
    return [tuple(pair) for pair in pairs]


# The line the agentdesk puts between a reply and the calls that earned it
# (awproto::turn::HISTORY_CALLS), reproduced here because the fixture is
# rendered in the desk's own shape.
HISTORY_CALLS = "\x1ecalls"

ACTION_PATTERNS = [
    (re.compile(r'^reading what is open$'), lambda m: ("list_apps", {})),
    (re.compile(r'^opening (\S+)$'), lambda m: ("open_app", {"name": m.group(1)})),
    (re.compile(r'^looking for an app for "(.*)"$'), lambda m: ("search_apps", {"query": m.group(1)})),
    (re.compile(r'^reading (\S+) of (\S+) in (\S+)$'),
     lambda m: ("read_cells", {"instance": m.group(3), "id": m.group(2), "range": m.group(1)})),
    (re.compile(r'^reading (\S+)$'), lambda m: ("read_app", {"instance": m.group(1)})),
    (re.compile(r'^(\S+) "(.*)" into (\S+) in (\S+): .*$'),
     lambda m: ("act", {"instance": m.group(4), "action": m.group(1), "target": m.group(3),
                        "value": m.group(2).replace("\\n", "\n")})),
    (re.compile(r'^(\S+) (\S+) in (\S+): .*$'),
     lambda m: ("act", {"instance": m.group(3), "action": m.group(1), "target": m.group(2)})),
]


def call_from(line):
    """The tool call an action line of the fixture stands for, as the agent
    would have reported it, or None for a line that is not one (the header,
    an elision)."""
    for pattern, build in ACTION_PATTERNS:
        found = pattern.match(line)
        if found:
            name, arguments = build(found)
            return json.dumps({"name": name, "input": arguments})
    return None


def entry(answered):
    """An agent's fixture entry in the desk's shape: the reply, then the
    calls. The fixture is written as the pane shows a turn (an actions
    header, one action per line, then the reply); what the model is given
    is the reply and the calls as tool-use blocks, which is what the desk
    keeps and the agent rebuilds (awagent/src/history.rs)."""
    lines = answered.split("\n")
    calls, reply = [], []
    for line in lines:
        if line.startswith("Actions this turn"):
            continue
        if line.startswith("- "):
            call = call_from(line[2:])
            if call:
                calls.append(call)
            continue
        reply.append(line)
    text = " ".join(part for part in reply if part).strip()
    if calls:
        return text + "\n" + HISTORY_CALLS + "\n" + "\n".join(calls)
    return text


def render(pairs):
    """The file the agent reads: `H: ` and `A: ` lines, a message each, with
    a message's further lines indented two spaces, exactly what
    awagent/src/padding.rs reads back."""
    out = []
    for asked, answered in pairs:
        out.append("H: %s\n" % asked.replace("\n", " "))
        first, *rest = entry(answered).split("\n")
        out.append("A: %s\n" % first)
        out.extend("  %s\n" % line for line in rest)
    return "".join(out)


def post(url, path, body):
    request = urllib.request.Request(
        url.rstrip("/") + path,
        data=json.dumps(body).encode(),
        headers={"content-type": "application/json"},
    )
    with urllib.request.urlopen(request, timeout=300) as response:
        return json.load(response)


def as_messages(pairs):
    """The conversation as the agent hands it to the model: the desk's
    entries expanded the way awagent/src/history.rs expands them, a tool
    call per call and a placeholder result each, in the OpenAI shape the
    local backend sends."""
    messages = []
    n = 0
    for asked, answered in pairs:
        messages.append({"role": "user", "content": asked})
        text = entry(answered)
        if "\n" + HISTORY_CALLS + "\n" in text:
            reply, trailer = text.split("\n" + HISTORY_CALLS + "\n", 1)
            calls = [json.loads(line) for line in trailer.split("\n") if line.strip()]
            messages.append({"role": "assistant", "content": None, "tool_calls": [
                {"id": "earlier-%d" % (n + i), "type": "function",
                 "function": {"name": call["name"], "arguments": json.dumps(call["input"])}}
                for i, call in enumerate(calls)]})
            for i in range(len(calls)):
                messages.append({"role": "tool", "tool_call_id": "earlier-%d" % (n + i),
                                 "content": "done (the result is not kept in the conversation's history)"})
            n += len(calls)
            messages.append({"role": "assistant", "content": reply})
        else:
            messages.append({"role": "assistant", "content": text})
    return messages


def tokenizer(url):
    """A function counting the tokens a conversation costs the model, with
    the server at `url`, or None.

    Counts what the model receives, not the file: the chat template renders
    every tool call as a block with its own markup, so a history of calls is
    about twice the tokens of its text. The server applies its template
    (`/apply-template`) and counts the result (`/tokenize`), so a size asked
    for is a size the first exchange will show within a few tokens. The
    first version counted the file and a "64k" history arrived as about
    125k, which no turn could finish inside."""
    if not url:
        return None

    def count(pairs):
        if not pairs:
            return 0
        prompt = post(url, "/apply-template", {"messages": as_messages(pairs)})["prompt"]
        return len(post(url, "/tokenize", {"content": prompt})["tokens"])

    try:
        count([("probe", "probe")])
    except (OSError, ValueError, KeyError):
        return None
    return count


def conversation(tokens, url=None, log=lambda message: None):
    """About this many tokens of conversation, as (text, turns, wrapped).

    `wrapped` is how many times the fixture was started over to reach the
    size: zero for a history that never repeats itself.
    """
    pairs = turns()
    if not pairs:
        raise SystemExit("no fixture turns under %s" % FIXTURES)
    count = tokenizer(url)
    if count is None:
        log("no tokenizer at %s; sizing by %.2f characters per token of the rendered text"
            % (url, CHARS_PER_TOKEN))

        def count(pairs):
            return int(len(render(pairs)) / CHARS_PER_TOKEN)

    chosen, wrapped, i = [], 0, 0
    # Grow in steps and measure the whole, so the cut lands within one turn
    # of the target; a per-turn count would be thousands of requests.
    step = 16
    while True:
        chunk = []
        for _ in range(step):
            if i >= len(pairs):
                i, wrapped = 0, wrapped + 1
            chunk.append(pairs[i])
            i += 1
        if count(chosen + chunk) > tokens:
            # Add turns one at a time up to the line.
            for pair in chunk:
                if count(chosen + [pair]) > tokens:
                    break
                chosen.append(pair)
            break
        chosen.extend(chunk)
    if wrapped:
        log("the fixture (%d turns) was started over %d time(s) to reach %d tokens"
            % (len(pairs), wrapped, tokens))
    return render(chosen), len(chosen), wrapped


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--tokens", type=int, required=True)
    parser.add_argument("--url", default="http://127.0.0.1:8080",
                        help="a llama-server whose tokenizer sizes the text")
    parser.add_argument("--check", action="store_true",
                        help="report the size and the turn count instead of printing the text")
    args = parser.parse_args()
    text, count, wrapped = conversation(args.tokens, args.url,
                                        log=lambda m: print(m, file=sys.stderr))
    if args.check:
        measured = tokenizer(args.url)
        pairs = turns()
        print("%d turns, %d characters, %s tokens as the model receives them, fixture wrapped %d time(s)" % (
            count, len(text), measured(pairs[:count]) if measured and count <= len(pairs) else "?", wrapped))
    else:
        sys.stdout.write(text)
    return 0


if __name__ == "__main__":
    sys.exit(main())
