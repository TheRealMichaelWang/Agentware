# Agentware: Getting On Device, In Order

OnDevice.md is the design: what is being built and why each piece is where it
is. This is the plan: what order to build it in, how to know each step worked,
and what result would mean the design was wrong.

Nothing here restates the design. Where a decision is referenced it lives there.

## How this plan is organized

Four principles, and they decide most of the ordering.

**Every phase ends in something that proves itself.** The house rule already:
the supervisor's self-test, a screenshot, a printed agent view. A phase whose
completion is a matter of opinion is not finished.

**Claude keeps working throughout.** The hosted backend is the control group and
the fallback. At no point is the machine unable to run a turn, which means no
phase is a cliff and any phase can be abandoned without losing the system.

**Measure before optimizing, which means measurement comes first.** There is no
timing instrumentation in the agent today. Every number in the design document is
arithmetic or a single informal observation. Phase 0 exists because the rest of
the plan is unfalsifiable without it.

**De-risk the unknowns early, even out of order.** The bare metal port is the
largest unknown and most of the plan does not depend on it. So it gets a short
spike near the front to size it, and the full port happens later.

## Where this has got to

**Phase 0 is done and Phase 1 came with it.** Phase 1 was pulled forward for a
practical reason: verifying the instrumentation needs a real turn, there is no
API key on this machine, and there is a model on the host. So the backend seam
was built early and the instrumentation was proved against a local model rather
than a hosted one, which is a better first proof anyway.

What exists now, all of it verified by a real turn photographed and logged:

* Per-exchange and per-turn accounting in `awagent`, printed to the kernel log,
  whose parts add up to the whole within two milliseconds.
* `printk.devkmsg=on` on every boot path, because without it the log silently
  drops the lines the accounting is made of.
* Connection reuse, a cache breakpoint on the conversation, and the prompt
  fixes.
* `flight_time`: the cursor's travel is proportional to its distance now.
* `backends/openai.rs`, and a Local entry in `turn::BACKENDS` that a machine
  with no API key starts on.
* `tools/modelbench.py`, the host-side model measurement of Phase 2b.
* `type` in `tools/screenshot.py`, without which no capture can drive a turn.

The measurements are in OnDevice.md. The two that changed the plan: prefill
rather than generation is the dominant cost of an exchange, and batching is
worth more than any other free change, cutting a real turn from 44.5s to 25.4s
and the gap between actions from 5.01s to 0.24s.

**What this settles about the goal.** The design asks for the first action
within a second and later actions paced by the cursor. Inside a batch that is
now true: the gap is 0.24s and it is the compositor. Across a batch boundary it
is not close, because a model exchange is 4.6s and nothing in the harness can
make a 27B dense model on 256 GB/s faster than that. A p95 under a second needs
every exchange under a second, which needs roughly ten times this prefill rate
and three times this generation rate. That is a different model, not a better
harness, which is why the sparse comparison is now the first item of Phase 3
rather than one of several.

## Phase 0: Instrumentation, and the wins that need no decisions

Everything here pays off immediately against Claude and remains true afterwards.
None of it depends on any other phase.

### 0a. Turn instrumentation

The thing that unblocks everything else. The harness currently logs what it did
and never how long anything took.

Record, per turn, to `/dev/kmsg` and as one summary line at the end:

* per exchange: wall clock, time to first token, input and output token counts,
  and whatever the backend reports about cache hits
* per tool call: the time between sending an intent and `done` coming back,
  which isolates the compositor's animation from everything else
* per turn: total wall clock, number of exchanges, number of actions, total
  tokens in and out

**Proves itself when:** a real turn produces a line that accounts for its own
duration, and the parts add up to the whole.

### 0b. The free harness fixes

Each is small and independent. In rough order of expected effect:

* Reuse the HTTPS connection. `http.rs:32` calls `connect(host)` inside the
  request function, so every exchange pays a fresh TCP and TLS handshake.
* Put a `cache_control` breakpoint on the conversation, not only on the system
  block. Within a turn each exchange is a strict prefix extension of the last,
  ten deep in a busy turn, and none of it is currently cached.
* Delete the sentence in the `read_app` tool description telling the model to
  read again after acting. The system prompt already tells it the opposite, and
  every time it obeys the description it spends a whole exchange on nothing.
* Say in the system prompt that several tool calls may be issued in one message.
  The loop and the compositor both already handle it; nothing tells the model.
* Poll immediately in `open_app` before sleeping. `OPENING_POLL` currently costs
  300ms even when the application is already up.
* Fix the stale module documentation in `awproto/src/agent.rs`, which still
  describes `query rows` and a section about reading tables a window at a time.
  Both went with the spreadsheet work.

