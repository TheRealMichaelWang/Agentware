# Agentware: Interaction Model

ARCHITECTURE.md describes what the processes are and how long they live. This describes what they say to each other.

Every connection in this document is a Unix socketpair created by the Supervisor before either endpoint existed, and handed over at spawn. No process in Agentware ever opens a socket by path. That single decision is what makes the rest of the model enforceable: a process can only talk to what it was given, and it cannot forge a claim about who it is, because it never makes one.

## The Connection Graph

```
                          ┌─────────────┐
                          │  Supervisor │  control socket, path-addressed
                          │   (PID 1)   │  the one exception
                          └──────┬──────┘
                 spawns, and hands out every socket below
      ┌──────────────┬───────────┼────────────┬──────────────┐
      │              │           │            │              │
 ┌────▼─────┐  ┌─────▼─────┐  ┌──▼───┐   ┌────▼────┐   ┌─────▼─────┐
 │ desktop- │  │ agentdesk │  │ app  │   │  agent  │   │ haimanager│
 │   main   │  │           │  │      │   │         │   │           │
 └────┬─────┘  └─────┬─────┘  └──┬───┘   └──┬───┬──┘   └─────▲─────┘
      │              │           │          │   │            │
      └──────────────┴───────────┴──────────┴───┼────────────┘
                  all display traffic           │
                                                │
                            agentdesk ◄─────────┘
                        history down, telemetry up
```

Four connection kinds, three of them handed out at spawn:

| Connection | Created by | Carries |
| --- | --- | --- |
| anything → Supervisor | connect by path | spawn and lifecycle requests |
| agentdesk / app / agent → haimanager | `socketpair` at spawn | markup, events, queries, intents |
| agent ↔ agentdesk | `socketpair` at spawn | conversation history down, telemetry up |
| Supervisor → haimanager | registration | descriptors for newly spawned processes |

The environment variables naming the inherited descriptors are `AGENTWARE_HAI_FD` (every process that draws) and `AGENTWARE_DESK_FD` (agents only).

## Applications and the haimanager

**App → haimanager: the whole tree, every time.**

An app describes its current interface as AWML and sends it. It does not send patches, does not track what it sent last, and does not diff. The haimanager diffs the new tree against the one it is holding and repaints only what changed.

This makes apps immediate-mode and trivially correct: describe the present state, done. The alternative, where apps compute deltas, makes every app author reimplement diffing and introduces a failure that cannot be detected — one misapplied patch and the app's model and the haimanager's model diverge silently, with no resync path. A two hundred node tree over a Unix socket costs nothing. If a thousand-row table ever proves otherwise, patches can be added for specific subtrees as an optimization, once there is a measurement to justify them.

**haimanager → app: one event at a time.**

A click, a text submission, a key combination. Each event names the node it happened to and the **version of the tree it was generated against**. Without that version there is a race: the app sends tree v1, the human clicks, the app has already sent v2 in which that node means something else, and the event arrives describing an intention the human never had. The version lets the app discard a stale event instead of acting on it.

**The haimanager owns ephemeral UI state.**

Focus, cursor position within a text field, scroll offset, selection. These are *not* in the tree the app sends. If they were, a full-tree resend would reset the human's cursor on every keystroke, and every app would reimplement their preservation slightly differently.

This is what makes stable IDs load-bearing rather than a nicety. The haimanager matches old node to new node by ID across a re-render and carries ephemeral state across. IDs are assigned by the app and must be stable; regenerating them per frame silently moves the human's cursor and the agent's target.

## Agents and the haimanager

An agent has exactly two channels, and neither one is the Supervisor.

**Agent → haimanager: queries.**

* *What applications are open?* Answered as AWML.
* *What is the state of application X?* Answered as AWML in a reduced schema: element IDs, semantic roles, labels, and declared state, with pure layout containers omitted. The model does not need to know that two buttons are in a row; it needs to know both exist, what they do, and whether they are enabled.

Both answers are **scoped to the agent's own agentdesk**. An agent cannot see or address an app in another workspace, and this is not enforced by checking a workspace id the agent supplies. The haimanager knows which workspace a connection belongs to because the Supervisor told it so when it handed the descriptor over. The agent is never asked and cannot lie.

**Agent → haimanager: intents, not events.**

This is the distinction the whole input model rests on.

* An **intent** is what an agent sends: "click node `send`".
* An **event** is what an app receives: "a click occurred on node `send`".

They are different schemas and the agent may only produce the first. The haimanager turns one into the other, and in between does everything that makes the action legitimate:

