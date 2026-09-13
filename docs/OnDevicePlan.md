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

**Prepared 13 September 2026, awaiting the boot.** What was needed, and
what it found out before any reboot:

* The machine is Strix Halo (Radeon 8060S, PCI `1002:1586`), booted by
  UEFI with **Secure Boot on**, which will refuse an unsigned kernel; it has
  to be turned off in the firmware setup for the stick to boot at all. Its
  GPU blocks are gfx 11.0, psp 13.0, smu 14.0, sdma 6.0, vcn 4.0.5 and dcn
  3.5, which is not the firmware list this section guessed; amdgpu is not in
  the spike's kernel and the list is for Phase 5.
* The QEMU kernel config lacked everything a screen needs before a GPU
  driver: no framebuffer, no `efifb`, no framebuffer console, no NVMe, no
  UAS. The USB kernel is a git worktree of the submodule (`kernel-usb-src/`,
  ignored) with `SYSFB`, `FB_EFI`, `FRAMEBUFFER_CONSOLE`, `BLK_DEV_NVME`,
  `USB_UAS` and a built-in command line, so the EFI stub boots it with no
  loader: `initrd=/initramfs.cpio.gz console=tty0 agentware.system=/dev/sda2
  agentware.state=/dev/sda3 agentware.volume-wait=15000`. The stub takes
  forward slashes in the initrd path; backslashes do not survive Kconfig.
* The supervisor waited 500ms for a volume, which is right for virtio and
  wrong for a USB stick that takes seconds to enumerate, so
  `agentware.volume-wait=<ms>` lengthens it on the command line and QEMU
  keeps its half second.
* `tools/usbstick.sh` lays the stick out without root, through udisks: a
  FAT32 ESP with the kernel as `EFI/BOOT/BOOTX64.EFI` and the initramfs, an
  ext4 system volume holding the sysroot tree, an empty ext4 state volume.
  The first boot is expected to reach the boot report, mount both volumes,
  and then fail to start the compositor for want of a DRM device, which is
  the spike's stopping point.

**Attempt 1, 13 September: the kernel booted and the supervisor aborted
itself 300 microseconds in.** The photograph showed the EFI framebuffer,
USB and HID up, the initramfs unpacked, "Run /init as init process", then
a general protection fault in init at a `hlt` and "Attempted to kill init!
exitcode=0xb". The `hlt` is the one in musl's `abort()`. The cause was the
command line, not the hardware: it ended `console=ttyS0`, the last
`console=` is what `/dev/console` becomes, PID 1's stderr is `/dev/console`,
and this machine has no serial port. The supervisor's first log line goes
through `eprintln!` before `/dev/kmsg` exists; `eprintln!` panics when the
write fails; the panic hook reported the panic through the same stderr; a
panic inside a panic is an abort. Reproduced exactly in QEMU under OVMF with
`-serial none` (same fault, same offset in the binary), which is how the
spike will be tested from now on: the stick's layout as a loop image, the
real firmware path, and the screen photographed through the monitor. Fixed
twice over: the supervisor writes its pre-kmsg lines and its panic message
with writes that cannot fail into a panic (`klog::stderr`), and the built-in
command line now ends `console=tty0`. Under QEMU with no serial port the
fixed initramfs boots to the report, mounts the system and state volumes off
the USB disk, and starts the compositor, which exits for want of a display
and is restarted with backoff, on the old command line and the new. That
restart loop would scroll the report off a console with no scrollback
before a phone could be pointed at it, so the spike's command line also
carries `agentware.report-only`: the supervisor mounts the volumes, prints
the report, and parks, with Ctrl-Alt-Del handed back to the kernel. Under
QEMU with no serial port the screen then holds: virtual filesystems, "DRM:
no /dev/dri nodes", two evdev nodes, the USB disk found as sda1 to sda3,
`/dev/sda2` mounted on `/system` and bound in, `/dev/sda3` on `/state`,
`/home` off it, the resolver written, "stopping here, as asked". The stick
carries that kernel and initramfs, verified byte for byte. Attempt 2 is
expected to show the same screen on the Framework.

**Attempt 2, 13 September: it worked.** The Framework Desktop booted the
stick to the supervisor's boot report and the line
`agentware.report-only: stopping here, as asked`, and Ctrl-Alt-Del rebooted
it back into Ubuntu. Two attempts, one day, the failure between them a
console bug rather than a hardware one. What the spike set out to answer:

* A kernel built from this tree's config plus the pieces above boots this
  hardware, through the EFI stub with no loader, with the firmware's
  framebuffer as its console.