**Proves itself when:** the same scripted turn, run before and after with 0a in
place, shows the exchange count down and the per-exchange overhead down. Needs
an API key in the state image, which `make configure_anthropic_key` writes.

### 0c. Distance-proportional cursor flight

`FLIGHT` is 600ms regardless of distance, so a cursor already on its target
spends 600ms travelling nowhere. Make the duration proportional to distance with
a floor and the current value as the cap.

Leave `KEYSTROKE` alone for now. It is not the bottleneck until inference is
local, and changing it early means tuning against the wrong constraint.

**Proves itself when:** two consecutive actions on the same control cost
noticeably less than 1.2 seconds of flight between them, and a mid-gesture
capture still reads as a cursor travelling rather than teleporting.

## Phase 1: The backend seam

One deliverable: `awagent` can talk to anything that speaks the OpenAI
chat-completions shape with tools, and the first thing it talks to is a real
model on real silicon.

### 1a. The OpenAI-compatible backend

A second implementation of `Backend`, beside `backends/claude.rs`. The trait's
whole contract is one streamed exchange, so this is a sibling rather than a
refactor. Streaming, tool calls, and whatever the endpoint reports about usage
and cache hits so that 0a has something to record.

### 1b. Point it at the host

llama-server runs on the Ubuntu side, where the iGPU already works. The guest
reaches it at `10.0.2.2` over the NIC that is already there for Anthropic.

This is the trick the whole plan leans on: **the entire inference architecture
can be built and measured in QEMU against real GPU-backed inference, with no
bare metal work at all.** What it does not exercise is the service living in
PID 1's table with its descriptors handed over at fork, which is a small amount
of work added in Phase 5.

### 1c. The Local entry

`turn::BACKENDS` stops being a compile-time table. One entry named Local,
offered only when a model is configured, defaulting on a machine with no API
key. The pane's working line names the actual model.

**Phase 1 proves itself when:** a turn runs end to end against a local model,
does something correct on screen, and 0a prints its timings.

## Phase 1.5: The bare metal spike

Two to three days, deliberately timeboxed, and the deliverable is a decision
rather than a working machine.

Boot the existing Agentware image on this Framework Desktop from a **USB stick**,
leaving the internal NVMe and its Ubuntu installation untouched. Get as far as
the supervisor printing its boot report. Nothing else.

The one insight that makes this tractable: **use the UEFI framebuffer for early
output.** `efifb` or `simpledrm` gives kernel messages and a console on the
screen before amdgpu is anywhere near loaded, which removes the chicken-and-egg
problem of needing graphics to debug graphics on a machine with no serial port.

What this answers:

* Does a kernel built from Ubuntu's config, stripped and with the needed pieces
  built in, boot this hardware at all?
* Does the initramfs with amdgpu firmware in it work, and how large does it get?
* Does NVMe come up and does the supervisor find and mount its volumes?

**Proves itself when:** the supervisor's boot report appears on the physical
screen. Not the compositor. Just the report.

**What the answer changes:** if this takes two days, Phase 5 is scheduled with
confidence. If it is still fighting after a week, the bare metal port becomes a
project of its own and everything else proceeds against the host server for
longer than planned.

## Phase 2: The evaluation harness

This is the phase most likely to be skipped and least advisable to skip. Every
decision after it is a comparison, and comparisons need a fixture.

### 2a. A repeatable agent task suite

A set of tasks with **programmatically checkable outcomes**, run headless. The
existing `tools/screenshot.py` already boots the guest, injects input and
captures state; this extends that machinery rather than inventing new.

Each task is a prompt plus an assertion:

* "Add 12 and 34 on the calculator" then assert the display reads 46.
* "Open quarter.csv and put 500 in D4" then assert `query cells` returns 500.
* "Rename welcome.txt to readme.txt" then assert the file dialog listing changed.
* "Sum column B of the sheet into B12" then assert the value.
* A task that requires answering a dialog, since `blocked` is a rejection the
  model has to reason about.
* A task that requires opening an application that is not yet open.

The assertions go through the agent surface itself, because `query view` and
`query cells` are already a machine-readable account of what is on screen. That
is a payoff of the architecture worth using.

The model is nondeterministic, so each task runs N times and the result is a
pass rate, not a boolean.

### 2b. Model microbenchmarks, host side

Separate from the guest entirely, because they measure the model rather than
Agentware. Fixed prompts of known token counts, straight at llama-server:

* prefill tokens per second at 1k, 8k, 16k, 32k
* generation tokens per second, with and without a grammar
* KV bytes per token, which llama.cpp reports at load
* slot save and restore wall clock for a slot of a few gigabytes
* the same set with KV quantized to q8 and q4

### 2c. What gets reported

One table per candidate configuration: task suite pass rate, median turn wall
clock, median time to first action, exchanges per turn, tokens per turn. That
table is the artifact every later phase argues with.

