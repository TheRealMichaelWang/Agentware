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
  awproto/              the wires: control socket, display, agent, desk-agent turn;
                        and the one file contract, settings
  awkit/                the app toolkit: models that render AWML; the dialogs
  supervisor/           PID 1: init, service table, spawn broker
    src/bin/            awtest awstubborn awctl awui: self-test stand-ins
  haimanager/           the compositor: DRM, input, AWML, layout, paint, clients
    src/startmenu.rs    the start menu panel: prompt and application grid
    assets/             the Agentware mark, compiled in
  agentdesk/            the workspace process: conversation, turns, taskbar clock
  awsettings/           the settings app (a first-party app that edits system state)
  awagent/              stand-in per-turn worker: scripted, speaks both channels
agentwareapps/          cargo workspace: first-party applications
  awcalc/               a calculator, the first real application
  awfiles/              a file explorer, and where the shared dialogs are seen
home/                   sample files, staged to /home (RAM; lost at power off)
default_wallpapers/     the wallpapers that ship, staged to /default_wallpapers
state.img               the state volume: ext4, mounted at /state, holds
                        settings.xml; made on first `make run`, gitignored
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
compositor work: a real `agent` with a model behind it, and more applications.

**Milestone 10: the real agentdesk.** `agentdesk/` is the workspace process
ARCHITECTURE.md always described, and `awapp` is gone. It owns the
conversation: what the human said, what the agent said back, and between them
every line the agent reported while it worked, all in the pane as it happens.
Send starts a turn by asking the broker for an agent, receiving the desk end
of the private channel on the reply (via `SCM_RIGHTS`; the stand-in used to
drop it on the floor), streaming history and the prompt down it, and reading
telemetry back up. The turn ends when the channel hangs up, whether the agent
finished, died or was stopped, so every ending is one code path. A message
sent mid-turn is queued and starts the next turn the moment this one ends.
The wire is `awproto::turn`: `history`/`prompt` down; `telemetry`, `open-app`
and `reply` up. `open-app` is how an agent opens an application: it asks the
workspace, which asks PID 1, and finds out whether it worked by asking the
compositor what is open. `awagent` speaks it now, so the demo turn is watched
in the pane rather than in the kernel log.

The start menu is a panel, not a process (`haimanager/src/startmenu.rs`).
The Agentware mark at the left end of the taskbar opens it, centred over the
workspace and sized to its content: a large prompt on top (Enter makes a new
agentdesk that begins with it; empty makes one with nothing to do), and below
it every installed application as a grid three across, icon over name, each
tile opening the app into the workspace on screen. A click anywhere else, or
Escape, closes it. It is compositor chrome for the reasons the dock is: it
needs the icons only the compositor holds, click-out is something only the
compositor sees, and it must work when the workspace under it does not. It is
still AWML through the same parser and painter; `button` gained `icon` and
`tile`, and `height` joined `width` as a compositor-internal hint, all for
this. Its requests go to PID 1 like the stop button's: `create-desk` and
`open-app`. The `+` at the end of the nav tabs still creates an empty
agentdesk, and the compositor asks for a blank one at boot and again whenever
the last one closes.