* The initramfs works unchanged: the supervisor and `/dev/console`, 259 KB.
  amdgpu firmware was not in it and not needed for this; its size is a
  Phase 5 question with a known answer (a few MB).
* The supervisor found and mounted its volumes off the USB stick; NVMe was
  built in but not exercised, since nothing of ours is on the internal
  drives yet.

**What the answer changes:** Phase 5 is scheduled with confidence. The
port converged in a day, and the remaining work is what the Phase 5 list
already says: amdgpu with its firmware, the real display and input, the
one-drive layout, a DHCP client.

**What the answer changes:** if this takes two days, Phase 5 is scheduled with
confidence. If it is still fighting after a week, the bare metal port becomes a
project of its own and everything else proceeds against the host server for
longer than planned.

## Phase 2: The evaluation harness

This is the phase most likely to be skipped and least advisable to skip. Every
decision after it is a comparison, and comparisons need a fixture.

### 2a. A repeatable agent task suite. Built: `tools/tasksuite.py`

The assertions turned out not to need what this section originally proposed.
Reaching `query view` from outside the guest needs a process inside it, and
two things already leaving the machine answer most of what a task wants
asserted:

* **The files it wrote**, read back out of the state volume with `debugfs`,
  the no-sudo trick `make configure_anthropic_key` already uses. Each run gets
  its own copy of `state.img`, so a task that saves a spreadsheet is checked
  against what it actually saved rather than against the agent's account of
  it.
* **The serial log**, carrying the telemetry, the reply, and the `turn:`
  accounting line. That line is the end-of-turn marker a headless run needs,
  and it is the reason this was writable without touching the guest at all.
  Phase 0a paid for itself twice.

One guest change was needed and it was one line: the reply was the only thing
the agent said that was not copied to the log, which left a headless run able
to see that a turn had ended but not what it concluded.

`query view` assertions remain the answer for anything the files and the reply
cannot settle, and are not built because nothing yet needs them.

### The tasks

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
* **Where quality actually falls off with context: it was falling off the
  history's shape, not its length, and the shape is fixed.** Measured 12
  September 2026 with the 35B MoE, six tasks, three runs each, the suite's
  strict checks, no history 17/18. Padding the history with a hand-written
  conversation of the kind a person has with this machine (varied requests,
  the agent's replies in the register of the real ones, none of the suite's
  tasks in it: `tools/fixtures/`, sized by the model's tokenizer through
  `tools/padding.py`) gave **0 of 15 short-task runs at 8k tokens**, every
  one a single exchange with no tool call and a report of work not done,
  copied in shape from the nearest earlier turn. The cause is what the
  agentdesk kept as history: the human's text and the agent's reply, with
  every tool call of every turn dropped at the turn boundary as "detritus".
  Forty turns of that show a model no tool call anywhere, and it concludes
  that in this conversation requests are answered with a report. Listing
  the actions as text above each reply made it write that text itself, 0
  of 3 again. Carrying the calls as what they are, tool-use blocks with
  their arguments and a placeholder result (`agentdesk` keeps them,
  `awagent/src/history.rs` rebuilds them), gave **11 of 12 short-task runs
  at 8k**, the miss a genuine one, fifteen real actions that lost a dialog.
  No history stayed at 17/18. The length question is now searched from 32k
  upward on that history (`search.sh`); the figure below is what that
  found. Two earlier curves, one from three hundred copies of a single
  action report and one from an answers-only conversation, are kept in the
  results directory for what they are: bounds, not the answer.
  What did not change: a long turn grows about 58k tokens above its
  history, so the slot stays 128k and the history budget must leave a turn
  that room; and tool calls are dearer than their prose, 44 turns being 8k
  tokens as text and 18k once the chat template renders the calls.
* **Thinking on versus off: answered, off, and it is a setting now.** The
  suite on 12 September 2026, same server and same day for both, thinking
  off 17/18 and thinking on 17/18. Thinking bought no pass. It cost two to
  three times the output tokens on every short task (calculator 387 against
  1106; write-text 522 against 2328), doubled their wall clock (open-and-edit
  22s against 47 to 62s), and its one miss was new in kind: the model
  reasoned in circles for the whole 8192-token output budget and never
  issued a tool call. On the long task it did help, cutting `traverse` to
  35 to 54 exchanges against 39 to 54 and 144s against 155s on the best
  runs, which is the one place a plan is worth thinking about. It is
  `<agent><local-thinking>` in `settings.xml`, a checkbox on the Settings
  app's Agent page, off by default, read by the agent at each turn. What
  this model's template offers is on or off; the server's budget knob is
  ignored by it, so there is no number to set.