**Phase 2 proves itself when:** the suite runs unattended and produces that table
for two configurations, Claude and one local model, without anybody watching.

## Phase 3: The measurement campaign

Answering the design's open questions. No new architecture, only experiments and
their conclusions, each of which settles something OnDevice.md currently marks
as unknown.

* **Dense versus sparse: answered.** Qwen3.6-35B-A3B at UD-Q5_K_XL is 2.8x the
  generation, 3.1x the prefill, and takes the same real turn from 25.4s to
  11.9s. It also holds 128k of context for 2.5 GiB more than 32k, which
  retires the context ceiling the design was built around. The numbers and the
  memory trap that came with them are in OnDevice.md. What is **not** answered
  is pass rate: one task run a handful of times is not evidence, and this
  model was seen to invent a `read_cells` call against an application with no
  spreadsheet in it. That is what the Phase 2 suite is for, and it is now the
  blocking item rather than the model.
* **Where quality actually falls off with context.** The 32k figure is one
  person's impression. The suite can measure it: run the same tasks with padded
  history at 8k, 16k, 32k, 48k. **This number sets the slot size, the context
  budget and the history trimming threshold**, which the design says should all
  be the same figure.
* **Thinking on versus off.** Half answered: it is the largest token lever and
  it is now off on the local path, because it also has the worst draft
  acceptance (0.52 against 0.97 for a tool call) and so loses twice. What is
  not answered is what it costs in correctness, which needs the task suite.
* **KV quantization.** q8 and q4 against pass rate. This buys slot count.
* **Does slot save and restore work with Vulkan and quantized KV**, and how fast.
  The three-tier cache design rests on a restore being a second or two.
* **Does llama.cpp reserve KV up front or lazily.** Decides whether the memory
  floor is a guarantee or needs a different mechanism.
* **Does llama.cpp build against musl with the Vulkan loader.** The only open
  question that changes the shape of the design rather than a number in it.

**Proves itself when:** every open question in OnDevice.md is either answered or
explicitly reclassified as not mattering.

## Phase 4: Making the local path fast

Now, and not before, because everything here is tuning and Phase 3 says what to
tune against.

* **Grammar-constrained tool calls.** GBNF generated from the same closed action
  vocabulary the tool schema is generated from, so the two cannot drift. Watch
  for forced-token fast-forwarding actually engaging; if it does not, most of the
  benefit is missing.
* **Prompt-lookup speculation**, which needs no draft model and suits output that
  is mostly ids copied out of the context. A draft model only if lookup
  underperforms.
* **The thinking budget** as a setting rather than a constant.
* **History trimming at the turn boundary**, in the agentdesk, to the threshold
  Phase 3 established. This is the only cut the design makes into context, and
  the reasoning for cutting there and nowhere else is in OnDevice.md.
* **A terser tool-call format**, only if the numbers justify it and only with the
  grammar in place. Measure pass rate carefully: models are trained on JSON tool
  calling and this is the change most likely to cost reliability.

**Proves itself when:** the Phase 2 table shows the local configuration meeting
the design's stated targets, or shows exactly which one it misses.

## Phase 5: Bare metal, properly

Informed by the 1.5 spike, and sequenced so each step has a visible result.

1. **Kernel config.** Start from Ubuntu's `/boot/config-*`, strip to what is
   actually bound on this machine, and build in what must exist before any
   filesystem is mounted. The initramfs has no module loader, which is the
   constraint that decides most of this.
2. **Firmware in the initramfs.** The `gc_11_5`, `psp_14`, `smu_14`, `dcn_3_5`,
   `vcn_4_0_5` and `sdma_6_1` sets, one variant of each. Measured at 3MB
   compressed for every variant, so the real figure is smaller.
3. **Boot path.** An ESP with the kernel and initramfs, EFI stub, cmdline. Keep
   Ubuntu bootable throughout; there is no reason for this to be destructive.
4. **Storage and partition layout.** NVMe via `agentware.system=` and
   `agentware.state=`, which already exist. The state volume stops being a 64MB
   image: models are tens of gigabytes and the cache budget is two hundred, so
   decide here whether settings, models and caches share a partition or are
   separated by lifetime.
5. **Display.** amdgpu modesetting, real EDID, real connectors. The compositor
   talks DRM/KMS, which should survive, but this is where the single-output
   question gets answered.
6. **Input.** USB and i2c HID behind the existing evdev abstraction.
7. **Network.** A DHCP client in userland, since the kernel will no longer
   configure the interface from an `ip=` argument. A small service; PID 1 stays
   dependency-free.

**Proves itself when:** `tools/screenshot.py`'s scenarios pass on the physical
machine, and the Phase 2 suite runs on it.

