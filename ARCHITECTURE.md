# Agentware: System Architecture

Agentware is a highly modular, multi-process operating system userland written in Rust. It discards legacy Linux graphics and input stacks in favor of a bespoke, DOM-based UI rendering engine and a strict process isolation model.

This document covers what the processes are and how long they live. INTERACTIONS.md covers what they say to each other.

## Project Layout
The `agentwarecore` repository is structured to separate the boot-critical supervisor from the graphical and application subsystems:
* `supervisor/` - The PID 1 bare-metal init system and spawn broker.
* `haimanager/` - The Human-Agent Interface Manager: the window manager, markup parser, and renderer.
* `agentdesk/` - The workspace process. One per open agentdesk, owning that workspace's conversation history, its taskbar, and the opening of applications on its agent's behalf.
* `awkit/` - The application toolkit: what an application links besides the protocol crate. Models that render to AWML and accept events, for an application to embed in its own tree. Today: the file and folder dialogs, a yes-or-no, and a line-of-text prompt.
* `awsettings/` - The settings application. In this workspace rather than `agentwareapps` because what it edits is system state every workspace reads, but an ordinary application in every other way: it links the protocol crate and nothing else.
* `agent/` - The per-turn worker. Spawned to execute one prompt and gone when that prompt is finished. Not built yet; `awagent/` is a scripted stand-in.

There is no start menu process. The start menu is a panel the haimanager draws: the Agentware mark at the left end of the taskbar opens it, centred over the workspace, with a prompt that becomes a new agentdesk, a grid of every installed application, and the machine's power controls in its lower-left corner. It is chrome for the reasons the dock is (it needs the icons only the haimanager holds, it must vanish on a click anywhere else, and it must work when the workspace under it does not), and its requests go to the Supervisor the way the stop button's does. The plus at the end of the navigation bar's tabs creates an agentdesk with nothing to do; the haimanager asks for a blank one itself when it comes up and again whenever the last one closes, so the machine boots to a workspace and never shows a display with nothing on it.

First-party applications live in a second workspace, `agentwareapps`, beside `agentwarecore` rather than inside it. Apps are clients of the display protocol, not parts of the system: they link the protocol crate, and the toolkit when they need a dialog, and nothing else; the separate workspace makes that boundary a directory rather than a convention. `awcalc` is a calculator; `awfiles` is a file explorer, and the place the shared dialogs are seen at work.

An installed application is a folder, `/apps/<name>/`, holding four things: `exec`, the program the Supervisor forks; `icon.svg`, which the haimanager rasterizes for the window title bar and the dock (SVG so one file serves every display scale); `description.txt`, which the start menu carries in each tile's description and which will tell agents what an application is for; and `name.txt`, the name people see under the icon, falling back to the folder's. A folder with an `exec` but no description is not offered: that is what keeps the self-test's stand-ins, which ship in the same format, out of the start menu. The division of labour matches the rest of the system: the Supervisor touches only `exec` and stays out of content, and the haimanager reads the icon under the name the Supervisor stated at handoff, so an application cannot wear another's face.

The wallpapers that ship live in `/default_wallpapers/`, one SVG or PNG each. The settings application offers them in a dropdown, or any picture on the machine through the shared file dialog, and records the choice in `settings.xml` on the state volume, the one thing that crosses between processes on the filesystem rather than over a socket and the one thing that outlives a boot; every agentdesk stats it on its clock tick, re-reads it when it has changed, and names the picture in an `image` element in its background region, and the haimanager loads and fits it.

## The Supervisor (PID 1)
The Supervisor is the absolute root of the userland.
* Runs as process ID 1 immediately after the Linux kernel finishes booting.
* Responsible for hardware initialization, mounting virtual filesystems (`/dev`, `/proc`, `/sys`), and bootstrapping the `haimanager`.
* Monitors the health of all sub-processes and acts as the grim reaper for zombie processes to prevent resource leaks.
* Acts as the **spawn broker**: every agentdesk and every agent in the system is forked by the Supervisor, on request, and is therefore a direct child of PID 1.