* **KV quantization: answered, and not worth it for the MoE.** q8 scored
  17/18 against f16's 18/18 (the miss unrelated to precision), saves 1.2 GiB
  per 128k slot because hybrid attention keeps the KV small anyway, and
  costs 25% of prefill speed on Vulkan. Stay at f16. q4 not measured; there
  is nothing for it to buy.
* **Slot save and restore: answered, it works, at f16 and q8.** 114k tokens
  save in 0.5s and restore in 0.3s against 242s to prefill; a restored
  conversation is reused in full by any request that continues it, which
  every exchange does. An exact repeat of the saved prompt is the one shape
  that is not resumed, because this hybrid model cannot rewind to
  re-evaluate the last token without a checkpoint the save file does not
  carry; that shape never occurs. `tools/slotbench.py`.
* **KV reservation: answered, up front.** 128k pins 2.4 GiB more than 32k
  before any token is sent, so the floor is a guarantee.
* **Does llama.cpp build against musl with the Vulkan loader: yes, and the
  shape it forces is a dynamically linked process.** Measured 12 September
  2026 with a musl cross toolchain (GCC 11.2.1, musl.cc), the Khronos loader
  1.4.362 built with it (window-system support off, and `USE_GAS=OFF`
  because the assembly trampolines need a generated helper the host cannot
  run), and llama.cpp at 058df67 configured as a declared cross build so its
  own two build-time helpers (the shader generator and the UI embedder) are
  built by the host compiler. `llama-server` links, 67 MB, and prints its
  version under musl's dynamic loader. What it cannot be is static: the
  Vulkan loader finds a driver by `dlopen`, and a static musl binary has no
  `dlopen`. So the inference service in Phase 6 is either the image's first
  dynamically linked process, carrying musl's `ld-musl`, `libstdc++`,
  `libgomp`, the loader and a musl-built Mesa RADV ICD, or it links the ICD
  directly and bypasses the loader. Both are work in Phase 5's territory
  (a musl Mesa is the larger half); neither is a change to the design. The
  recipe is `~/llm/musl/build.sh` on the development machine, outside the
  tree.

**Proves itself when:** every open question in OnDevice.md is either answered or
explicitly reclassified as not mattering.

## Phase 4: Making the local path fast

Now, and not before, because everything here is tuning and Phase 3 says what to
tune against.

* **Grammar-constrained tool calls: already there, and worth nothing in
  speed.** llama.cpp builds a grammar from the tool schemas on every request
  that carries `tools`; for this model's template (the Qwen3-Coder XML shape)
  it engages at `<tool_call>` and holds the function name, the parameter
  names and every argument to the schema, the `act` enum included. So the
  closed vocabulary the harness sends already reaches the sampler, and
  there is no GBNF to write. Measured 12 September (`modelbench --only
  grammar`, greedy, MTP draft): lazy grammar 93.2 tok/s, grammar from the
  first token 94.6, no grammar at all 95.2. The server does not jump forward
  over forced tokens, so a grammar cannot make a call faster; it makes it
  well-formed, which it already was. Nothing to do here.
* **Speculation: measured, and the answer is the draft head first, n-gram
  lookup second.** Same exchanges, greedy, tok/s of generation:

  | mode | first exchange | later exchanges | acceptance |
  | --- | --- | --- | --- |
  | none | 57.4 | 57.6 | |
  | MTP draft head | 94.8 | 94.3 | 1.00 |
  | n-gram simple | 71.7 | 72.4 | 0.62 |
  | n-gram mod | 59.1 | 102.7 to 116.4 | 0.74 |
  | MTP + n-gram simple | 88.3 | 89.3 | 0.73 |
  | MTP + n-gram mod | 78.0 | 101.2 to 139 | 0.68 |

  Prompt lookup does exactly what the plan guessed: once a turn's context
  holds a tool call to copy from, `ngram-mod` beats the draft head by a
  fifth; on the first exchange, with nothing to copy, it is no faster than
  no speculation. The draft head is flat at 1.6x whatever the context
  holds. Adding the simple lookup to the draft head made it slightly
  worse; adding `ngram-mod` to it is the best of all on every exchange
  after the first (120 tok/s on the repeated exchange, 139 on a grammar
  shape) and costs a sixth on the first, where the lookup's misses are
  drafts the model has to refuse. A turn is one first exchange and several
  later ones, so `--spec-type draft-mtp,ngram-mod` is the serving
  configuration now. What none of this changes: prefill is still the cost
  of an exchange, and generation at 120 tok/s is 2.5s of a 4s exchange
  only because these are 300-token calls measured greedy.
