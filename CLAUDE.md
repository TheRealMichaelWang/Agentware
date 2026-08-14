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
  awapp/                stand-in agentdesk: the reference desk connection
  awagent/              stand-in per-turn worker: queries, intents, rejections
agentwareapps/          cargo workspace: first-party applications
  awcalc/               a calculator, the first real application
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
8. Window management, and making an agent's actions visible as they happen
9. Agentdesk tabs, automatic arrangement for agents, and a presentation path
   fast enough to feel like one

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

Milestone 8 was not on the original list. It exists because the result of 1 to 7
was correct and looked like it: square boxes, windows that could not be moved,
and an agent whose actions were visible only in their consequences. The
rasterizer gained antialiased rounded rectangles and soft shadows, windows gained
chrome that can be dragged, minimized, maximized and closed, and every action now
has a visible moment: a control sinks and darkens for a fifth of a second, and
text is typed rather than pasted.

Milestone 7 is the other half of that. An agent sends intents, never events, and
the compositor resolves each one: find the application in the agent's own
workspace, **arrange the stage** (the target's window comes to the front
maximized and the workspace's other windows are minimized, so a covered target
is impossible rather than rejected), find the node, check it is enabled and not
scrolled away, move the fake cursor there so the human sees it, and only then
synthesize the event. `not-visible` now means exactly one thing, scrolled out of
view inside the app, and `scroll-into-view` remains its remedy. The reverse can
never happen: there is no move, resize, raise or arrange in the intent
vocabulary, so arrangement is done *for* an agent, never *by* one. A human's click and an agent's intent go through one
`Client::act`, so an application cannot tell them apart and the two paths cannot
drift.

Milestone 9, like 8, came from using the thing. The nav tabs are agentdesks:
named "Agentdesk N" by default, renamed by clicking the active tab's title
(inline field, same width as the tab, blinking caret, Enter commits, Escape
cancels, clicking away commits), reordered by dragging (the held tab lifts out
as a ghost under the pointer; the row reorders live as midpoints are crossed),
and closed by the ✕ each tab carries, which is a `close-desk` to PID 1. Names
and order are compositor chrome; the agentdesk process is not told its tab's
name any more than an app is told where its window is.

The presentation path was rebuilt after "it feels sluggish" was traced with
numbers rather than guesses (a per-second `frames:` log in the kernel log
reports paint/blit cost while frames are produced):

* Cursors are an **overlay**, not part of the scene: pointer motion restores
  and restamps small patches with partial dirty rectangles instead of
  repainting 2560x1440 in software per twitch.
* Every present is **atomic**: everything for a frame reaches the framebuffer
  before one `DIRTYFB` with all the clips, because every flush is a chance for
  the host to show a half-composed frame, which is what flicker is.
* Presents are **capped near 120Hz** (`PRESENT_MIN`); work arriving faster is
  deferred through the epoll timeout, never dropped.
* A pure window drag repaints only the region the window swept (`drag_damage`),
  and window shadows come from cached corner tiles plus constant-alpha edge
  strips instead of a per-pixel square root. Measured on a six-second drag:
  paint fell from 14ms avg / 34ms worst to 7.1ms avg / 14ms worst.

**Nothing is left of the original plan.** What is missing now is not
compositor work: `startmenu`, a real `agentdesk` that streams conversation to
and from an agent, a real `agent` with a model behind it, and more applications.

First-party applications live in `agentwareapps/`, a separate workspace because
apps are clients of the display protocol, not parts of the system: they link
`awproto` and nothing else. `awcalc`, a pocket calculator, is the first and so
far only one, and doubles as the reference for how an application is written: a
model and a `render`, hand-written stable ids, no diffing, no ephemeral state.
The demo agent turn drives it: 12 + 34, one press at a time, and reads back 46.

No `agentdesk`, no `agent`, no `startmenu` exist yet. `awapp` stands in for the
agentdesk and is the reference client for the desk connection: regions, the
conversation pane, and the broker requests a workspace makes. The supervisor
logs and skips what is not installed rather than crash looping against it, so
the system boots and is useful without them.

## Building and running

```
make selftest    # headless supervisor self-test, exits 0 on success
make run         # boot in a QEMU window
make pack        # build and pack the initramfs without booting
```

The guest display is a custom 2560x1440 monitor QEMU invents, opened fullscreen
with zoom-to-fit so the window fills whatever the host really has. Sizing the
window to the reported host desktop was tried twice and both times read as
"nothing changed", because the host either shrank the window or already matched
it. `tools/hostsize.sh` still reports the host desktop on the boot line, for
diagnosis rather than sizing. Ctrl+Alt+F leaves fullscreen.

The compositor scales its whole interface by one factor set at startup: 960
logical rows whatever the mode, so 1440 rows means 1.5x type and chrome rather
than an emptier desk. Quarter steps; `agentware.scale=1.25` on the kernel
command line overrides it. Every metric in `ui.rs` and `screen.rs` is a logical
size through `ui::sc()`, so a new hardcoded pixel count is a bug.

```
make run DISPLAY_W=3840 DISPLAY_H=2160     # a bigger custom monitor
tools/screenshot.py out.png --width 2560 --height 1440
```

Screenshots at 1600x1000 and below render at scale 1.0, which keeps the older
coordinate-scripted captures valid.