The Supervisor deliberately stays small. It does not parse UI markup, touch the display, or know anything about what an agent is doing. It is the one process that is not permitted to crash, so it owns only what nothing else can: the process tree, signals, and shutdown.

## The Human-Agent Interface Manager
Agentware completely reimagines the display server. It does not use pixels or legacy framebuffers for input and application state.
* **Markup-Based UI:** Instead of pushing pixel arrays, applications output a special, proprietary UI markup language (similar to a DOM). 
* **Deterministic Rendering:** The haimanager parses this markup and natively renders it to the screen via the kernel's DRM/KMS subsystem.
* **Semantic Agent Input:** Because the UI is a structured DOM rather than a flat image, agents do not rely on fragile computer vision or pixel coordinates. They read the exact semantic structure of the UI and interact by targeting specific UI element IDs, driving the "fake cursor" visually to those exact nodes.
* **Serializable Workspaces:** Because a workspace is a markup tree rather than a framebuffer, its entire visual state can be written out and rebuilt exactly. This is what makes suspending and restoring an idle agentdesk tractable.
* **Sole Owner of I/O:** Keyboard, mouse and display belong to the haimanager alone. Nothing else in the userland opens `/dev/input` or `/dev/dri`. It also owns layout, window management, and the ephemeral UI state that apps deliberately do not track: focus, cursor position, scroll offset, selection.
* **The Boundary Agents Act Through:** An agent never produces an input event. It sends an *intent* naming a node, and the haimanager resolves it, checks the node is visible and enabled, moves the fake cursor there, and only then synthesizes the event a human would have produced. This is where the visible-embodiment promise is kept or lost.

## The Lifetime Model
The single most important structural decision in Agentware is that an agentdesk and an agent are not the same thing, and do not live for the same length of time. Three tiers, from longest lived to shortest:

| Tier | Lives | Owns |
| --- | --- | --- |
| **Agentdesk** | Until the human closes it, or until reboot | Identity, conversation history, open apps, workspace UI state |
| **Apps** | Until closed within their agentdesk | Their own documents and view state |
| **Agent** | One turn | Nothing durable |

No workspace outlives the machine. The entire userland runs from a RAM-backed initramfs, and a reboot wipes every agentdesk, its apps and its conversation. The system starts each boot with exactly one empty workspace. This is a deliberate simplification rather than an unfinished one, and it removes session restore and on-disk workspace state from the design entirely.

One thing does outlive the machine: settings. The Supervisor mounts a small state volume at `/state` right after the virtual filesystems (a virtio drive under QEMU, `state.img` in the repository; a partition on hardware, named by `agentware.state=` on the kernel command line until it can be found by label), and `settings.xml` lives there. A first run has no file and the first process to ask writes the defaults; from then on the file is the truth, written whole and atomically by the settings application and re-read by every agentdesk when its clock changes. Without a volume, `/state` is a directory in RAM and settings last until power off, and the boot log says so.

An agentdesk with no prompt in flight has **no agent process at all**. It is a workspace with apps in it. When a prompt arrives, whether at creation time or as a follow-up long afterwards, the Supervisor forks an agent, hands it the accumulated history plus the new message, and that process exits when the turn is complete.

The agent is therefore a stateless worker. This is not an optimization, it is what makes several hard promises cheap to keep:

* **Interruption is a signal.** VISION.md guarantees the human can stop an agent at any exact moment. With a long-lived agent that requires cancellation plumbed through every tool call. With a per-turn agent it is `SIGTERM` to a process that owns nothing. The workspace, apps, and history live elsewhere and survive untouched.
* **Crashes are contained.** An agent that dies mid-turn takes nothing with it. The agentdesk keeps its apps and history and surfaces an error the human can act on.
* **Agents never restart.** A failed agent is a *result* to report, not a process to resurrect. Restarting one would silently re-run side effects it had already performed, which is the opposite of what the user asked for.
* **Idle costs nothing.** A hundred open agentdesks cost a hundred workspaces and zero resident agent processes.

