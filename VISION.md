# Agentware: OS Vision and User Experience

Agentware is an AI-native operating system built from the ground up to treat agents as first-class users. Instead of bolting AI onto a traditional desktop, Agentware provides a native environment where agents can fluidly interact with applications alongside the human user.

The core unit of the system is the **agentdesk**: a workspace holding one thread of work, the apps that work needs, and a conversation with an agent about it. Everything the user does happens inside one.

## The Start Menu
* **Start Work, Nothing Else:** The home screen is where work is *started*, not where it is done. It holds no apps and no workspace of its own. Pressing the windows key opens it from anywhere.
* **Summoned, Not Landed On:** The OS never boots to a menu. It starts inside a single empty agentdesk, so the machine opens on somewhere to work rather than a list of choices.
* **Launch by Prompt:** A single large bar sits at the center. Typing a prompt and pressing enter creates an agentdesk *with that prompt already in it*, so it opens on an agent that is already working: opening apps, moving its cursor, carrying out the task.
* **Launch Empty:** A second button creates an agentdesk with no prompt at all, a bare workspace for the human to work in themselves.
* **The Same Thing Either Way:** Both create an ordinary agentdesk. The only difference is whether it started with something to do.
* **The Open Set:** Every open agentdesk is listed here. This is how both the human and the agent move between workspaces.

## Agentdesks
* **One Workspace Per Thread of Work:** An agentdesk is an isolated desktop environment holding one task, one conversation, and however many apps that work needs. Multiple apps living together in one workspace is the whole point of it.
* **Apps Are Opened From Inside:** Each agentdesk carries its own app launcher listing everything installed, and opening one puts it in *that* workspace. This is the only way an app is ever launched, so there is exactly one action for "start something new" and exactly one for "add to what I am doing," and they live in different places.
* **First-Class Interaction:** Within its agentdesk, the agent operates exactly like a human user. It can freely open applications, navigate interfaces, and manage tasks.
* **Minimalist UI:** The agentdesk features a streamlined interface containing only its app launcher and a taskbar for switching between the apps open inside it. Either the human or the agent can open more at any time.
* **Global Navigation:** The human user can quickly move between the home screen and any open agentdesk using a persistent upper navigation bar.

## The Side Pane
* **Chat and Telemetry in One:** A pane docks to the right side of an agentdesk. It streams the agent's real-time thoughts, tool calls, and execution state, and it is also where the human types to the agent. Watching and talking happen in the same place.
* **Collapsible:** The pane can be collapsed at any time. Collapsed, an agentdesk is just a clean desktop with apps on it, which is what a human doing their own work should see.
* **A Conversation, Not a Job:** When the agent finishes a task it does not disappear from the conversation. The thread stays open, and the human can send a follow-up the way they would in a chat app. The work resumes in the same workspace, with the same apps still open and the same history behind it.
* **Quiet When Unused:** An agentdesk that has never been given a prompt has no agent thinking in the background. It is simply a desktop until the human asks for something.

## Human-Agent Collaboration
* **Visual Embodiment:** The agent possesses a visible, "fake" cursor that physically moves across the screen toward the specific UI DOM elements it is targeting and clicking, making its actions highly predictable and observable.
* **Absolute Human Authority:** The human user retains ultimate control. They can interrupt the agent at any exact moment, take over the mouse, provide corrections, or inject new instructions mid-task. Stopping an agent never costs the human anything: the workspace, the open apps, and the conversation all survive untouched.
* **A Shared Desk:** Because the human and the agent work in the same environment, either can pick up where the other left off. A human can open an app, do part of the job, then expand the pane and ask the agent to finish it, and the agent sees exactly the state the human left behind.

## Sessions
* **Work Stays Put While the Machine Is On:** An agentdesk lives until the human closes it. Switching away and coming back finds the same apps open, the same files on screen, and the same conversation ready to continue.
* **Idle Desks Get Out of the Way:** A workspace nobody has looked at in a long time quietly steps aside and rebuilds itself when the human returns to it, the way a browser handles a window full of tabs. Dozens of open agentdesks should cost no more than the one being looked at.
* **Closing Is Deliberate:** Nothing tidies itself away on the human's behalf. Work ends when the human ends it, and closing an agentdesk closes everything inside it: its apps, its conversation, its workspace.
* **A Reboot Is a Clean Slate:** Powering the machine off ends every agentdesk. It boots back into a single empty workspace with nothing carried over. Work in Agentware lasts as long as the machine stays on, and no longer.
