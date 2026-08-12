# Agentware

An AI-native operating system userland in Rust, booting on a custom Linux kernel
with no legacy graphics or input stack. Agents are first-class users: they read
the interface as a semantic tree and act on it by naming elements, never by
pixel coordinates.

## Read these first

| Document | Covers |
| --- | --- |
| `VISION.md` | What the user experience is meant to be |
| `ARCHITECTURE.md` | What the processes are and how long they live |
| `INTERACTIONS.md` | What the processes say to each other |
| `docs/UIElements.md` | The AWML element catalogue and action vocabulary |

The code carries its reasoning in comments and commit messages. When something
looks arbitrary, the comment usually says why, and it is usually because the
obvious alternative was tried and failed.

## Where things are

```
agentwarecore/          cargo workspace
  awproto/              both wire protocols: the control socket, and display
  supervisor/           PID 1: init, service table, spawn broker
    src/bin/            awtest awstubborn awctl awui: self-test stand-ins
  haimanager/           the compositor: DRM, input, AWML, layout, paint, clients
  awapp/                reference client, standing in for apps and the agentdesk
  awagent/              stand-in per-turn worker: queries, intents, rejections
initramfs/              staged image contents (build output, gitignored)
tools/screenshot.py     boot, inject input, capture the screen as PNG
kernel-build/           Linux submodule
```

## State

**Supervisor: complete.** Mounts, signals, reaper, service table with restart
policy and backoff, readiness gating, control socket, spawn broker, cgroup per
workspace, descriptor handoff, clean shutdown. `make selftest` exercises all of
it and powers the machine off; QEMU exiting on its own is the pass signal.

**haimanager: milestones 1 to 5 done.**

1. DRM/KMS bring-up
2. Software rasterizer, outline fonts via `fontdue`
3. evdev input, both cursors, event-driven loop
4. AWML parser, layout, hit testing, agent view
5. Client protocol, tree diffing, ephemeral state, versioned events
6. Workspace compositing: regions, windows, the navigation bar, input routing
7. The agent surface: scoped queries, intents, the fake cursor, rejections

Milestone 5 in more detail, since the rest builds on it. Clients arrive as
descriptors the supervisor pushes over the control socket, each tagged with the
workspace it belongs to. Each connection holds one tree, its version, and the
ephemeral state applications deliberately do not track: focus, the caret, and
scroll offsets, keyed by node identity so they survive a re-render. A new tree
is diffed against the held one and an identical resend costs no frame. Events go
back stamped with the version of the tree they were generated against.

A field's `value` is the application's and the caret in it is the compositor's,
so typing is applied locally, painted immediately, and sent on. Values sent are
remembered until a tree comes back carrying one, which is what stops a second
keystroke being thrown away by the echo of the first.

Milestone 7 is the other half of that. An agent sends intents, never events, and
the compositor resolves each one: find the application in the agent's own
workspace, find the node, check it is enabled, not scrolled away and not behind
another window, move the fake cursor there so the human sees it, and only then
synthesize the event. A human's click and an agent's intent go through one
`Client::act`, so an application cannot tell them apart and the two paths cannot
drift. `scroll-into-view` is the way out of both ways a node can be unreachable:
it scrolls the container and raises the window.

**Nothing is left of the original plan.** What is missing now is not
compositor work: `startmenu`, a real `agentdesk` that streams conversation to
and from an agent, a real `agent` with a model behind it, and applications.

Nothing else exists yet: no `agentdesk`, no `agent`, no `startmenu`, no real
apps. `awapp` stands in for the first and the last of those, and is the reference
client for the display protocol rather than a product. The supervisor logs and
skips what is not installed rather than crash looping against it, so the system
boots and is useful without them.

## Building and running

```
make selftest    # headless supervisor self-test, exits 0 on success
make run         # boot in a QEMU window
make pack        # build and pack the initramfs without booting
```

`make run` passes `agentware.demo` on the kernel command line. `startmenu`
does not exist, so nothing would otherwise ask the broker for a workspace and
the compositor would come up with no clients at all. The flag substitutes two
stand-ins and nothing else: `awapp desk` as the agentdesk, and `awctl demo` as
the start menu, which asks for one workspace with one `awapp` in it. Everything
between them is the real path. Drop the flag to see the compositor with nothing
attached.

Once it is up, F1 cycles workspaces, standing in for the start menu, and F2
toggles a diagnostic overlay listing every connection and the version it is on.
The taskbar's launcher and the pane's Send button both go through the real
broker, so clicking them forks real processes: Send starts an agent turn, which
is the whole of milestone 7 running against a live screen. The kernel log carries
its intents and their outcomes.

Builds target `x86_64-unknown-linux-musl`. No sudo is needed: `cpio` records a
device node's major/minor from `stat` and never opens it.

## Verifying graphics

**Graphics cannot be checked from a serial log.** `tools/screenshot.py` boots the
guest, optionally injects input through the QEMU monitor, captures the
framebuffer and writes a PNG, which can then be viewed directly.

