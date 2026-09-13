#!/usr/bin/env python3
"""Measure a model behind an OpenAI-compatible endpoint, as an agent uses it.

This is deliberately not a token-per-second benchmark. A generic benchmark
answers "how fast is this model", and the question that actually decides
whether the local path is usable is narrower: **how long does one agentic
exchange take**, where an exchange is a growing conversation prefilled again
and a short tool call generated. Prefill dominates when the conversation is
long, generation dominates when the model thinks, and the two are traded
against each other by settings, so they are measured separately and together.

It talks to the endpoint the way `awagent` does, over the same route with the
same tool definitions, so a number here is a number the harness could see.
Nothing about Agentware is imported: it runs on the host, against a server on
the host, with no guest at all.

    tools/modelbench.py                       # every case, default endpoint
    tools/modelbench.py --only prefill        # one group
    tools/modelbench.py --url http://127.0.0.1:8080
"""

import argparse
import json
import statistics
import sys
import time
import urllib.error
import urllib.request

# The four tools the harness offers, verbatim in shape. What matters for the
# measurement is that the schema is the same size and the same closed
# vocabulary, because the tool definitions are part of every prefill.
TOOLS = [
    {
        "type": "function",
        "function": {
            "name": "list_apps",
            "description": "List the applications open in this workspace, as markup naming "
                           "each app. Call this first, and again after open_app.",
            "parameters": {"type": "object", "properties": {}},
        },
    },
    {
        "type": "function",
        "function": {
            "name": "read_app",
            "description": "Read one open application's interface as reduced semantic "
                           "markup: every control's id, description, state, and the actions "
                           "it currently accepts.",
            "parameters": {
                "type": "object",
                "properties": {"app": {"type": "string", "description": "The application's name"}},
                "required": ["app"],
            },
        },
    },
    {
        "type": "function",
        "function": {
            "name": "read_cells",
            "description": "Read part of a spreadsheet, as one line per row with values "
                           "separated by tabs.",
            "parameters": {
                "type": "object",
                "properties": {
                    "app": {"type": "string"},
                    "id": {"type": "string"},
                    "range": {"type": "string", "description": "A rectangle like A1:D20"},
                },
                "required": ["app", "id", "range"],
            },
        },
    },
    {
        "type": "function",
        "function": {
            "name": "act",
            "description": "Perform one action on one control, by id. The compositor brings "
                           "the app's window to the front itself, moves the visible cursor "
                           "to the control, and performs the action as a human would.",
            "parameters": {
                "type": "object",
                "properties": {
                    "app": {"type": "string"},
                    "action": {
                        "type": "string",
                        "enum": ["focus", "click", "type-text", "clear", "submit", "check",
                                 "uncheck", "toggle", "select", "select-range", "deselect",
                                 "set-value", "open", "close", "move"],
                    },
                    "target": {"type": "string"},
                    "value": {"type": "string"},
                },
                "required": ["app", "action", "target"],
            },
        },
    },
]

SYSTEM = (
    "You are the agent of an Agentware agentdesk: an AI-native operating system where you "
    "and the human share one workspace and use the same applications through the same "
    "interface. You act for the human, visibly: everything you do is performed on their "
    "screen with a cursor they can watch.\n\n"
    "read_app returns an application's interface as reduced semantic markup. Every control "
    "carries an id, a description of what it does, its state, and an actions attribute "
    "listing what it accepts right now. The actions attribute is the authority. A disabled "
    "control lists none. A control marked blocked is behind an open dialog.\n\n"
    "act names an application, a control id, and one action from a closed vocabulary. The "
    "compositor stages every action itself: the target's window comes to the front and its "
    "siblings are put away before your action lands. You cannot move, resize or arrange "
    "windows, and never need to.\n\n"
    "Read before acting: list_apps, then read_app, then act. You may issue several tool "
    "calls in one message, and should whenever the next step does not depend on the "
    "previous one's result. Your final message, with no tool call, ends the turn."
)

# A real agent view, as the compositor produces one. Used as a tool result so
# the prefill measured is prefill of the thing the harness actually sends.
CALC_VIEW = """<app name="awcalc">
<window title="Calculator">
  <text description="The running total">0</text>
  <row>
    <button id="seven" description="Digit 7" actions="click"/>
    <button id="eight" description="Digit 8" actions="click"/>
    <button id="nine" description="Digit 9" actions="click"/>
    <button id="divide" description="Divide" actions="click"/>
  </row>
  <row>
    <button id="four" description="Digit 4" actions="click"/>
    <button id="five" description="Digit 5" actions="click"/>
    <button id="six" description="Digit 6" actions="click"/>
    <button id="times" description="Multiply" actions="click"/>
  </row>
  <row>
    <button id="one" description="Digit 1" actions="click"/>
    <button id="two" description="Digit 2" actions="click"/>
    <button id="three" description="Digit 3" actions="click"/>
    <button id="minus" description="Subtract" actions="click"/>
  </row>
  <row>
    <button id="zero" description="Digit 0" actions="click"/>
    <button id="point" description="Decimal point" actions="click"/>
    <button id="equals" description="Compute the result" actions="click"/>
    <button id="plus" description="Add" actions="click"/>
  </row>
  <button id="clear" description="Clear the display" actions="click"/>
</window>
</app>"""


