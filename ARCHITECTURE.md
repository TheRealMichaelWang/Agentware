# Agentware: System Architecture

Agentware is a highly modular, multi-process operating system userland written in Rust. It discards legacy Linux graphics and input stacks in favor of a bespoke, DOM-based UI rendering engine and a strict process isolation model.

## Project Layout
The `agentwarecore` repository is structured to separate the boot-critical supervisor from the graphical and application subsystems:
* `supervisor/` - The PID 1 bare-metal init system.
* `ui-manager/` - The core window manager and DOM parsing engine.
* `desktop-main/` - The primary human-facing shell and prompt interface.
* `agentdesk/` - The isolated desktop environment spun up for individual agents.
* `apps/` - First-party system applications natively compatible with the UI markup language.

## The Supervisor (PID 1)
The Supervisor is the absolute root of the userland.
* Runs as process ID 1 immediately after the Linux kernel finishes booting.
* Responsible for hardware initialization, mounting virtual filesystems (`/dev`, `/proc`, `/sys`), and bootstrapping the `ui-manager` and `desktop-main` processes.
* Monitors the health of all sub-processes and acts as the grim reaper for zombie processes to prevent resource leaks.

## UI Manager & Window Manager
Agentware completely reimagines the display server. It does not use pixels or legacy framebuffers for input and application state.
* **Markup-Based UI:** Instead of pushing pixel arrays, applications output a special, proprietary UI markup language (similar to a DOM). 
* **Deterministic Rendering:** The UI Manager parses this markup and natively renders it to the screen via the kernel's DRM/KMS subsystem.
* **Semantic Agent Input:** Because the UI is a structured DOM rather than a flat image, agents do not rely on fragile computer vision or pixel coordinates. They read the exact semantic structure of the UI and interact by targeting specific UI element IDs, driving the "fake cursor" visually to those exact nodes.

## Process Isolation & Lifecycle
Every major component in Agentware is strictly isolated in its own process space to guarantee system stability.
* **Main Desktop Process:** The primary desktop (containing the central prompt bar) runs as a persistent, standalone process directly under the Supervisor.
* **Agent and Agentdesk Processes:** When a human issues a prompt, the system forks entirely new, isolated processes for both the AI Agent and its corresponding `agentdesk` graphical shell. 
* **Sandboxed Execution:** If an agent crashes, gets stuck in a loop, or fails, its specific `agentdesk` process can be killed and restarted by the supervisor without affecting the main desktop or any other agents running in parallel.