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
  awagent/              the per-turn agent: the harness owning the agentic
                        loop and both channels, a Backend trait that is one
                        streamed model exchange, and backends/claude.rs
                        speaking the Anthropic API over rustls
agentwareapps/          cargo workspace: first-party applications
  awcalc/               a calculator, the first real application
  awfiles/              a file explorer, and where the shared dialogs are seen
  awsheet/              a spreadsheet: one open CSV file per tab
home/                   the skeleton for /home: what a machine has the first
                        time it boots. Copied onto the state volume when there
                        is nothing there yet, and never read again, so a
                        person's files survive `make pack`
default_wallpapers/     the wallpapers that ship, staged to /default_wallpapers
default_themes/         the palettes that ship, one XML each; dark.xml is the
                        palette the compositor paints with unless told otherwise
state.img               the state volume: ext4 on /dev/vdb, mounted at /state,
                        holds settings.xml; made on first `make run`, gitignored
initramfs.cpio.gz       the boot stage: the supervisor and /dev/console,
                        nothing else; written by tools/mkinitramfs.py
system.img              the OS: an ext4 volume (/dev/vda) the supervisor
                        mounts and binds into the root, demand-paged; rebuilt
                        whole by `make pack`
sysroot/                the staging tree system.img is written from (build
                        output, gitignored)
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
synthesize the event. Scrolling happens on the way: every container above the
target is moved so that it is reachable, so no rejection mentions the screen.
The reverse can
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

**A second round of that, from "dragging windows goes sluggish after working
with an agent for a while".** Everything below was found by measuring rather
than reading, and every one of them is a cost that grows with *use* and is
therefore invisible in a capture of a fresh boot. The `frames:` line now
carries what arriving trees cost as well, since a frame's paint is only half
of what a drag competes with.

* **Line breaking was quadratic.** `wrap` measured `text[start..next]` from
  the top of the line at every character, so a line cost the square of its
  length in glyph lookups. It now carries the width forward as the loop
  advances, over `Fonts::advance`.