1. Arrange the stage: the target's window comes to the front maximized, and the workspace's other windows are minimized. An agent names controls, never windows, so whether its target was covered is a question the haimanager makes impossible rather than one it answers with rejections. Only the human arranges and resizes windows; no intent exists for either.
2. Resolve the node ID to a screen rectangle.
3. Verify the node exists, is visible within its own window, and is enabled.
4. Animate the fake cursor to it, so the human sees what is about to happen.
5. Synthesize exactly the event a human click would have produced.

Step 4 is more literal than it sounds. Text is entered one character at a time, at roughly a keystroke's interval, so an application receives seventeen events for a seventeen character address exactly as it would from a person. Setting the value in one step would produce something no human could have produced, and would also be unreadable to a human watching. `check` and `uncheck` become a `toggle` only when the state actually has to move, for the same reason: that is the event a person pressing the box would have generated.

If the agent could emit the event directly, every one of those steps would be skippable. An agent could "click" a disabled button, or one scrolled off screen, or one behind a dialog, and the app would receive something no human could have produced. The visible embodiment VISION.md promises would quietly stop being true.

Because the steps can fail, **intents are rejectable**: `no such node`, `node is disabled`, `node is not visible`, `blocked` (behind a dialog the application has open), `the human has taken over`. An agent that cannot be told no acts blind and retries forever.

**Dialogs are part of the application's tree**, and so of its view. An application that needs a choice made, a file to open or a name to save under, renders a `dialog` among its own nodes and takes it out when the answer is in; the compositor floats it over the window and makes everything outside it inert for the human and the agent alike, and the agent sees it as controls nested in `<dialog>`, nothing more. The dialogs every application shares come from `awkit`, a toolkit crate applications link beside the protocol crate: file and folder dialogs, a yes-or-no, and a line-of-text prompt, each a model that renders one `dialog` and answers the events that name its ids, so choosing a file, confirming a deletion or naming a folder is the same clicks in every application. It reads the filesystem in the application's own process, because nothing is namespaced yet; on the day it is, the same markup can front a picker the compositor brokers, handing back a descriptor rather than a path.

## The agentdesk and the haimanager

The agentdesk is not an app. It is the workspace itself, and **the agent cannot see it or drive it at all**.

It speaks the same AWML over the same kind of connection, and the haimanager runs it through the same parser and renderer. Two things make it different, and neither is a special case in the rendering path.

**Regions.** A workspace is divided into four, and the agentdesk's top-level nodes declare which one they belong to. The `region` attribute is honoured only on a desk connection.

| Region | Owner | Contents |
| --- | --- | --- |
| `background` | agentdesk | wallpaper |
| `taskbar` | agentdesk | the clock and date, at the right; the compositor's start button and dock share the band |
| `pane` | agentdesk | chat transcript, input box |
| `apps` | app processes | application windows |

Above all workspaces sits the navigation bar, drawn by the haimanager itself because it belongs to no workspace.

**Window chrome is the compositor's, not the application's.** The title bar, the shadow, and the window controls are drawn by the haimanager around a client, from the `title` the application declared. The controls are stroke glyphs at the bar's right end, quiet until hovered: a chevron pointing down at the dock the window will join, corner brackets that push outward and flip inward once there is nowhere further to go, and a cross. They are the only controls in the system that are not AWML, and that is the point: putting them in the tree would let every application decide whether it was closable, and would make an agent's view of a window include the button that destroys it.

Every open window appears as an icon tile in a dock centred in the taskbar band, drawn by the compositor from the `icon.svg` in the app's package: a window on screen carries a dot under its tile, and an app without an icon shows its initial. The same icon leads the window's title bar. The dock is compositor chrome even though it sits in the agentdesk's band, because switching between windows is what a dock is for and which window is where is not something the agentdesk is told. At the left end of the same band the compositor draws the start button, the Agentware mark, which opens the start menu: a panel centred over the workspace holding a prompt that becomes a new agentdesk and a grid of every installed application, closed by a click anywhere else. The agentdesk keeps both ends of its taskbar clear for these the way it leaves the title bars alone: by design, not by protocol. What the agentdesk draws in the band is what it does own: the clock and date, at the far right.

The `background` region holds the wallpaper, an `image` element naming a file the compositor loads and fits. It is the one region laid out without breathing room, because a wallpaper reaches the edges. Which file is a setting: the settings application writes it to `settings.xml` on the state volume, and every agentdesk stats that file once a second alongside its clock and re-reads it when it has changed. No process is told and nothing is broadcast; the same tick that moves the clock notices the setting, and the setting is there again after a reboot.