## Workspace Ownership and Teardown
Apps are per-workspace instances, not singletons. The same app may be open in a dozen agentdesks at once, and each one is a separate process belonging to exactly one workspace. Closing an agentdesk must therefore take its apps with it, or every closed workspace leaks processes for the lifetime of the machine.

The boundary that makes this reliable is a **cgroup per agentdesk**. The Supervisor creates one when it forks the workspace, and every app it later forks into that workspace is placed in the same cgroup. This is preferred over process groups because a process can leave a process group on its own (`setsid`, `setpgid`) but cannot leave its cgroup, so a misbehaving or double-forking app cannot escape the boundary that owns it.

Closing an agentdesk reuses the same escalation the Supervisor already applies at system shutdown, scoped to one workspace:

1. `SIGTERM` the workspace's agent, if a turn is running. Its work is disposable, so it goes first.
2. `SIGTERM` its apps, so they can flush open documents to the filesystem.
3. `SIGTERM` the agentdesk, so it can detach cleanly from `haimanager` and release its workspace. The reverse of how the workspace was built up.
4. After a grace period, `cgroup.kill` as the backstop, which terminates everything remaining in the cgroup atomically.
5. Reap. Every process involved is a direct child of PID 1, so the existing reaper collects them all with no special case.

Step 5 is the reason apps are forked by the Supervisor rather than by the agentdesk that requested them. If a workspace forked its own apps and was itself killed first, those apps would orphan mid-teardown and outlive the thing that owned them.

The per-agentdesk cgroup earns its keep twice more. It is where resource limits go, which is how a runaway agent is capped without affecting other workspaces. And it provides per-workspace memory accounting, which is exactly the signal needed to decide which idle agentdesk to suspend next.

## Workspace Suspension
Open agentdesks accumulate the way browser tabs do. An agentdesk that has not been viewed in a long time can have its app processes discarded entirely, with the workspace serialized to its markup tree and rebuilt on the human's return. The DOM-based rendering model is what makes this practical: restoring a suspended workspace is re-parsing markup, not replaying GPU state.

Suspension is a memory optimization, not a persistence mechanism. The serialized tree and the conversation history stay in RAM for as long as the machine is on, and die with it like everything else. The win is still large: a markup tree costs a fraction of the app processes and agent context it replaces, which is what lets a machine hold many more open agentdesks than it could hold live ones.

## The Spawn Broker
The haimanager and the agentdesks do not fork processes themselves. They ask the Supervisor over a control socket at `/run/agentware/sup.sock`:

```
CreateDesk { prompt: Option<text> } -> desk_id
OpenApp    { desk_id, app }         -> forks an app process into that workspace
CloseApp   { desk_id, pid }         -> ends one app, leaving the workspace open
StartAgent { desk_id }              -> forks a per-turn agent, returns a channel to it
Interrupt  { desk_id }              -> SIGTERM the desk's current agent
CloseDesk  { desk_id }              -> tear down the desk and everything in it
PowerOff   {}                       -> orderly shutdown, then power off
Reboot     {}                       -> orderly shutdown, then a fresh boot
```

`PowerOff` and `Reboot` come from the start menu's power controls. The
Supervisor does not act on them inline: each becomes the signal the machine's
own power button would send, picked up by the signalfd in the main loop, so
the one shutdown path serves the button, the keystroke and the request alike.

`CreateDesk` carries a prompt or nothing, matching the two ways a workspace is made: the start menu's prompt, and the navigation bar's plus. That opening prompt is the only piece of user text the Supervisor ever handles, and it exists solely because a brand new workspace has no other way to learn what it was created for. It is handed to the agentdesk, not to an agent.