**Also measure here:** compositor paint time at the panel's native resolution.
The design withdrew the claim that software rasterization starves the model of
bandwidth, but 7 to 8ms per frame at 1440p scales with pixels and this is where
that becomes a real number rather than an estimate.

## Phase 6: awinference as a service

The inference server moves into the guest and becomes a citizen.

1. **The service table entry.** Started by PID 1, readiness gated, restart policy,
   clean shutdown, descriptors handed to `awagent` and `agentdesk` at fork.
2. **Model packages.** `/models/<name>/`, scanned at runtime, on the state volume.
   Model selection in the Settings app, with the eviction and load made visible
   and a rule for turns in flight.
3. **Slot management.** A slot is an agentdesk. The memory floor first, slot count
   derived, pinning for the desk on screen and for running turns.
4. **The disk tier.** Save and restore, the two hundred gigabyte budget, LRU, and
   deletion when an agentdesk closes. Cleared at boot, because conversations do
   not outlive one.
5. **Prefetch on switch.** The agentdesk begins loading its cache when it is
   switched to, and when its selector changes to Local. Cancellable, asynchronous.

Whether this is llama-server behind an OpenAI backend or a Rust service linking
libllama is decided by Phase 3's musl answer and by whether the HTTP and JSON
overhead shows up in Phase 0's instrumentation. Both are viable; the design
argues for the second eventually.

**Proves itself when:** switching between two agentdesks with long conversations
costs nothing visible, and the machine still has its ten gigabyte floor with
every slot full.

## Phase 7: Voice

Last, because it needs the microphone and it benefits from everything else being
fast.

1. **Audio in the guest**, emulated first so the architecture can be proven
   before the real ALC623 is touched.
2. **`awvoice`**: VAD always on, wake word, streaming recognition on the NPU.
3. **Wake word routing**: "Agentware" to a new agentdesk, "Agentdesk" to the one
   on screen, and nothing else addressable.
4. **The compositor side**: words typed into the destination visibly, through the
   same text box every other keystroke uses, submitted at end of speech.
5. **TTS**, optional and off by default, which requires deciding how narration is
   told from the reply earlier than the harness currently does.

**Proves itself when:** "Agentware, add twelve and thirty four on the calculator"
produces the same outcome the Phase 2 task suite checks for when typed.

## Testing, across all of it

**What already exists and must keep passing:** `make selftest`, the unit tests in
both workspaces, clippy silent, and the screenshot scenarios. Nothing in this
plan is allowed to regress them.

**What gets built here:**

* The **task suite** (2a), which is the only thing that can tell a fast model
  from a good one. Run it on every configuration change.
* The **turn instrumentation** (0a), which is the only thing that can tell where
  a second went.
* **Host-side microbenchmarks** (2b), which measure the model rather than the
  system and can run without a guest at all.

**What each kind of change needs before it lands:**

| Change | Verification |
| --- | --- |
| Harness or protocol | unit tests, task suite pass rate unchanged |
| Model or decoding config | task suite pass rate, plus the timing table |
| Compositor animation | mid-gesture capture, plus pixel diff where something should look unchanged |
| Context or history policy | task suite with padded history at several lengths |
| Bare metal | the screenshot scenarios on the physical machine |

The pixel diff rule is worth restating because it has been earned twice: when two
implementations are being collapsed into one, or when something should look
unchanged, the diff is the oracle and a screenshot with an opinion is not.

## What would mean the design is wrong

Written down now, while it is cheap to be honest.

* **A local model cannot pass the task suite at a rate close to Claude's.** Then
  the choice is a better model, a fine-tune, or accepting that Local is for
  simple work and hosted is for the rest. The architecture survives either way,
  because the selector already exists.
* **Prefill is slow enough that the disk tier does not help.** If restoring a
  slot is not meaningfully faster than rebuilding it, the three-tier design
  collapses into two and the slot count has to carry everything.
* **llama.cpp reserves KV lazily, or not at all as assumed.** Then the memory
  floor needs a different mechanism, and the design's central promise about the
  cache always yielding needs rewriting rather than restating.
* **Software rasterization cannot hold a frame at native resolution.** Then the
  compositor needs GPU compositing, which is a change of shape rather than of
  constants, and it is a project rather than a phase.
* **The bare metal port does not converge.** Then this remains a QEMU system that
  talks to a host model, which is a worse outcome but not a dead one, and every
  other phase still stands.

## What this plan does not include

Fine-tuning a model on Agentware's own view format, which is the largest
available win and needs training data, which needs logged turns, which is a
decision with its own weight. Worth revisiting after Phase 3 says how far a
general model gets.

Multi-output display handling, which the single-output question in Phase 5 may
turn into work.

Anything about suspend, thermals or power management, which a machine that is
plugged in can defer.