**Collapsing the pane is the compositor's too.** The grip that folds it away is drawn on its edge and the width of the region is compositor geometry. The reasoning is the stop button's: the pane is a fifth of the screen, and a wedged agentdesk must not be able to keep it. An agentdesk that drew its own toggle would mean a human who cannot reclaim their own display.

**Invisibility to agents is a property of the connection, not an attribute.** Trees arriving on a desk connection are chrome. They are excluded from every agent-facing query by construction, and intents naming their node IDs are rejected. Nothing is marked; nothing can be marked wrongly or forgotten. A useful consequence: an agent cannot read the chat pane containing its own streamed thoughts, which would otherwise be a feedback loop.

### The stop button does not belong to the agentdesk

The stop button is drawn by the haimanager, and clicking it sends `Interrupt` to the **Supervisor**, which SIGTERMs the agent process.

It deliberately does not pass through the agentdesk. The agentdesk is the process most likely to be busy at exactly the moment the human wants to stop something: it is streaming telemetry from the agent, managing apps, and rendering a growing transcript. If it drew the button and received its click, a wedged agentdesk would mean a human who cannot stop a running agent. VISION.md's "absolute human authority" would hold only while everything else was healthy, which is when it is least needed.

Routing to the Supervisor costs nothing extra. The haimanager is already a registered client of that socket and `Interrupt` already exists.

### Input arbitration

While an agent is running a turn, the human cannot click into that workspace's `apps` region. Two processes driving the same cursor and the same DOM is the failure mode this prevents.

The freeze is scoped to that one region, and specifically **not** to the screen:

* The **navigation bar** stays live. An agent working in one workspace must not trap the human inside it; other workspaces are independent and switching between them has no bearing on the turn.
* The **stop button** stays live, or the freeze is a trap rather than a safety measure.
* The **chat input** stays live. A message arriving mid-turn is queued, and freezing the box would make that decision unreachable. Typing goes to the agentdesk, never to the app the agent is driving.
* **Scrolling the transcript** stays live, being read-only.

## Starting work

The Supervisor is the only path by which a process comes into existence.

**A workspace.** The haimanager sends `CreateDesk`: from the start menu with the prompt the human typed, from the plus at the end of the navigation bar's tabs with none, and with none at startup and whenever the last workspace closes, so there is always one. The Supervisor forks the agentdesk, handing it a descriptor to the haimanager, and passes the opening prompt as an argument. That prompt is the only user text the Supervisor ever handles; it exists because a brand new workspace has no other way to learn what it was created for.

Creating a workspace does not start a turn. The agentdesk reads its opening prompt and asks for an agent itself.

**A turn.** The agentdesk sends `StartAgent { desk_id }`, carrying no text. The Supervisor forks the agent with two descriptors and returns the agentdesk's end of the private channel on the reply, attached via `SCM_RIGHTS`. The agentdesk then streams conversation history and the prompt down that channel directly.

History is not passed as an argument or as a path to a file. A socket has no cleanup problem, needs no filesystem capability once agents run in their own mount namespace, and is the same channel telemetry flows back up. One mechanism instead of three.

The channel's vocabulary is small. Down: `history <role> <text>` for every earlier message, then `prompt <text>` for the one that starts the turn. Up: `telemetry <kind> <text>` for anything the human should watch as it happens (a thought, an action, a result, an error), `open-app <name>` to ask the workspace to open an application, since the agent has no broker connection and no way to get one, and finally `reply <text>`, the turn's answer, which joins the conversation. The turn is over when the agent hangs up, whether it finished, failed or was interrupted, so every ending looks the same to the agentdesk and none needs a message of its own. A message the human sends while a turn is running is queued and starts the next turn the moment this one ends.

**An application.** The human opens one from the start menu, and the haimanager sends `OpenApp { desk_id, app }` for the workspace on screen; an agent asks its agentdesk over the turn channel, and the agentdesk sends the same request. The Supervisor forks it into the workspace's cgroup and hands the haimanager its descriptor tagged `app-attached <desk> <app> <pid>`. That tag is what tells the haimanager which workspace to render it in and which agent is permitted to see it.

## Why the Supervisor is in the middle of all this

Every socket above is created by PID 1 and handed out at spawn. The Supervisor is never on the resulting connection and never sees a byte that crosses it: not markup, not events, not prompts, not conversation, not telemetry. It creates the pipe and steps out of the way.

That buys three things at once. There is no startup race, because the connection exists before either process does. There is no path for a sandboxed process to reach anything it was not handed, which is what makes mount-namespace isolation viable later. And identity is a capability rather than a claim, which is the enforcement mechanism behind both agent workspace scoping and agent invisibility of the desk.