* **Painting walked every node whether or not it could be seen.** A
  transcript line scrolled out of the pane still ran its line breaking and a
  glyph lookup per character, discarded a pixel at a time. Now a branch whose
  layout clip the canvas has already excluded is skipped whole (a child's clip
  is its parent's or a piece of it, so this is sound), and a node whose own box
  is out of view is handed an empty clip.
* **Measuring cannot be skipped the same way, so it is remembered.** A
  `scroll` is only as tall as its content, so the pane cannot know its own
  height without breaking every line of the conversation, including the
  thousand that scrolled off the top; culling helps painting and can never
  help this. `Fonts::break_lines` caches where each line of a string starts
  and ends, by string, face and width (`WRAPS_KEPT` entries, dropped whole
  when full rather than evicted one at a time). A transcript re-rendered for
  its clock breaks no lines at all the second time.

Measured with `bench_pane` (ignored, `--nocapture`), which lays out and paints
a pane of telemetry lines. Layout is what the agentdesk pays once a second
while a turn runs; paint is what every frame pays, a window drag included:

| lines | layout before | layout now (cold / warm) | paint before | paint now |
| --- | --- | --- | --- | --- |
| 100 | 37.8ms | 0.55 / 0.11ms | 13.0ms | 1.5ms |
| 400 | 163ms | 2.35 / 0.45ms | 52.9ms | 1.6ms |
| 2000 | 836ms | 11.7 / 2.2ms | 267ms | 1.9ms |

Four hundred lines is one afternoon with an agent. Two thousand is a week,
and it used to be 836ms of the compositor's thread once a second and a quarter
of a second per frame, which is not a sluggish desk, it is a stopped one.
* **A workspace nobody was looking at repainted the one in front.** Every tree
  from a background agentdesk or a minimized window set the screen dirty and
  threw away the drag's damage rectangle. `Screen::readable` now answers for
  the screen, not for the client, and so do an agent's intents and its typing.
* **A press animation anywhere ran the compositor at 60Hz.** `wants_frame`
  asked every client whether it was animating, so a control sinking in a
  workspace nobody is looking at bought a fifth of a second of full repaints,
  once per agent action. Measured against a two-desk case the last two are
  worth about one repaint a second plus those bursts; they matter because
  they compound with the number of agentdesks left working, and because they
  are the difference between a window drag keeping its damage rectangle and
  losing it.

What is left is genuinely linear and small: laying out a conversation still
walks every line to add up its height, at about a microsecond each once they
are broken. Windowing the transcript the way a table is windowed would remove
even that, and would need the pane's scroll offset to cross the protocol,
which it deliberately does not.

Verified by pixels as well as numbers: the whole screen is identical to the
build before this work at 2560x1440 and at 1600x1000, and culled against
unculled is identical with a scrolled table, a strip of tabs and an open menu
on screen.

**Nothing is left of the original plan.** What is missing now is not
compositor work: more applications, and more backends behind the agent.

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
The wire is `awproto::turn`: `backend`/`history`/`prompt` down; `telemetry`,
`open-app` and `reply` up. `open-app` is how an agent opens an application:
it asks the workspace, which asks PID 1, and finds out whether it worked by
asking the compositor what is open.

The start menu is a panel, not a process (`haimanager/src/startmenu.rs`).
The Agentware mark at the left end of the taskbar opens it, centred over the
workspace and sized to its content: a large prompt on top (Enter makes a new
agentdesk that begins with it; empty makes one with nothing to do) with the
model that will answer it beside the button that starts it, and below
it every installed application as a grid three across, icon over name, each
tile opening the app into the workspace on screen. The heading says what the
panel is; the sentence that used to explain the box underneath it is gone,
because the placeholder in the box already says what it is for. A click anywhere else, or
Escape, closes it. It is compositor chrome for the reasons the dock is: it
needs the icons only the compositor holds, click-out is something only the
compositor sees, and it must work when the workspace under it does not. It is
still AWML through the same parser and painter; `button` gained `icon` and
`tile`, and `height` joined `width` as a compositor-internal hint, all for
this. Its requests go to PID 1 like the stop button's: `create-desk` and
`open-app`, and from the power controls in its lower-left corner, `poweroff`
and `reboot`, which the supervisor turns into the signals its shutdown path
already handles, so the buttons run the same orderly teardown Ctrl-Alt-Del
does; `make run` no longer passes `-no-reboot`, so Restart boots the machine
fresh rather than exiting QEMU. There is deliberately no Sleep: the kernel
could suspend, but QEMU wakes a guest only from the host monitor, and a sleep
the keyboard cannot end is a power-off wearing the wrong label. The `+` at
the end of the nav tabs still creates an empty agentdesk, and the compositor
asks for a blank one at boot and again whenever the last one closes.

The taskbar band, left to right: the start button (compositor), the dock
centred (compositor), and the clock and date at the far right (the desk's).
The kernel keeps one clock, UTC, and knows nothing of zones; the image has no
zone database; so the zone is a setting, an offset from UTC chosen on the
Settings app's Time page from a scrolling list of the offsets places keep,
stored as `<time><utc-offset>+05:30</utc-offset></time>`, and applied by the
desk when it shows the time. No daylight saving. A first run shows UTC until
told. The desk re-renders on a one-second tick for the clock and re-reads the
settings file on the same tick when it has changed.

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
moved past. `/state` is the **state volume**, the machine's own disk as
opposed to the OS image: `state.img`, a 64MB ext4 image the Makefile creates
on first `make run` (`mkfs.ext4` on a file, no root) and QEMU attaches as
the second virtio drive; the supervisor mounts `/dev/vdb` (or
`agentware.state=/dev/...`) on `/state` right after the virtual filesystems
and says so in the log, or says settings will not outlive the boot if there
is no drive. `make pack` rebuilds the OS image and never touches it. `make cleanstate`
deletes it, which is the first-run case. `tools/screenshot.py` boots against a
throwaway snapshot of it by default so captures never change the machine's
state; `--keep-state` writes for real, which is how persistence across boots
was verified. `select`/`option` are implemented for it (the
app owns `open`; the compositor sends `open`/`close`, floats the options over
what follows, and closes on a press elsewhere). `text` wraps, and `scroll
anchor="end"` keeps a transcript pinned to its end until the human scrolls
away.

**The palette is a theme, and a theme is a file.** The thirteen colours the
compositor paints with (`ui::background()` and friends, atomics read at paint
time) come from one XML file per theme in `/default_themes`, the original
hardcoded palette shipped as `dark.xml`, so far alone; another theme is a
file dropped beside it, nothing registered anywhere. Which
one is `<desktop><theme>` in `settings.xml`, chosen from a dropdown under
the wallpaper's on the Settings app's Desktop page; the compositor stats the
settings file once per idle loop pass and swaps the palette when it names
another theme, so the whole machine changes within a second, no restart. A
theme names every colour or is refused whole: there is deliberately no
palette in the code to patch a file with (`awproto::theme`), only an
emergency monochrome the screen wears if no theme file loads at all, ugly on
purpose so a broken image gets reported rather than shipped. The wallpaper
cache is the one thing that bakes a colour in (composites over the
background), so a theme change clears it. Floating dropdown options clip to
the document, not their container, which the theme dropdown discovered by
living in a group one row tall.

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

`awsheet` is a spreadsheet, and **one tab is one open file**. It reads and
writes CSV, which is the only format it knows and the only one it claims:
there are no formulas and no arithmetic, and a cell holds the text typed into
it. The File menu is New, Open, Save and Save as, each answered by an `awkit`
dialog filtered to `.csv`; a name saved without an extension gets one, since
the format it writes is the format it reads. A tab whose sheet has changed
since it was saved carries a `*`, and closing one asks first, through
`Confirm` in the app's own tree like every other question. A clean tab closes
without a word: a dialog that appears when nothing is at stake is one people
learn to click through. Opening a file that is already open shows that tab
rather than opening it twice, because two tabs on one file are two sets of
edits with one place to put them. Saving writes to a scratch file and renames
it into place, so a failure halfway leaves what was there rather than half of
what was coming. What is written is the rectangle from A1 to the furthest
cell with anything in it, which is the only part of a thousand-row sheet
worth writing down.

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

**The agent is real.** `awagent` is the per-turn worker with a model behind
it; the scripted stand-in and the `agentware.demo` flag it needed are gone,
and `Programs::system` names `/bin/awagent`. The harness in `main.rs` owns
the agentic loop and both wires: context down the turn channel, the
conversation to the model through a `Backend` trait whose whole contract is
one streamed exchange, and every tool call executed over the compositor link
as intents. The model gets four tools that map one to one onto the agent
surface: `list_apps`, `read_app`, `act` (the fourteen-verb vocabulary as an
enum), and `open_app` (up the turn channel, then polling `apps()` until the
app appears). A rejection (`blocked`, `disabled`, `unsupported-action`) goes back as
a tool result for the model to reason about, which is rejections-as-answers
carried one level up. The agent wire also carries one unsolicited word,
`changed <app>`, sent when an application's tree genuinely differed on a
re-render; the harness answers it itself, re-reading each changed app after
the tools run (a 150ms settle first, so the drain does not race the very
re-render it exists to catch) and attaching the fresh views to the tool
results, so the model sees the consequences of its actions without spending
an exchange asking. The name crosses, never the diff: the remedy is a whole
fresh view, for the same reason apps send whole trees. Thinking streams into the pane as `thought` telemetry
a line at a time, narration between tool calls as `result` lines, every act
as an `action` line with its outcome, and the model's final message, the one
with no tool calls, is the reply. While a turn runs the transcript ends with
a working line, "Claude Opus 5 is working... (14s)", dots moving on the
desk's own clock tick, so a model thinking quietly still visibly exists.
Stopping a turn has three doors, all ending in the same `interrupt` to PID 1:
the nav bar's Stop, a stop square beside the pane's send button while a turn
runs, and a click anywhere in the workspace's apps region, which is the human
taking the workspace back. The composer is an `editor`, multi-line and
marked `enter-submits`: Enter sends, Shift+Enter starts a new line, the
paper-plane button sends too, and the send button stays through a turn
because a message sent mid-turn queues. `backends/claude.rs` speaks the Anthropic
Messages API: raw HTTPS over rustls with the ring provider and webpki's CA
bundle compiled in (`http.rs` is the whole client: HTTP/1.1, chunked
transfer, SSE), streaming, adaptive thinking with summarized display, the
system prompt as a cached prefix, and bounded retries for what retrying can
fix. The awagent crate carries the only network dependencies in the system;
PID 1 and awproto stay dependency-free.

**Which model answers is chosen in the pane.** The turn wire's `backend`
message names one of the configurations in `awproto::turn::BACKENDS` (label,
backend, model: five Claude models today, Opus 5 down to Haiku 4.5), a
dropdown the agentdesk draws at the top of its pane. Per workspace, deliberately: which model answers is a
property of the conversation being had, and the selector is desk chrome, so
an agent is told what it runs as and can never see or change the control. A
change applies from the next turn. The API key is the machine's: entered on
the Settings app's Agent page (a `kind="password"` field, committed on Enter
or Save), stored as `<agent><anthropic-key>` in `settings.xml`, and read by
the agent at each turn. The compositor masks a password field's `value` in
the agent view exactly as it does on screen, so the one process that could
echo the key to a model reads asterisks; a turn without a key answers with
where to set one.

**The guest has a network, for the agent alone.** QEMU adds a slirp NIC
(`-netdev user`), the kernel configures it itself from the `ip=` boot
argument (CONFIG_IP_PNP and CONFIG_VIRTIO_NET were already in the config),
and the supervisor's one contribution is `/etc/resolv.conf` naming slirp's
DNS at 10.0.2.3, written at boot onto the initramfs root and reported
boot_report style. No process in the userland holds networking code except
the agent. Either way the machine boots to one blank agentdesk, no prompt,
and closing the last agentdesk opens a blank one, so there is never a
display with nothing on it.

## Building and running

```
make selftest    # headless supervisor self-test, exits 0 on success
make run         # boot in a QEMU window
make pack        # build the boot stage and system image without booting
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
revealing a node for an agent lights them the same way.

The start menu's tiles and prompt, the nav bar's `+` and the pane's Send
button all go through the real broker, so clicking them forks real processes:
Send starts an agent turn, which is the whole of milestone 7 running against a
live screen, watched in the pane. The stop button ends it, and the pane says
so.

Builds target `x86_64-unknown-linux-musl`. No sudo is needed anywhere:
`mkfs.ext4 -d` populates the system image from the staging tree as an
ordinary user, and the initramfs's console node is written straight into the
archive by `tools/mkinitramfs.py`, because a device node only needs
privileges to exist on a filesystem, not in an archive.

**Boot is two stages, like an actual OS.** The initramfs holds exactly what
must exist before any disk does: the supervisor and `/dev/console`, about
250KB. PID 1's pages live in RAM, so a missing or dying disk is something it
reports rather than something that takes it down: booted without the system
drive, the machine comes up with a live supervisor saying "no volume at
/dev/vda; nothing beyond the supervisor can run" instead of a kernel panic.
Everything else is the **system volume**, `system.img` on `/dev/vda`
(`agentware.system=` overrides): the supervisor mounts it at `/system` and
bind-mounts its top-level directories (`bin`, `apps`, the wallpapers and
themes) over the root, so no path anywhere else in the userland
changed. The kernel demand-pages all of it, so code is in memory only while
it runs. Workspaces and conversations still die at reboot; they always
were process state. At shutdown the binds come off and the volume properly
unmounts, which PID 1 living off-disk is what makes possible.

**`/home` is not on that volume, and putting it there was a real bug.** Files
were part of the OS image, so they were rebuilt every time it was: `make pack`
writes the image whole and `make run` packs first, which made every boot during
development a reinstall and every saved file disappear. Nothing an application
could do would have survived it. So the image's `home/` is a **skeleton** now,
what an installer would call it: `early::mount_home` copies it onto the state
volume the first time there is nothing there to copy to, and never reads it
again. `/home` is a bind out of `/state/home`, a reinstall leaves a person's
files alone, and `make cleanstate` is what erases them, which is the gesture
that already meant this machine has never been booted. Without a state volume
the image's copy is bound instead, read-write in RAM, and the log says it will
not last, which is the same graceful degradation settings get.

The other half of that bug was in the application, and it is the older trap:
**a rename without an fsync gives you the new name and the old contents, or
none.** `awsheet` wrote its scratch file and renamed it, unsynced, so the
directory entry reached the disk and the bytes did not. The save said it had
worked, the next boot showed an empty file, and it read exactly like a fault in
the state volume. Write, `sync_all`, rename, then sync the directory, which is
what `awproto::settings` already did and the reason it was already right.

## Verifying graphics

**Graphics cannot be checked from a serial log.** `tools/screenshot.py` boots the
guest, optionally injects input through the QEMU monitor, captures the
framebuffer and writes a PNG, which can then be viewed directly.

```
tools/screenshot.py out.png --seconds 8 --append "console=ttyS0,115200" \
  --do "mouse_move 150 -120" --do "mouse_button 1" --do "mouse_button 0" \
  --do "sendkey h" --do "sendkey shift-l" --do "mouse_move 0 0 -1"
```

The tool adds the slirp NIC and the `ip=` boot argument itself, so a capture
has the same network `make run` has. Photographing a real agent turn needs an
API key in the state image's `settings.xml`, which `make
configure_anthropic_key` writes with `debugfs -w` rather than making anyone
type a secret through the monitor; without one, a message is answered with
where to set the key, which photographs fine. The
pointer starts in the middle of the screen and every move is a delta from
where it is now.

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
  They are process state and die with their processes. **Files do survive**,
  now that the system volume is a disk: what an app saves is saved, like an actual
  OS. **Settings do too**, on the state volume in `settings.xml`, which is
  the machine's own disk as opposed to the OS image, and nothing else is
  written there until it earns a place.
* The supervisor never sees prompts, conversation, telemetry or markup. It
  creates sockets and steps out of the way.
* Identity is a capability, not a claim: the haimanager knows which workspace a
  connection belongs to because the supervisor said so at handoff.
* The stop button is drawn by the haimanager and routes to the supervisor, so it
  works even if the agentdesk is wedged.
* Applications send the **whole tree** every time; the haimanager diffs it.
  The one exception is a `spreadsheet`, whose cells are published as runs on
  their own frames and are never diffed at all. It earns the exception by
  being the first thing that is not small, and it pays for it with a version
  in the tree that every render re-asserts.
* Window chrome is the **compositor's**: title bar, shadow, and the three dots.
  They are the only controls that are not AWML, so no application decides
  whether it is closable and no agent sees the button that destroys its window.
  **The cross asks, and that is all it does.** It sends `close` with an empty
  target; the application exits, or puts a question up and exits when that is
  answered. Only the application knows whether there is anything to lose, so
  only it can decide what closing means. Nothing in the compositor can end a
  process and there is no verb at PID 1 for it either: `close-app` is gone,
  along with the pid the compositor used to keep in order to name one. The
  window goes when its connection does. **This assumes applications written in
  good faith**, which on this machine they are, and it is worth what it saves:
  the alternative carried a second press that closed the window anyway, a flag
  on every window, a clock to tell one gesture from two, and the compositor
  reaching into a client's tree for a dialog to decide whether it had been
  answered. An application that ignores being asked is a bug in that
  application.
* An agent's typed text goes in **one character at a time**, producing one event
  per keystroke. A value set in one step is something no human could produce.
* A human's click and an agent's intent end in **one function**. Nothing else may
  synthesize an event, or the two paths drift and the guarantee that an agent can
  only do what a human could have done stops being checkable.
* **Scrolling is not in the agent's vocabulary and never was a good idea
  there.** Acting on a node scrolls every container above it, outermost
  first, and a grid to the cell being acted on, exactly as acting on an
  application arranges its window. An agent expresses what should be true and
  is not told about pixels. `not-visible` is gone with it: every case it named
  was either a bug here (a nested container not walked, a grid walked past) or
  a different word (a folded option offers no actions, which is what it should
  say). What is left is `unreachable`, which means the compositor tried and
  failed, and is logged as the fault it is.
* **A sheet is read by asking for a rectangle, not by reading the view.**
  `query cells <app> <id> <range>` answers with rows of values. A query rather
  than an action because it is a read, and unlike the `query rows` it replaced
  it asks the application nothing at all: the compositor holds the cells, so
  the answer is a lookup.
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
  a prompt that becomes a new agentdesk, the grid of installed apps, and the
  machine's power controls. Its requests are `create-desk`, `open-app`,
  `poweroff` and `reboot` to PID 1, the way the stop button
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
* **The key is the machine's; the model is the conversation's.** The API key
  lives in `settings.xml`, entered once on the Settings app's Agent page.
  Which backend configuration answers is chosen per agentdesk in the pane,
  from the one table in `awproto::turn`, and travels to the agent as the
  `backend` message on the turn channel. The selector is desk chrome, so an
  agent is told what it runs as and can never see or change the control.
  **The start menu offers the same choice for a conversation that does not
  exist yet**, from the same table, because choosing the model after the
  first turn has already run is choosing it too late. It travels to the new
  workspace the way the opening prompt does, as an argument PID 1 passes on
  without reading; a name the agentdesk does not recognise falls back to the
  default, which is what a workspace nobody chose for gets anyway. It is not
  a machine setting and is not remembered between openings: the next
  conversation is not this one.
* **A dialog belongs to the application's tree**, never to a separate process
  or to chrome, so the agent sees it in the app's view as controls nested in
  `<dialog>`. The compositor makes it modal for human and agent alike; there
  is no attribute to opt out.

**The grid: `spreadsheet`, and the one place the whole-tree rule is set
aside.** `table`, `column`, `row` and `cell` are gone. They worked, and they
did not scale in three directions at once. A screenful of a grid is four
hundred cell elements, so an agent paid **46.6 KB to read twenty numbers**,
89% of it identical action lists and descriptions the compositor had composed
itself; every keystroke made the application re-serialise the visible grid;
and ten thousand rows could only be described a window at a time, so
scrolling was a question the application had to answer.

So a spreadsheet's cells are **not in the tree**. The element carries a
*name* and a *version*, the way an `image` carries a path, and the cells go
up the same socket as their own frames (`awproto::display::MSG_SHEET`,
`haimanager/src/sheet.rs`):

```
sheet <source> <version> <base> <at> <value>...
```

One shape and no operation verbs: put these values in, starting at this cell
and running across. A single cell is a run of one, a row is a run of many,
and clearing is a run of empty strings, because in a sheet an empty cell and
a cleared one are the same cell. **Nothing is compared with anything** — the
compositor does not work out what changed, it is told, and it writes what it
is told. `version` is what the sheet becomes and `base` what it must already
be, so a run that cannot be placed is refused whole and answered with
`sheet-resend`; a `base` of zero means "forget what you have", so a first
publish and a recovery are one code path. `SheetOut` owns the counter,
because getting it wrong is the one way this can go wrong. Ordering is free
because it is one connection: cells, then the tree that claims them.

**Both axes became the compositor's**, which is a settled decision reversed
on purpose. It held while the rows on screen were the ones the application
chose to describe; it does not hold now that the compositor holds the sheet.
Sixty wheel notches move a thousand-row sheet, and a drag of its bar from the
top to row 989, with the application hearing nothing at all: `query rows`,
`first-row`, `PendingRows`, the deferred reply and the `scroll` action all
went with it. What the compositor holds is what the application *chose to
publish*, and it scrolls the shape the element declares.

**A `source` is a sheet, not an application**, and the compositor holds one
per source whether or not an element points at it. That is what makes a
workbook cheap: `awsheet` gives each open file a stream of its own (`book/1`,
numbered so a closed sheet's name dies with it), so switching tabs is a
different `source` in the next tree, with no cells on the wire, nothing
diffed and nothing thrown away.

It used to share one stream between the three sheets and republish on every
tab click, which was both the cost and a bug: `publish_all` sends one run per
row that has anything in it, so a sheet with nothing in it sent nothing at
all, `restart` was a flag waiting for a run that never came, and the
compositor went on showing the sheet before it. **An empty sheet has to be
sayable**, so `SheetOut::restart` now sends a snapshot carrying no values,
which is also the only way to tell the compositor a stream is finished; a
snapshot that brings nothing hands the table's memory back rather than
keeping an empty husk.

And the check that would have caught it did not exist. The version on the
element was written down as re-asserted on every render and read by nobody;
`Client::fit_sheets` now compares it with the version held and answers
`sheet-resend` when they differ, once per disagreement rather than once per
render. The documents claimed divergence was caught on the next frame for as
long as it took for divergence to actually happen.

**Resizing is declaring a different shape**, and there is no resize message
because the tree already answers that question on every render. The two
directions cost very different things, which is the point. Growing is free at
both ends: a cell's place is arithmetic rather than a node, so `rows="1000"`
becoming `rows="100000"` moves nothing, allocates nothing and is not diffed.
Shrinking is the one place in the system where a client's memory shrinks on
its say-so: a cell outside the shape is not scrolled away, it is not in the
sheet, so `Sheet::fit` drops it and hands the table back its slack. The pass
is over what the sheet holds, not over what it declares, so cutting a hundred
thousand columns to ten costs the filled cells and nothing else. Deciding
between the two is two comparisons against a bound written as cells arrive
(`Sheet::reach`, an upper bound that is never walked back when a cell is
emptied, because all it has to answer is "can this shape cut anything?"), so
the usual case, a render re-asserting the shape it already had, walks nothing.

It is safe only because of the publish order. Cells go up before the tree that
claims them, so a value written past the old shape is always followed by the
shape that makes room for it, and a cut can only drop what the application has
just said is not in the sheet. A well-behaved application therefore never
relies on it: `awsheet` empties the row it is about to lose. The cut is what
guarantees the compositor cannot hold, paint or report a cell outside the
declared shape whatever an application does, which matters most for `used`,
the one attribute an agent reads to decide what to ask for.

There is **no 26-column cap** anywhere: the lettering runs A, B, … Z, AA as far
as it is asked, `parse` takes four letters, and both axes are capped at 100,000
only because the geometry is arithmetic a client chose the inputs to. The 26 in
`awsheet` is `awsheet`'s.

The honest limit: an application whose sheet is too large to publish whole
has no way to be told where the view is, so it cannot stream a band. Nothing
needs that yet — a sparse sheet of a hundred thousand filled cells is about
6MB — and the shape a fix would take is a `scroll` event again, opted into on
the element so the common case stays silent. It is not built, so it is not
claimed.

**A cell is a coordinate, not a node.** There is nothing for a target to
name, so the cell travels beside the action — a `cell` field on the event,
and `sheet!B7` in an intent. Layout produces one `Grid` record of arithmetic
instead of four hundred rectangles, and painting, hit testing and the fake
cursor all read cells out of it, so a sheet of a million costs what a sheet
of ten costs. Column letters are the compositor's, because A, B, … Z, AA is
what every spreadsheet does. Typing into a cell is the compositor's copy
until the application echoes it, exactly as in a field.

Measured, first tree of `awsheet`: the agent's view went from **423 lines and
46,650 bytes to 14 lines and 1,082**, and the tree from about four hundred
nodes to twelve.

One bug worth keeping: the compositor's copy of a cell being typed into is
keyed `element!B7`, which is not a document key, and `reconcile` pruned
`editing` by asking whether the key was still in the tree. It never was, so
every re-render threw the edit away — and the application re-renders after
every keystroke. Typing `42` into a cell left `2` in it.

**A grid is more than cells.** `menu` and `menuitem` are a dropdown by
another name and share its machinery: items float while open, fold to
nothing when closed, painted last and hit first. **A `menu` is a child of
`window` and of nothing else**, and a window's menus are its menu bar: the
compositor lays them across the top of the window under the title bar it
draws, paints the band, and stacks the content below. Where a menu bar goes
is not a thing an application gets an opinion about, any more than where its
window goes is, so a menu anywhere else is a parse error and the document is
refused whole. That rule was not there at first and it cost the only thing
that mattered: the strip could hold anything, `awsheet` put its Edit menu in
its row of sheet tabs, and what came out was a word sitting among things you
choose that opened instead of choosing. Nothing in the markup was wrong; the
markup allowed a sentence with no meaning, which is exactly what a closed
vocabulary is supposed to prevent. `tabs` and `tab` are a
strip of things you choose, holding no panels, because which content belongs
to a tab is the application's business. **The navigation bar is built from `tabs` and `tab`**, which is the only
reason an application's tab strip looks like it: there is one implementation
and the bar is its first user. `nav_markup` writes the same markup an
application writes, and the element paints the band, the tabs, their crosses
and the hairline for both. What is left in `screen.rs` is the bar's own
behaviour: switching workspaces and renaming in place. Renaming is not
shared because an agentdesk's name is chrome the compositor keeps and never
tells the workspace, so there is nothing in the protocol for it; an
application that wants an editable name renders a `field` and owns it.

**Dragging a tab to reorder is shared, up to the point where the two differ.**
`ui::tab_slot` answers "which slot is the pointer in" for both, from the
tabs' rectangles and nothing else, so a burst of motions between two
repaints all agree. What it cannot share is what happens next: the bar's
order is the compositor's and it rearranges itself, while an application's
order is the application's, so a `movable` tab's drag sends `move` naming
the slot and the application answers with a new tree, the way it answers
every other event. An agent has `move` too: where a sheet sits in a workbook
is the document's business, unlike a window, which it may never arrange.
A positional tab id does not survive this, which is why `awsheet` numbers
its sheets: the tab a name refers to would change under the hand carrying
it.

The bar did not change when it moved onto the element: **zero differing
pixels at 2560x1440 and at 1600x1000**, against a capture of the old
implementation taken by stashing the work. That is the only acceptable
result when what is being shared is how something already looks.

Getting there meant reproducing four accidents of the old code, none of them
guessable and each found by diffing captures. The bar is `raised` with the
desk's colour punched into the middle, so a margin stays raised at each end.
Its inset is *derived* from the bar's height, so it lands differently at
every interface scale, and the `+ 12` and the `10` inside that arithmetic
are unscaled. Its tabs are not clamped to that inset: they stand their
natural height and are **clipped** by it, which is why they have no bottom
edge and why their lighting runs the length of a taller shape than is
visible. And the room for the cross is four literal spaces measured as part
of the label, not a width added to it, because a proportional font's
advances do not accumulate the same way.

Every one of those was first written as a scaled constant, which agreed with
the bar at 1.0 and nowhere else.

Two things the bar could not have shown, because the bar is the whole width
of the screen and a window is not. The strip's tabs are measured off the
**band** it paints rather than off its own box, so an application's first
tab starts the same distance in as the bar's; measured off the box they sat
a window's padding further in, a margin the bar does not have. And a box
with a border all the way round is clamped to what is visible as well as to
the row: a tab has no bottom edge on purpose, and the rename field, sharing
the row with tabs, was losing its bottom edge to the same clip. That one was
in the old bar too, so it is a fix rather than a regression.
Neither a tab nor a menu renders a press, and a tab draws no focus ring:
becoming the chosen one, or opening, is the feedback, a flash on top of it
is a button being clicked, and the ring is what made switching quickly
flicker, since a pressed tab outlines itself an instant before the
application answers and fills it. That rule is in the paint rather than in either click path, since
the bar's tabs are clicked through the compositor's handler and an
application's through `Client::act`, and matching conditions in two places
drift. A tab is then a button through
`paint_control_face` with `emphasis="primary"` on the chosen one, and a
`closable` tab carries the same cross the bar's do, from the same
`ui::draw_tab_close`. The strip holds whatever else belongs in the bar: the
plus that adds a sheet is an ordinary `button`, which unlike the
compositor's own plus is a control in the tree and so is addressable by an
agent. `column` became a control so a
header can be chosen, which is how a whole column is; a row already was.

`select-range` names two corners, one as the target and one as the value,
and is refused unless both are cells of the same table. One action because a
drag across a grid is one gesture, and because an agent saying "A1 through
C5" is legible where fifteen selects are not. The human's drag sends the
same event again each time the run reaches another cell, on the same
principle as one event per keystroke.

**Right-click belongs to the application.** The other button sends a
`context` event naming what was under it and nothing else; an application
answers by opening one of its menus, and the compositor hangs that menu's
items from where the press landed, which is the whole of what makes a
context menu appear under the hand. Nothing in the tree says so, because the
compositor is what saw the press; a press with the ordinary button, or any
agent action, clears it. An agent has no right-click and needs none: it
opens a menu by naming it and reaches the same commands without a pointer.

**Selection and the clipboard are the human's, and never reach an
application.** A press anchors, a drag extends, the run paints behind the
words; Ctrl+A/C/X/V are resolved against the compositor's own copy of a
text control, so a paste arrives at an application as an ordinary
`type-text` carrying the value the control now has. No modifier reaches the
event vocabulary: `input/keymap.rs` turns the chord into an intention
first. One clipboard per machine, in `haimanager/src/clipboard.rs`, holding
a kind and its content, text today and base64 images the shape it is
already written for, so a reader that only understands words can say so
rather than print base64. An agent may read it (`query clipboard`) and has
no way to write one: something it wants said, it says with `type-text`.

**There is one text box on the machine** (`haimanager/src/text.rs`), and an
application's `field`, an `editor`, a `cell` being typed into, the start
menu's prompt and the navigation bar's rename field are all it. It was not
true before: the two chrome boxes had `push` and `pop`, so neither could be
selected in, copied out of, pasted into, or have its caret moved by an arrow
key, and nobody would have designed that. It happened because each of the
three was written where it was needed and a text box is small enough that
writing it again never feels like the mistake it is. What each caller keeps
is only what Enter means, which is the one thing that genuinely differs: a
field submits, an editor breaks the line, the prompt makes a workspace, the
rename field commits a name. A press puts the caret down and anchors a run,
a second press in the same place takes the word and a third takes the line,
shift with an arrow drags a run out from the keyboard whether or not anything
is selected yet, Home and End go to the ends, Delete eats forward, and the
four chords do what they do. In one place, for all of them.

The one that is not shared is a cell that is merely chosen. A cell being
typed into is a text box; a cell that is not has no caret and no selection,
and any key that started a box for it took the arrow keys away from the
spreadsheet, which is what `Ctrl+C` over a grid did. Copy there means the
chosen *cells*, and that is the application's answer to give. **Shift with an
arrow there means one cell further**, and ends in the same `select-range` a
drag across the grid sends, with the far corner remembered between
keystrokes; a plain arrow is how it stops being a run. Over static text the
same chord reaches further along the words, out from wherever the press
landed.

**Words on a page are selectable too, and that was the half that was
missing.** `Layout::hit` only ever answers with controls, because that is
what a press acts on, so a press on a `text` element landed on nothing: a
field could always be selected in and an agent's answer, which is the thing
in this system most worth copying, could not. A run over static text is a
separate piece of ephemeral state (`Client::text_run`), because static text
is not a control: it has no value the application tracks, no caret, and
nothing that can be typed into it, and the compositor knows about it for one
reason, which is copying. `Layout::text_at` finds the words, `text_offset_at`
turns a point into a character using the same broken lines the paint uses,
and Ctrl+C is the whole of what a run answers to. A run stops at one element,
so a reply is selectable and a reply plus the label above it is not.

**Cut, copy and paste are on the other button, and that menu is the
compositor's.** It has to be: what is selected and what is on the clipboard
were never told to anyone. So the other button over a `field`, an `editor` or
any `text` opens the compositor's own Cut / Copy / Paste
(`haimanager/src/editmenu.rs`) and the application hears nothing, while over
a `cell` the press stays the application's, because copying over a grid means
*cells* and which cells are chosen is the application's state. A cell being
typed into is a field like any other and the compositor takes it back. The
menu is AWML through the same parser, layout and painter as everything else,
one open unlabelled `menu` hung from the press by the same `context_at` an
application's context menu uses, and what a press on it does is fed back
through `Client::handle` as the chord it stands for, so the menu is a second
way to say Ctrl+C and not a second implementation of it.

The bug worth remembering: the anchor is set on every press, because a
press is where a drag would start, and nothing cleared it when the button
came up without moving. The second character typed after a click then
deleted the first, since the caret had walked away from an anchor nobody
had dragged and the run between looked exactly like a selection to replace.
Typing "abc" in the pane and reading back "bc" is what found it; no unit
test would have, because it needs a click and two keystrokes in that order.

`width` and `height` attributes on a control are compositor-internal sizing
hints in physical pixels (the tab rename field keeps its tab's width; the
start menu's prompt is a large box), `icon` and `tile` on a button draw
an installed app's icon beside or above its label (the start menu's grid),
and `glyph` on a button draws a named stroke shape instead of a label
(`send` is the pane's paper plane, `stop` its square), because the shipped
font cannot be trusted to carry either. `enter-submits` on an editor swaps
Enter and Shift+Enter, so the pane's composer sends on Enter the way every
messenger does while a plain editor keeps Enter as a line break. None of
them is part of the application catalogue and none reaches an agent.

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
* **A cost that grows with use cannot be photographed.** Every capture starts
  from a fresh boot with an empty conversation, so the quadratic line
  breaking and the paint that walked scrolled-away nodes were invisible to
  the whole screenshot harness: the desk looked right and the numbers on a
  new boot were fine. What found them was measuring one function against
  size, which is what `bench_pane` (ignored, `--nocapture`) exists to do.
  It reports cold and warm separately for the same reason, and its lines are
  distinct per size, because a bench that shares strings between runs warms
  its own cache and reports a fix that is not there.
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
  agent script proved the old `not-visible` with a draft scrolled off the end of a list,
  which stopped being true the moment the display got bigger and the list fitted.
  There is no list length that works at every size. It now proves it with one
  window covering another, which is true at any size.
* **`/dev/input` is not fully populated at startup.** QEMU's PS/2 mouse appears
  about 300ms after the directory first has entries, so devices must be
  rescanned rather than enumerated once.
* **Verify at the resolution the machine actually runs.** `make run` is
  2560x1440, which is interface scale 1.5; captures default to smaller. The
  navigation bar was diffed to zero at 1600x1000 and was still wrong by
  33,192 pixels at 1440, because every constant written as `sc(n)` agreed
  with the old derived arithmetic only at 1.0. A check at one scale is not a
  check.
* **When collapsing two implementations into one, diff the pixels before
  and after.** Rebuilding the navigation bar on the `tabs` element changed
  3159 of its pixels on the first attempt and 33,192 at the real resolution,
  in ways nobody would report precisely: raised margins turned dark, tabs
  grew six pixels, the hairline moved a row, gradients shifted a step. Six
  rounds of diff-and-fix took it to zero at both scales. The diff is the
  oracle; a screenshot and an opinion are not.
* **Two implementations of the same look will not stay the same.** A row of
  tabs existed twice, in the navigation bar and as the `tabs` element, and
  every attempt to make the second look like the first was a guess that had
  to be checked a pixel at a time; three of them were wrong. The fix was not
  a better guess, it was deleting one of the implementations: the bar is
  built from the element now. If something must look like something else,
  make it be that thing.
* **Appearance questions are answered by reading pixels, not by looking.**
  Making an application's tab strip match the navigation bar took three
  wrong attempts, each of which looked plausible in a screenshot: the tabs
  were already pixel-identical, and the difference was entirely in what they
  stood on. Dumping a column of RGB values down each bar found it in one
  pass, and dumping a 2D map found the rest: the bar's tabs stand on
  `background` with thin `raised` edges above and below, not on a `raised`
  band. `tools/screenshot.py` plus a few lines that print colours by name is
  the tool; a crop viewed by eye is not.
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