Once it is up, F1 cycles workspaces, standing in for the start menu, F2 toggles a
diagnostic overlay listing every connection and the version it is on, and F3
folds the conversation pane away (animated, 200ms smoothstep; also the grip on
the pane's edge). Windows have a title bar with chevron/brackets/cross controls
(hover chips; close is danger), can be dragged, and resize from the right edge,
bottom edge and corner, with double-arrow cursors over the bands. The pointer
becomes an I-beam over text, and carets blink on a 530ms clock that wakes the
loop only when a caret exists. Scrollbars hug the content edge, drag (thumb or
track-jump), and auto-hide 900ms after the content stops moving; the agent's
`scroll-into-view` lights them the same way.

The agentdesk's `taskbar` region is parked at zero height for now. The strip
duplicated the dock and spent a full-width band doing it. The region stays in
the protocol and the layout path, so nothing breaks when a desk declares it; it
gets no room until there is a design worth giving room to.
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
* Window chrome is the **compositor's**: title bar, shadow, and the three dots.
  They are the only controls that are not AWML, so no application decides
  whether it is closable and no agent sees the button that destroys its window.
* An agent's typed text goes in **one character at a time**, producing one event
  per keystroke. A value set in one step is something no human could produce.
* A human's click and an agent's intent end in **one function**. Nothing else may
  synthesize an event, or the two paths drift and the guarantee that an agent can
  only do what a human could have done stops being checkable.
* `scroll-into-view` is the only remedy for an unreachable node, which since
  automatic arrangement means scrolled out of view. The agent expresses what
  should be true, never the steps.
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
* An agent **never arranges windows** and cannot ask to: acting on an app
  maximizes it and minimizes its siblings first, so covering is a question the
  compositor makes impossible rather than answers. Only the human moves,
  resizes, reorders or closes anything.
* The agentdesk stays an ordinary AWML client and **never sends pixels**. Raw
  pixels belong inside the tree as `image` content when that day comes, never
  as a client's whole surface.
* Applications choose type and colour. All of it is stripped from the agent's
  view, which is safe because appearance can never carry meaning: descriptions
  are required and actions are derived.

A `width` attribute on a control is a compositor-internal sizing hint in
physical pixels (used so the tab rename field keeps its tab's width); it is not
part of the application catalogue and never reaches an agent.

## Gotchas that cost real time

* **virtio-gpu does not scan out what you wrote.** The host keeps its own copy
  and only transfers on an explicit dirty call. Without it the screen stays as
  it was while every ioctl reports success.
* **`rustix::process::waitpid(None, ..)` is `waitpid(0)`**, meaning any child in
  the caller's *process group*. The reaper must use `wait()`, which is
  `waitpid(-1)`. Services call `setsid` and orphans keep their original group,
  so the wrong one silently collects almost nothing.
* **`/dev/kmsg` is rate limited** to about ten messages per five seconds unless
  `printk.devkmsg=on` is on the kernel command line. The eleventh line of a boot
  and everything after it vanishes. The `frames:` timing log needs this flag.
* **`grep -c` exits nonzero on zero matches**, so `cargo build | grep -c error
  && make pack` silently skips the pack and the guest boots the previous image.
  One "verified" perf run measured a binary that did not contain the change.
* **A paced test cannot catch a burst bug.** Input arrives many events per
  repaint; the tab drag worked when a script sent one motion per frame and
  failed at mouse speed, because a reorder tore down the nav layout mid-burst.
  Drive gestures with back-to-back events when testing drags.
* **Two flushes are a flicker.** Any dirty ioctl is a chance for the host to
  present the framebuffer as it stands; erase-then-stamp as separate flushes
  showed cursorless frames. Compose everything, then flush once.
* **The harness proves mechanisms, not feel.** Pixel-diff verification is blind
  to dead travel, missing feedback and latency; capture mid-gesture frames, and
  treat "it works" claims about interaction dynamics as unverified until
  timing numbers or mid-gesture captures exist.
* **printk prints levels strictly below `console_loglevel`.** Setting it to 6
  suppresses level-6 messages.
* **A relative mouse can never align with the host cursor.** The PS/2 mouse
  streams deltas, so the guest integrates its own position and drifts from the
  host pointer the moment the window is scaled. The virtio tablet reports
  absolute positions and the two become one; the compositor's `EV_ABS` handling
  assumes QEMU's fixed 0..32767 range. The PS/2 devices stay for the monitor's
  injected input. In the screenshot tool, `--do "abs 0.5 0.9 click"` drives the
  tablet over QMP; `mouse_move` still drives the PS/2 mouse.
* **QEMU's `mouse_button` wheel bits do not reach a PS/2 guest.** Bit 8 and bit
  16 produce nothing at all. The wheel is the optional third argument to
  `mouse_move`, so `mouse_move 0 0 -1` is one notch down.
* **A large `mouse_move` is truncated by the PS/2 packet format.** Around 400
  pixels still works and 530 does not, and the failure looks exactly like a
  broken hit test: the cursor is drawn where it was asked to be and the click
  lands somewhere else. Split any move over about 250 pixels into two.
* **Detection that falls back silently is detection that never worked.** The
  first host-size probe piped `xrandr` errors to /dev/null; xrandr was not
  installed, so every boot used the fallback and the change appeared to do
  nothing. The probe lives in `tools/hostsize.sh` now, it has a second source
  (WSLg's log), and the boot line prints what the host reported so an empty
  answer is visible.
* **A demonstration that depends on geometry breaks at another resolution.** The
  agent script proved `not-visible` with a draft scrolled off the end of a list,
  which stopped being true the moment the display got bigger and the list fitted.
  There is no list length that works at every size. It now proves it with one
  window covering another, which is true at any size.
* **`/dev/input` is not fully populated at startup.** QEMU's PS/2 mouse appears
  about 300ms after the directory first has entries, so devices must be
  rescanned rather than enumerated once.