class Endpoint:
    def __init__(self, url, model, timeout=600, greedy=False):
        self.url = url.rstrip("/")
        self.model = model
        self.timeout = timeout
        # Greedy sampling makes a comparison between two server configurations
        # mean something. At temperature 0.7 the model answers one prompt with
        # 50 tokens and the next with 300, and a tokens-per-second figure read
        # off two different answers compares two different pieces of work.
        self.greedy = greedy

    def chat(self, messages, tools=None, max_tokens=512, think=None, grammar=None, extra=None):
        """One completion. Returns (reply, timings, wall_clock_seconds).

        `extra` is merged into the request body, for the one-off knobs a
        group wants to compare (`tool_choice`, say) without every caller
        learning a parameter for each.
        """
        body = {
            "model": self.model,
            "messages": messages,
            "max_tokens": max_tokens,
            "temperature": 0.0 if self.greedy else 0.7,
            "top_p": 1.0 if self.greedy else 0.8,
            "top_k": 1 if self.greedy else 20,
            "stream": False,
            "cache_prompt": True,
        }
        if tools:
            body["tools"] = tools
        # Two different knobs, and only one of them turns thinking off. This
        # template refuses reasoning_effort "none" with a Jinja error and
        # ignores the server's own reasoning_budget entirely; enable_thinking
        # is what it reads. Worth knowing before believing a "thinking off"
        # measurement that was quietly still thinking.
        if think == "off":
            body["chat_template_kwargs"] = {"enable_thinking": False}
        elif think is not None:
            body["chat_template_kwargs"] = {"reasoning_effort": think}
        if grammar:
            body["grammar"] = grammar
        if extra:
            body.update(extra)

        request = urllib.request.Request(
            f"{self.url}/v1/chat/completions",
            data=json.dumps(body).encode(),
            headers={"content-type": "application/json"},
            method="POST",
        )
        started = time.monotonic()
        with urllib.request.urlopen(request, timeout=self.timeout) as response:
            payload = json.load(response)
        wall = time.monotonic() - started
        return payload, payload.get("timings", {}), wall

    def props(self):
        with urllib.request.urlopen(f"{self.url}/props", timeout=10) as response:
            return json.load(response)


def filler(tokens):
    """Roughly `tokens` tokens of plausible conversation.

    Padding with repeated text would be prefilled but is also exactly what a
    cache is best at, so this varies every line: the point is to make the
    model prefill something, not to find out how fast it can skip.
    """
    lines = []
    count = 0
    n = 0
    while count < tokens:
        n += 1
        line = (f"The human asked about item {n} in the report, and the agent read the "
                f"spreadsheet, found row {n * 7 % 991}, and reported the value "
                f"{n * 37 % 1013} back with a note about column {chr(65 + n % 26)}.")
        lines.append(line)
        count += len(line) // 4
    return "\n".join(lines)


def report(name, timings, wall, extra=""):
    prefill = timings.get("prompt_n", 0)
    prefill_ms = timings.get("prompt_ms", 0.0)
    gen = timings.get("predicted_n", 0)
    gen_ms = timings.get("predicted_ms", 0.0)
    prefill_rate = (prefill / prefill_ms * 1000) if prefill_ms else 0
    gen_rate = (gen / gen_ms * 1000) if gen_ms else 0
    # Speculation, when the server is doing any: how many tokens the draft
    # offered and how many the model kept. The ratio is the whole story of
    # whether a draft head or an n-gram lookup is paying its way.
    drafted = timings.get("draft_n", 0) or 0
    accepted = timings.get("draft_n_accepted", 0) or 0
    draft = f" | draft {accepted}/{drafted}" if drafted else ""
    print(f"  {name:<34} wall {wall:6.2f}s | "
          f"prefill {prefill:6d} tok @ {prefill_rate:7.1f} tok/s ({prefill_ms/1000:5.2f}s) | "
          f"gen {gen:5d} tok @ {gen_rate:5.1f} tok/s ({gen_ms/1000:5.2f}s){draft}{extra}")
    return {
        "name": name, "wall": wall,
        "prefill_tokens": prefill, "prefill_s": prefill_ms / 1000, "prefill_rate": prefill_rate,
        "gen_tokens": gen, "gen_s": gen_ms / 1000, "gen_rate": gen_rate,
        "drafted": drafted, "accepted": accepted,
    }