Creating a workspace does not start a turn. The agentdesk reads its opening prompt and asks for an agent itself. `StartAgent` therefore carries no text at all: the Supervisor is told *that* a turn should run, never what it is about.

Instead it forks the agent with two pre-connected descriptors and returns the agentdesk's end of the private one on the reply, via `SCM_RIGHTS`. The agentdesk streams conversation history and the prompt down that channel itself, and telemetry comes back up the same one. History is deliberately not passed as an argument or as a path to a file: a socket has no cleanup problem, needs no filesystem capability once agents are namespaced, and is a channel the two processes need anyway.

There is one agent process per workspace at a time. This is a structural limit rather than a queueing policy: two agents doing computer use in one workspace would fight over the same cursor and the same DOM. A human message that arrives while a turn is running is queued by the agentdesk and delivered when the turn ends. It is never refused. To act on it sooner the human interrupts, which ends the turn and lets the queued message open the next one. All of this happens inside the agentdesk and is invisible to the Supervisor.

Apps never enter a workspace at creation time; they arrive later through `OpenApp`: from the haimanager when the human presses a tile in the start menu, or from the agentdesk when its agent asks for one over the turn channel.

`CloseApp` exists because a window's close button is the human ending one application, not the workspace. It is a separate escalation from `CloseDesk` and shares only the signal. The request comes from the haimanager rather than from the agentdesk, since window chrome is drawn by the compositor and the agentdesk is never told that windows exist. The pid is checked against that workspace's own apps rather than trusted, so no workspace can ask PID 1 to signal another's process.

Brokering through PID 1 rather than forking locally buys four things:

* **Process tree ownership.** If the compositor forked the workspaces its start menu and plus ask for, a crash of the compositor would orphan every open workspace to PID 1 with no record attached, leaving them unnameable and unmanageable. As children of the Supervisor they are first-class entries in its service table from the start.
* **Privilege separation.** Sandboxing an agent requires namespaces and cgroups, which require privilege. With the broker at PID 1, nothing that asks for a process needs any.
* **File descriptor passing.** Every agentdesk, app and agent is forked already holding a `socketpair` to the haimanager, and agents hold a second one to their agentdesk. Nothing opens a socket by path. This removes the startup race, keeps a sandboxed process from reaching anything it was not explicitly handed, and turns identity into a capability rather than a claim: the haimanager knows which workspace a connection belongs to because the Supervisor told it at handoff, which is what scopes an agent to its own desk and keeps the agentdesk's own chrome invisible to it.
* **One owner of lifetime.** The Supervisor already reaps and already tracks process state. A second spawner would mean two components tracking lifetime, and they would drift apart.

What deliberately does **not** cross this socket is agent telemetry. The stream of thoughts and tool calls that fills the side pane is high-volume application data and flows directly from the agent to its agentdesk. Every byte routed through PID 1 is a byte that can wedge the one process that must never wedge.

## Process Isolation & Lifecycle
Every major component in Agentware is strictly isolated in its own process space to guarantee system stability.
* **Agentdesk Processes:** Each open workspace is its own process, forked by the Supervisor on request and placed in its own cgroup along with its apps. If one crashes, gets stuck, or is closed, no other workspace is affected.
* **App Processes:** One per app *per workspace*. The same app open in several agentdesks is several independent processes, each owned by exactly one workspace and torn down with it.
* **Agent Processes:** Short-lived and frequent, one per turn. Because they perform computer use on a live workspace, they are the correct place to apply sandboxing (`CLONE_NEWPID`, `CLONE_NEWNS`, cgroup limits) as the isolation model matures. Nothing they need arrives by path, so a mount namespace costs them no capability they actually use.
* **Restart Policy by Kind:** `haimanager` restarts forever with exponential backoff, since the machine is unusable without it. Agentdesks and agents do not silently restart, because doing so would destroy conversation state or repeat work the human did not ask for twice.
