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

1. Resolve the node ID to a screen rectangle.
2. Verify the node exists, is visible, is not covered, and is enabled.
3. Animate the fake cursor to it, so the human sees what is about to happen.
4. Synthesize exactly the event a human click would have produced.

If the agent could emit the event directly, every one of those steps would be skippable. An agent could "click" a disabled button, or one scrolled off screen, or one behind a dialog, and the app would receive something no human could have produced. The visible embodiment VISION.md promises would quietly stop being true.

Because the steps can fail, **intents are rejectable**: `no such node`, `node is disabled`, `node is not visible`, `the human has taken over`. An agent that cannot be told no acts blind and retries forever.

## The agentdesk and the haimanager

The agentdesk is not an app. It is the workspace itself, and **the agent cannot see it or drive it at all**.

It speaks the same AWML over the same kind of connection, and the haimanager runs it through the same parser and renderer. Two things make it different, and neither is a special case in the rendering path.

**Regions.** A workspace is divided into four, and the agentdesk's top-level nodes declare which one they belong to. The `region` attribute is honoured only on a desk connection.

| Region | Owner | Contents |
| --- | --- | --- |
| `background` | agentdesk | wallpaper |
| `taskbar` | agentdesk | open apps, launcher button |
| `pane` | agentdesk | chat transcript, input box, collapse toggle |
| `apps` | app processes | application windows |

Above all workspaces sits the navigation bar, drawn by the haimanager itself because it belongs to no workspace.

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

**A workspace.** `startmenu` sends `CreateDesk`, with the prompt the human typed or with nothing. The Supervisor forks the agentdesk, handing it a descriptor to the haimanager, and passes the opening prompt as an argument. That prompt is the only user text the Supervisor ever handles; it exists because a brand new workspace has no other way to learn what it was created for.

Creating a workspace does not start a turn. The agentdesk reads its opening prompt and asks for an agent itself.

**A turn.** The agentdesk sends `StartAgent { desk_id }`, carrying no text. The Supervisor forks the agent with two descriptors and returns the agentdesk's end of the private channel on the reply, attached via `SCM_RIGHTS`. The agentdesk then streams conversation history and the prompt down that channel directly.

History is not passed as an argument or as a path to a file. A socket has no cleanup problem, needs no filesystem capability once agents run in their own mount namespace, and is the same channel telemetry flows back up. One mechanism instead of three.

**An application.** Either the human or the agent opens one from the workspace's launcher; the agentdesk sends `OpenApp { desk_id, app }`. The Supervisor forks it into the workspace's cgroup and hands the haimanager its descriptor tagged `app-attached <desk> <app> <pid>`. That tag is what tells the haimanager which workspace to render it in and which agent is permitted to see it.

## Why the Supervisor is in the middle of all this

Every socket above is created by PID 1 and handed out at spawn. The Supervisor is never on the resulting connection and never sees a byte that crosses it: not markup, not events, not prompts, not conversation, not telemetry. It creates the pipe and steps out of the way.

That buys three things at once. There is no startup race, because the connection exists before either process does. There is no path for a sandboxed process to reach anything it was not handed, which is what makes mount-namespace isolation viable later. And identity is a capability rather than a claim, which is the enforcement mechanism behind both agent workspace scoping and agent invisibility of the desk.