def group_prefill(api, results):
    """How prefill scales with the conversation, which is what grows in a turn."""
    print("\nPREFILL, cold (no cached prefix): what a first exchange pays")
    for target in (1000, 4000, 8000, 16000, 32000):
        messages = [
            {"role": "system", "content": SYSTEM + "\n\n" + filler(target)},
            {"role": "user", "content": "Reply with the single word: ready."},
        ]
        # A fresh prefix every time, so nothing is served from the slot.
        _, timings, wall = api.chat(messages, max_tokens=8, think="off")
        results.append(report(f"prefill ~{target} tok", timings, wall))


def group_cached(api, results):
    """The same conversation twice: the second is what caching is worth."""
    print("\nCACHED PREFIX: the second exchange of a turn, and what it saves")
    base = [
        {"role": "system", "content": SYSTEM + "\n\n" + filler(8000)},
        {"role": "user", "content": "Reply with the single word: ready."},
    ]
    _, timings, wall = api.chat(base, max_tokens=8, think="off")
    results.append(report("8k cold", timings, wall))

    # Extend it the way a turn does: the assistant's answer plus a tool result.
    grown = base + [
        {"role": "assistant", "content": "ready"},
        {"role": "user", "content": "Here is the calculator:\n" + CALC_VIEW +
                                    "\nReply with the single word: seen."},
    ]
    _, timings, wall = api.chat(grown, max_tokens=8, think="off")
    results.append(report("8k warm + one view appended", timings, wall))


def group_exchange(api, results):
    """The measurement that decides everything: one agentic exchange."""
    print("\nAGENTIC EXCHANGE: read a view, emit a tool call. The real unit of work")
    for effort, label in (("off", "thinking off"), ("low", "thinking low"), ("medium", "thinking medium")):
        messages = [
            {"role": "system", "content": SYSTEM},
            {"role": "user", "content": "Add 12 and 34 on the calculator."},
            {"role": "assistant", "content": None,
             "tool_calls": [{"id": "c1", "type": "function",
                             "function": {"name": "read_app", "arguments": '{"app":"awcalc"}'}}]},
            {"role": "tool", "tool_call_id": "c1", "content": CALC_VIEW},
        ]
        try:
            payload, timings, wall = api.chat(messages, tools=TOOLS, max_tokens=2048, think=effort)
        except urllib.error.HTTPError as err:
            print(f"  {label:<34} refused: {err.read().decode()[:120]}")
            continue
        message = payload["choices"][0]["message"]
        calls = message.get("tool_calls") or []
        names = ",".join(c["function"]["name"] for c in calls) or "none"
        got = f" | {len(calls)} call(s): {names}"
        results.append(report(f"exchange, {label}", timings, wall, got))
        if calls:
            print(f"  {'':<34} -> {calls[0]['function']['arguments'][:100]}")


def group_batching(api, results):
    """Whether the model will issue several calls at once when told it may.

    This is the single largest lever on interaction latency: four buttons in
    one message costs one exchange, four buttons in four messages costs four.
    """
    print("\nBATCHING: will it emit several calls in one message when it can?")
    messages = [
        {"role": "system", "content": SYSTEM},
        {"role": "user", "content": "Type 12, then plus, then 34, then equals on the calculator. "
                                    "The buttons do not depend on each other's results."},
        {"role": "assistant", "content": None,
         "tool_calls": [{"id": "c1", "type": "function",
                         "function": {"name": "read_app", "arguments": '{"app":"awcalc"}'}}]},
        {"role": "tool", "tool_call_id": "c1", "content": CALC_VIEW},
    ]
    for effort in ("off", "medium"):
        payload, timings, wall = api.chat(messages, tools=TOOLS, max_tokens=2048, think=effort)
        calls = payload["choices"][0]["message"].get("tool_calls") or []
        detail = f" | {len(calls)} call(s) in one message"
        results.append(report(f"batch, thinking {effort}", timings, wall, detail))