* **The thinking budget as a setting: done**, as on or off, which is all the
  template takes (Phase 3 above).
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
   separated by lifetime. **One drive**, decided 13 September 2026: the OS
   and the state volume live on the same drive, as partitions if they are
   separate at all; QEMU's two virtio drives are a development convenience
   and not the shape of the install. The USB stick of Phase 1.5 already has
   that shape.
5. **Display.** amdgpu modesetting, real EDID, real connectors. The compositor
   talks DRM/KMS, which should survive, but this is where the single-output
   question gets answered.
6. **Input.** USB and i2c HID behind the existing evdev abstraction.
7. **Network.** A DHCP client in userland, since the kernel will no longer
   configure the interface from an `ip=` argument. A small service; PID 1 stays
   dependency-free.

**Proves itself when:** `tools/screenshot.py`'s scenarios pass on the physical
machine, and the Phase 2 suite runs on it.

**Steps 1, 2 and 3 built, 13 September 2026, awaiting the boot.** What
the stick now carries, and what was learned making it:

* **The firmware is a submodule**, `linux-firmware/`, the official tree at
  kernel.org pinned to its 20260910 release, shallow and checked out sparse
  to the `amdgpu` directory (101 MB of working tree over 792 MB of pack;
  the alternative was Ubuntu's `/lib/firmware`, which is the same files
  with no record of which release). The list this section guessed was
  wrong in every version number. The real one was read off the GPU's own
  IP discovery table (`/sys/class/drm/card1/device/ip_discovery`, no root
  needed): GC 11.5.1, PSP 14.0.1, DCN 3.5.1, VCN 4.0.6 with two instances,
  SDMA 6.1.1, VPE 6.1.1, and mapped to file names by the driver's
  `MODULE_FIRMWARE` lines, since the prefix rules (`psp_14_0_1_toc` is
  served by `psp_v13_0.c`; VCN's second instance wants `vcn_4_0_6_1.bin`)
  are not guessable either. Fourteen files, 3.5 MB unpacked, 1.3 MB in
  the archive. The SMU on an APU needs none (its firmware is in the BIOS)
  and the ISP, the camera pipeline, is 3.8 MB on its own and not built.
* **The initramfs carries it**, at `/lib/firmware/amdgpu/`, because the
  driver is built in and asks for its firmware while it probes, before PID
  1 has mounted anything; `tools/mkinitramfs.py` writes the directory
  records too, since the kernel's unpacker creates nothing it is not told
  to. It is a second archive, `initramfs-usb.cpio.gz`, 1.5 MB against the
  QEMU one's 259 KB, because virtio-gpu wants no firmware and an initramfs
  is unpacked into RAM on every boot. `make usb` builds the image, the
  bare-metal kernel and this archive; `tools/usbstick.sh` copies them.
* **The bare-metal kernel configuration is checked in**,
  `kernel/agentware-usb.config`, and `make kernel-usb` builds it into the
  `kernel-usb-src` worktree the way `make kernel` builds the QEMU one. It
  adds `DRM_AMDGPU` with `DRM_AMD_DC` and `DRM_FBDEV_EMULATION` (the console
  survives the driver taking the screen from `efifb`), and drops i915, AGP,
  the compute stack (`HSA_AMD`), the SI and CIK generations and the ISP.
  The built-in command line no longer carries `agentware.report-only`: the
  boot is expected to reach the desk.
* **amdgpu refuses the compositor's dirty call.** `DRM_IOCTL_MODE_DIRTYFB`
  is what makes virtio-gpu transfer a frame; amdgpu's `amdgpu_dirtyfb`
  answers `ENOSYS` to any caller with a file, meaning every userspace one,
  and the compositor logged "could not present" on every frame it would
  have drawn, into a kernel log with the rate limit off. The card scans
  out of the dumb buffer the CPU wrote, so nothing is lost; `Display`
  now learns from the first `ENOSYS` and stops asking, one log line.
* **Tested the way Phase 1.5 was**: the stick's layout on a loop device
  (`tools/usbstick.sh` takes one; `udisksctl loop-setup -f` needs no root),
  booted under OVMF as a USB disk with no serial port and photographed
  through the monitor (`tools/usbboot.py`, which is that recipe as a tool).
  The desk is up at twelve seconds, off the USB disk, on virtio-vga. What
  that cannot test is the one thing this step is for: QEMU has no Strix
  Halo, so amdgpu's own bring-up, the firmware being found and accepted,
  and the compositor on a real connector are the boot's to answer.

What the boot should show, in order: the firmware's framebuffer as the
console, the kernel's amdgpu lines as it takes the screen over, the
supervisor's report, then the desk. If the desk does not appear, the
compositor's error is on the console and the supervisor restarts it with
backoff, so the line to photograph is the one that repeats.

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
