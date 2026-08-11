# Agentware: OS Vision and User Experience

Agentware is an AI-native operating system built from the ground up to treat agents as first-class users. Instead of bolting AI onto a traditional desktop, Agentware provides a native environment where agents can fluidly interact with applications alongside the human user.

## The Main Desktop
* **The Command Center:** The human user's primary interface is the main desktop, featuring a large, centralized prompt bar.
* **Frictionless Tasking:** When the user types a prompt into this bar and hits enter, the OS instantly spawns a new agent to execute the task. 
* **Seamless Transitions:** Upon spawning an agent, the OS automatically creates a dedicated workspace and seamlessly switches the user's view directly to it.

## Agentdesks
* **Dedicated Workspaces:** Every agent is isolated in its own dedicated desktop environment called an **agentdesk**.
* **First-Class Interaction:** Within its agentdesk, the agent operates exactly like a human user—it can freely open applications, navigate interfaces, and manage tasks.
* **Minimalist UI:** The agentdesk features a streamlined interface containing only a start menu and a taskbar for switching between open apps.
* **Global Navigation:** The human user can quickly navigate between their main desktop and any active agentdesks using a persistent upper navigation bar.

## Human-Agent Collaboration
* **Live Telemetry:** When a human opens and views an active agentdesk, an information pane docks to the right side of the screen. This pane streams the agent's real-time thoughts, tool calls, and execution state.
* **Visual Embodiment:** The agent possesses a visible, "fake" cursor that physically moves across the screen toward the specific UI DOM elements it is targeting and clicking, making its actions highly predictable and observable.
* **Absolute Human Authority:** The human user retains ultimate control. They can interrupt the agent at any exact moment, take over the mouse, provide corrections, or inject new instructions mid-task.