def group_grammar(api, results):
    """What the server's tool-call grammar costs, and what it buys.

    llama.cpp builds a grammar from the tool schemas on every request that
    carries `tools`. For this model's template (the Qwen3-Coder XML shape)
    it is lazy: nothing is constrained until `<tool_call>` appears, then the
    function name, the parameter names and every argument are held to the
    schema, the `act` action to its enum included. So the closed vocabulary
    the harness sends already reaches the sampler; nothing has to be
    generated in GBNF by hand. What the server does not do is jump forward:
    a token the grammar leaves no choice about is still sampled one step at
    a time, so a grammar cannot make a tool call faster, only certain.

    Three shapes of one exchange, greedy, same content in each: the tools
    as the harness sends them (lazy grammar), the same with `tool_choice`
    required (the grammar from the first token), and the tools described
    in the prompt with no `tools` field at all (no grammar, and the model
    writes the XML from memory).
    """
    print("\nGRAMMAR: the same tool call with the server's grammar lazy, from the first token, and absent")
    prompt = ("Add 12 and 34 on the calculator. It is open as awcalc#1 and this is its "
              "interface:\n\n" + CALC_VIEW)
    described = SYSTEM + (
        "\n\nThe tools, as JSON schemas:\n" + json.dumps(TOOLS) +
        "\n\nCall a tool by writing, on its own lines, "
        "<tool_call>\\n<function=NAME>\\n<parameter=ARGUMENT>\\nVALUE\\n</parameter>\\n"
        "</function>\\n</tool_call>, one block per call."
    )
    shapes = (
        ("tools, lazy grammar", SYSTEM, TOOLS, {}),
        ("tools, grammar from token one", SYSTEM, TOOLS, {"tool_choice": "required"}),
        ("tools in the prompt, no grammar", described, None, {}),
    )
    for label, system, tools, extra in shapes:
        rates = []
        for round_number in range(3):
            messages = [{"role": "system", "content": system},
                        {"role": "user", "content": prompt}]
            try:
                payload, timings, wall = api.chat(messages, tools=tools, max_tokens=1024,
                                                  think="off", extra=extra)
            except urllib.error.HTTPError as err:
                print(f"  {label:<34} refused: {err.read().decode()[:120]}")
                break
            message = payload["choices"][0]["message"]
            calls = message.get("tool_calls") or []
            text = message.get("content") or ""
            # Without the grammar the call is text; count the blocks the
            # parser would have found, so the three shapes report one thing.
            found = len(calls) or text.count("<tool_call>")
            row = report(f"{label} {round_number + 1}", timings, wall, f" | {found} call(s)")
            results.append(row)
            rates.append(row["gen_rate"])
        if rates:
            print(f"  {'':<34} median gen {statistics.median(rates):.1f} tok/s")


def group_repeat(api, results, rounds):
    """The same exchange several times, for a spread rather than one number."""
    print(f"\nSPREAD: the same exchange {rounds} times, thinking off")
    messages = [
        {"role": "system", "content": SYSTEM},
        {"role": "user", "content": "Add 12 and 34 on the calculator."},
        {"role": "assistant", "content": None,
         "tool_calls": [{"id": "c1", "type": "function",
                         "function": {"name": "read_app", "arguments": '{"app":"awcalc"}'}}]},
        {"role": "tool", "tool_call_id": "c1", "content": CALC_VIEW},
    ]
    walls = []
    for round_number in range(rounds):
        payload, timings, wall = api.chat(messages, tools=TOOLS, max_tokens=1024, think="off")
        calls = payload["choices"][0]["message"].get("tool_calls") or []
        walls.append(wall)
        results.append(report(f"repeat {round_number + 1}", timings, wall, f" | {len(calls)} call(s)"))
    if walls:
        walls.sort()
        p95 = walls[min(len(walls) - 1, int(len(walls) * 0.95))]
        print(f"  {'':<34} median {statistics.median(walls):.2f}s, "
              f"p95 {p95:.2f}s, min {walls[0]:.2f}s, max {walls[-1]:.2f}s")


GROUPS = {
    "prefill": group_prefill,
    "cached": group_cached,
    "exchange": group_exchange,
    "batching": group_batching,
    "grammar": group_grammar,
}


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--url", default="http://127.0.0.1:8080")
    parser.add_argument("--model", default="local")
    parser.add_argument("--only", action="append", default=[], choices=list(GROUPS) + ["repeat"])
    parser.add_argument("--rounds", type=int, default=5, help="repetitions for the spread group")
    parser.add_argument("--greedy", action="store_true",
                        help="temperature 0, so two configurations can be compared")
    parser.add_argument("--json", default=None, help="write the raw results here")
    args = parser.parse_args()

    api = Endpoint(args.url, args.model, greedy=args.greedy)
    try:
        props = api.props()
    except (urllib.error.URLError, OSError) as err:
        print(f"no server at {args.url}: {err}", file=sys.stderr)
        return 1

    model = props.get("model_path", "?").split("/")[-1]
    context = props.get("default_generation_settings", {}).get("n_ctx", "?")
    print(f"model:   {model}")
    print(f"context: {context}")
    print(f"url:     {args.url}")

    results = []
    wanted = args.only or list(GROUPS) + ["repeat"]
    for name in wanted:
        if name == "repeat":
            group_repeat(api, results, args.rounds)
        else:
            GROUPS[name](api, results)

    if args.json:
        with open(args.json, "w") as handle:
            json.dump({"model": model, "context": context, "results": results}, handle, indent=2)
        print(f"\nwrote {args.json}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