The taskbar band, left to right: the start button (compositor), the dock
centred (compositor), and the clock and date at the far right (the desk's;
kernel time, and QEMU is booted with `-rtc base=localtime` so it reads as
the host's). The desk re-renders on a one-second tick for the clock and reads
the wallpaper setting on the same tick.

The `background` region holds the wallpaper: an `image` element, new to the
catalogue, whose `src` the compositor loads (SVG via resvg, PNG via tiny-skia),
fits with `cover`, composites over the system background once per size, and
blits as opaque rows every frame. `awsettings` is a first-party app in
`agentwarecore`: a rail of categories down the right (Desktop, so far) and
the page beside it, where a `select` dropdown offers the pictures in
`/default_wallpapers` plus "Choose an image...", which opens the shared file
dialog filtered to SVG and PNG anywhere on the machine. The choice is written
to `/state/settings.xml` (`awproto::settings`: `Settings::load()` creates the
file with defaults on a first run, `save()` writes it whole, synced, and
renamed into place; a small reader that understands only what it writes),
the one thing that crosses between processes as a file: every desk stats it
on its clock tick and re-reads it when it changed, and the settings app
re-reads it before acting, so nothing ever writes a choice the machine has
moved past. `/state` is the **state volume**, the one filesystem that
outlives a boot: `state.img`, a 64MB ext4 image the Makefile creates on first
`make run` (`mkfs.ext4` on a file, no root) and QEMU attaches as a virtio
drive; the supervisor mounts `/dev/vda` (or `agentware.state=/dev/...`) on
`/state` right after the virtual filesystems and says so in the log, or says
settings will not outlive the boot if there is no drive. `make cleanstate`
deletes it, which is the first-run case. `tools/screenshot.py` boots against a
throwaway snapshot of it by default so captures never change the machine's
state; `--keep-state` writes for real, which is how persistence across boots
was verified. `select`/`option` are implemented for it (the
app owns `open`; the compositor sends `open`/`close`, floats the options over
what follows, and closes on a press elsewhere). `text` wraps, and `scroll
anchor="end"` keeps a transcript pinned to its end until the human scrolls
away.

First-party applications live in `agentwareapps/`, a separate workspace because
apps are clients of the display protocol, not parts of the system: they link
`awproto`, plus `awkit` when they need a dialog, and nothing else. `awcalc`, a
pocket calculator, is the first and the reference for how an application is
written: a model and a `render`, hand-written stable ids, no diffing, no
ephemeral state. `awfiles` is a file explorer: one folder at a time, every
row a checkbox and a name, the checkbox marking the file or folder and the
toolbar acting on what is marked (New folder, Rename, Copy to..., Move to...,
Delete, Mark all). Marking is a checkbox rather than a modifier key because
there are no modifier keys in the event vocabulary and should not be: an
agent marks a file by naming its checkbox, exactly as a human does. Every
question it asks is an `awkit` dialog. The demo agent has two scripts, picked
by the prompt: one adds 12 + 34 on the calculator and reads back 46, and one
whose prompt says "file" marks welcome.txt, opens Copy to..., is told
`blocked` for a control behind the dialog, walks into notes and confirms.

**A window opens at the size its content asks for.** AWML has no width to
declare, so both are derived on the first tree (`ui::document_width`,
`ui::natural_height`): the width is what the tree wants so every control sits
inside its container at natural size (rows sum, columns take the widest, text
counts up to a cap because it wraps, spacers count nothing), clamped between
the resize minimum and the room left in the workspace; the height is the
measure at that width. Rows measure each child at the slot it will really get,
and a growing `scroll` measures as at least a few rows. An app whose
top-level content is marked `grow` is saying the window should have room,
not just fit (`ui::wants_room`): it opens at half the workspace in each
direction, or its content's size if that is more, so a browser or a settings
page opens with room and a calculator opens the size of a calculator; the
proportion comes from the display, so no app carries a pixel size. First
trees only: an app that re-renders larger does not move a window the human
may hold. In a row, one-line controls (button, field, select, checkbox) keep
their own height and sit in the middle; containers and text take the row.

**Dialogs are in the app's tree** (`docs/UIElements.md`). An application with a
question renders a `dialog` among its nodes and drops it when answered. The
compositor floats it centred over the window and makes it modal for human and
agent alike: outside controls are unreachable by click, focus jumps into the
dialog, the agent's view marks them `blocked` with empty actions, and intents
on them are rejected `blocked` (distinct from `disabled`). To the agent a
dialog is controls nested in `<dialog>`, nothing more. `awkit` holds three:
`FileDialog` (`open(dir)`, `save(dir, name)`, `folder(dir)`; `render()` gives
the element, `accept(event)` answers `Ignored | Changed | Chosen(path) |
Cancelled`), `Confirm` (a yes-or-no, `.danger()` for destructive), and
`TextPrompt` (one line of text, `refuse(why)` to keep it open with a reason).
Each owns its ids and answers `Ignored` for events that are not its own, so
an app hands every event to its open dialog first. The file dialog reads the
filesystem in-process because nothing is namespaced; the portal that hands
back a descriptor instead is the later step and can keep this markup. A
dialog's first text control opens with the caret at the end of its value.

An installed app is a **package, not a binary**: `/apps/<name>/` holds `exec`
(what the supervisor forks), `icon.svg` (what the compositor draws in the title
bar and the dock; SVG so one file serves every scale), `description.txt` (what
the start menu carries per tile and agents will read), and `name.txt` (the
label under the icon; the folder name if absent). The supervisor only ever touches `exec`; the
compositor loads the icon itself under the name PID 1 handed over, parses it
once per app, rasterizes once per size, and caches the miss too, so an iconless
app costs one probe, not one per frame. The selftest stand-ins ship in the same
format so `make selftest` exercises the same spawn path, but without a
description, which is what keeps them out of the start menu. The dock is icon tiles
with a running dot, not text pills; an app without an icon shows its initial.

Rendering is still the hand-rolled rasterizer, with **tiny-skia behind
`Canvas`** for what genuinely needs a path engine: resvg renders the icons
through it, and `Canvas::blend_pixmap` is the one place premultiplied RGBA
meets the XRGB frame. Raised surfaces (buttons, title bars, the dock) carry a
faint top-lit vertical gradient, drawn as row-interpolated fills after the
tiny-skia scratch version measurably doubled paint time on a maximized window
full of buttons. Measured after: drag paint 3-6.5ms avg at 2560x1440, agent
turn repaints 5-6.8ms avg at 1600x1000.

No `agent` exists yet. `Programs::system` names `/bin/agent`; without
`agentware.demo` a message sent from a desk gets "could not start an agent"
in the pane and the log, and everything else works. With it, `awagent` stands
in and answers any message by adding 12 and 34 on the calculator. Either way
the machine boots to one blank agentdesk, no prompt, and closing the last
agentdesk opens a blank one, so there is never a display with nothing on it.

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

Once it is up, F1 cycles workspaces, F2 toggles a
diagnostic overlay listing every connection and the version it is on, and F3
folds the conversation pane away (animated, 200ms smoothstep; also the grip on
the pane's edge). Windows have a title bar with chevron/brackets/cross controls
(hover chips; close is danger), can be dragged, and resize from the right edge,
bottom edge and corner, with double-arrow cursors over the bands. The pointer
becomes an I-beam over text, and carets blink on a 530ms clock that wakes the
loop only when a caret exists. Scrollbars hug the content edge, drag (thumb or
track-jump), and auto-hide 900ms after the content stops moving; the agent's
`scroll-into-view` lights them the same way.

The start menu's tiles and prompt, the nav bar's `+` and the pane's Send
button all go through the real broker, so clicking them forks real processes:
Send starts an agent turn, which is the whole of milestone 7 running against a
live screen, watched in the pane. The stop button ends it, and the pane says
so.

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

The `--append` matters: without `agentware.demo` there is no agent to answer a
message, so a turn cannot be photographed; the desk itself is there either way. The pointer starts in the middle of the screen and every move is a
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
* Workspaces, applications, windows and conversations do not survive a
  reboot, by choice: no session restore, no on-disk state for any of them.
  **Settings do.** They live in `settings.xml` on the state volume, the one
  filesystem the supervisor mounts from disk, and nothing else is written
  there until it earns a place.
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
* There is **no start menu process**. The start menu is a compositor panel:
  a prompt that becomes a new agentdesk, and the grid of installed apps. Its
  requests are `create-desk` and `open-app` to PID 1, the way the stop button
  and a tab's close are; the nav bar's `+` is `create-desk` with no prompt,
  and the compositor asks for the first workspace at boot. The human opens
  apps from the menu; an agent opens them only through its agentdesk.
* **Settings are the one thing that crosses as a file.** A preference set in
  one process and read by every workspace has no socket to travel on, so it is
  one file, `settings.xml` on the state volume, written whole and atomically
  by whoever changes it and re-read by everyone else when its clock changes.
  Nothing is broadcast, and the file is the truth: a first run creates it
  with the defaults, and every reader agrees with it from then on.
* **The agent opens applications through its agentdesk**, never itself. It has
  no broker connection; `open-app` up the turn channel is a request, and the
  agent learns the outcome by asking the compositor what is open.
* **A dialog belongs to the application's tree**, never to a separate process
  or to chrome, so the agent sees it in the app's view as controls nested in
  `<dialog>`. The compositor makes it modal for human and agent alike; there
  is no attribute to opt out.

`width` and `height` attributes on a control are compositor-internal sizing
hints in physical pixels (the tab rename field keeps its tab's width; the
start menu's prompt is a large box), and `icon` and `tile` on a button draw
an installed app's icon beside or above its label (the start menu's grid).
None of them is part of the application catalogue and none reaches an agent.

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
* **A per-repaint scratch rasterization is a per-frame cost.** Gradient buttons
  drawn through tiny-skia scratch pixmaps doubled paint time the moment a
  maximized window was full of them; the frames log caught it. Anything drawn
  every frame must be row fills, cached tiles, or a blit of something
  rasterized once. tiny-skia is for icons and genuinely curved work, not for
  shapes a row fill can describe. The wallpaper follows the rule: fitted and
  composited once per size into opaque XRGB rows, blitted per frame (about
  1.5ms at 2560x1440; steady-state paint with a wallpaper showing measured
  7.9ms avg there, 2-5ms at 1600x1000). The first fit of a full-screen SVG is
  one long frame, 160ms at 1600x1000 and 350ms at 2560x1440, once per
  wallpaper per size; moving it off the paint path is the obvious next step
  if it ever matters.
* **The parser decoded entities in attributes and not in text.** Nobody had
  put a quote in text content until the pane showed the agent's telemetry as
  `&quot;hello&quot;`. `escape` is used on both, so `unescape` must be too.
* **A tick-driven client discards clicks it should not, unless it checks
  versions.** The agentdesk re-renders once a second for its clock, so a click
  can land against a tree that is one version stale for no reason the human
  can see. The stale check is by version, so such a click is thrown away; the
  cost is one lost click a minute at worst and the alternative is acting on a
  tree the human did not see. Do not "fix" it by skipping the check.