```
tools/screenshot.py out.png --seconds 8 --append "console=ttyS0,115200 agentware.demo" \
  --do "mouse_move 150 -120" --do "mouse_button 1" --do "mouse_button 0" \
  --do "sendkey h" --do "sendkey shift-l" --do "mouse_move 0 0 -1"
```

The `--append` matters: without `agentware.demo` there is nothing on screen to
photograph. The pointer starts in the middle of the screen and every move is a
delta from where it is now.

Use it. Every rendering bug so far was found this way and none would have been
found any other way: a black screen where every ioctl reported success, four
glyphs silently rendering as capitals, a list label drawn on top of its first
item, content flush against the screen edge.

The kernel log carries the other half. The haimanager prints a client's agent
view when its first tree arrives, so the reduced schema can be read against the
document that produced it, and prints a line per tree after that saying what the
diff found, which is how "the application resent something identical" is told
apart from "the screen is stale".

## Working conventions

**Run things yourself.** Build, boot, screenshot, and check the result. Do not
hand commands back to be run unless they genuinely need a human, such as an
interactive login.

**Verify, do not assume.** Every milestone ends with something that proves
itself: the supervisor's self-test, a screenshot, a printed agent view. If a
claim cannot be demonstrated, say so rather than asserting it.

**Prose style: no emojis, no em dashes.** Plain, direct writing.

**Commit and push freely**, with messages that explain *why* rather than
restating the diff. The bug that cost a debugging cycle belongs in the message.

**Do not raise design questions about components that do not exist.** If it is
not built and not blocking, it is not a question worth asking yet.

**Keep clippy clean.** `cargo clippy --release --target x86_64-unknown-linux-musl
--all-targets` should be silent.

## Settled decisions, not to be reopened

These were argued through and decided. The reasoning is in the documents; this
is the list so it does not get relitigated.

* An agentdesk **is** the agent's workspace. The agent process is a separate,
  per-turn worker that owns nothing durable.
* Agents never restart. A failed agent is a result to report, not a process to
  resurrect.
* Interrupting an agent is `SIGTERM` to a process that owns nothing.
* A message arriving mid-turn is **queued** by the agentdesk, never refused.
* Nothing survives a reboot. No persistent storage layer, by choice.
* The supervisor never sees prompts, conversation, telemetry or markup. It
  creates sockets and steps out of the way.
* Identity is a capability, not a claim: the haimanager knows which workspace a
  connection belongs to because the supervisor said so at handoff.
* The stop button is drawn by the haimanager and routes to the supervisor, so it
  works even if the agentdesk is wedged.
* Applications send the **whole tree** every time; the haimanager diffs it.
* A human's click and an agent's intent end in **one function**. Nothing else may
  synthesize an event, or the two paths drift and the guarantee that an agent can
  only do what a human could have done stops being checkable.
* `scroll-into-view` is the only remedy for an unreachable node, and it covers
  both scrolling and raising. The agent expresses what should be true, never the
  steps.
* The tree version is the **application's own counter**, stamped by it and echoed
  back on every event. Checking one is then a comparison against a number the app
  already holds, not a mapping it has to maintain.
* Focus, the caret, and scroll offsets are the compositor's and never appear in
  the protocol in either direction. They are carried across a re-render by
  matching node identity: the `id` where there is one, position where there is
  not.
* Agents send **intents**, never events. The haimanager resolves, checks
  visibility and enabled state, animates the cursor, then synthesizes the event.
* Element actions are **derived** from type and state, never declared by the
  application.
* Applications choose type and colour. All of it is stripped from the agent's
  view, which is safe because appearance can never carry meaning: descriptions
  are required and actions are derived.

## Gotchas that cost real time

* **virtio-gpu does not scan out what you wrote.** The host keeps its own copy
  and only transfers on an explicit dirty call. Without it the screen stays as
  it was while every ioctl reports success.
* **`rustix::process::waitpid(None, ..)` is `waitpid(0)`**, meaning any child in
  the caller's *process group*. The reaper must use `wait()`, which is
  `waitpid(-1)`. Services call `setsid` and orphans keep their original group,
  so the wrong one silently collects almost nothing.
* **`/dev/kmsg` is rate limited** to about ten messages per five seconds unless
  `printk_devkmsg` is set to `on`. The eleventh line of a boot and everything
  after it vanishes.
* **printk prints levels strictly below `console_loglevel`.** Setting it to 6
  suppresses level-6 messages.
* **QEMU's `mouse_button` wheel bits do not reach a PS/2 guest.** Bit 8 and bit
  16 produce nothing at all. The wheel is the optional third argument to
  `mouse_move`, so `mouse_move 0 0 -1` is one notch down.
* **A large `mouse_move` is truncated by the PS/2 packet format.** Around 400
  pixels still works and 530 does not, and the failure looks exactly like a
  broken hit test: the cursor is drawn where it was asked to be and the click
  lands somewhere else. Split any move over about 250 pixels into two.
* **`/dev/input` is not fully populated at startup.** QEMU's PS/2 mouse appears
  about 300ms after the directory first has entries, so devices must be
  rescanned rather than enumerated once.
