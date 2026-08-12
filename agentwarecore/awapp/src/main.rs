//! A stand-in application: the reference client for the display protocol.
//!
//! Real applications do not exist yet, and until they do nothing exercises the
//! path a milestone-5 haimanager is built around. This is that path, written the
//! way an application is meant to be written, so the compositor is being tested
//! against the contract rather than against a mock shaped to fit it.
//!
//! Three things are worth noticing about how little it does.
//!
//! **It has a model and a `render`, and nothing in between.** Every change to
//! the model resends the entire interface. There is no diffing here, no dirty
//! tracking, no retained widget tree. The compositor works out what changed.
//!
//! **It never tracks focus, the caret, or scroll position.** Those never appear
//! in what it sends and never arrive in what it receives. A resend does not move
//! the human's cursor, and this file contains no code to make sure of that,
//! which is the entire point of putting that state in the compositor.
//!
//! **Its ids are written by hand and never change.** `to`, `body`, `send`. They
//! are what the human's caret is carried across, and what an agent will name to
//! act. Generating them per frame would break both, silently.
//!
//! Which face it wears comes from the name it was invoked under, because the
//! spawn broker forks an application by name with no arguments: `awapp` is the
//! compose window, `awnotes` is a second one so windows overlap. The agentdesk
//! is the exception, since the supervisor does pass a workspace id to a desk.
//!
//! Usage: awapp | awnotes | awapp desk <id>

use std::fmt::Write as _;
use std::io::Write as IoWrite;

use awproto::broker::Broker;
use awproto::display::{self, Event, Surface};

/// Which interface this process is standing in for.
///
/// The supervisor forks the same binary for both, because what a connection is
/// allowed to do is decided by the descriptor it was handed, not by the program
/// on the other end of it.
#[derive(PartialEq, Eq)]
enum Face {
    /// An application window: the thing an agent will read and drive.
    Compose,
    /// A second application, so a workspace holds more than one window and
    /// covering is a real case rather than a hypothetical one.
    Notes,
    /// The workspace shell. Chrome, and invisible to agents by construction.
    Workspace,
}

struct Draft {
    id: &'static str,
    label: &'static str,
    archived: bool,
}

struct App {
    face: Face,
    /// Which workspace this process belongs to. Only a desk has one.
    desk: u32,
    // The compose window's model.
    to: String,
    copy_self: bool,
    body: String,
    drafts: Vec<Draft>,
    selected: Option<usize>,
    // The workspace shell's model.
    message: String,
    transcript: Vec<String>,
    // Shown on screen so the app's own view of what happened can be compared
    // against the compositor's.
    status: String,
    /// The control socket, for the workspace face only. An application never
    /// has one: apps are opened *into* a workspace by the workspace, and an app
    /// that could fork its own would be outside the boundary that owns it.
    broker: Option<Broker>,
}

fn main() {
    let program = std::env::args().next().unwrap_or_default();
    let face = match std::env::args().nth(1).as_deref() {
        Some("desk") => Face::Workspace,
        _ if program.ends_with("awnotes") => Face::Notes,
        _ => Face::Compose,
    };

    // A desk is told which workspace it is, because it has to name it when it
    // asks the broker for anything. An application is told nothing: it does not
    // know which workspace it is in and has no use for the answer.
    let desk: u32 = std::env::args().nth(2).and_then(|id| id.parse().ok()).unwrap_or(0);

    let mut surface = match Surface::inherited() {
        Ok(surface) => surface,
        Err(err) => {
            log(&format!("no interface connection: {err}"));
            std::process::exit(1);
        }
    };

    let mut app = App::new(face, desk);
    if let Err(err) = surface.render(&app.render()) {
        log(&format!("could not send the first tree: {err}"));
        std::process::exit(1);
    }

    loop {
        let event = match surface.next_event() {
            Ok(Some(event)) => event,
            // The compositor hung up. There is nothing left to draw on, and
            // nothing to save: this process owns no state anyone else wants.
            Ok(None) => {
                log("the compositor closed the connection");
                return;
            }
            Err(err) => {
                log(&format!("connection failed: {err}"));
                std::process::exit(1);
            }
        };

        if !app.accept(&surface, &event) {
            continue;
        }

        if let Err(err) = surface.render(&app.render()) {
            log(&format!("could not send a tree: {err}"));
            return;
        }
    }
}

impl App {
    fn new(face: Face, desk: u32) -> Self {
        App {
            face,
            desk,
            to: String::new(),
            copy_self: true,
            body: String::new(),
            drafts: vec![
                Draft { id: "draft-1", label: "Notes from Tuesday", archived: false },
                Draft { id: "draft-2", label: "Re: budget", archived: false },
                Draft { id: "draft-3", label: "Holiday plans", archived: true },
                Draft { id: "draft-4", label: "Conference travel", archived: false },
                Draft { id: "draft-5", label: "Re: kitchen rota", archived: false },
                Draft { id: "draft-6", label: "Landlord, again", archived: false },
                Draft { id: "draft-7", label: "Reading list", archived: false },
                Draft { id: "draft-8", label: "Invoice 0041", archived: true },
                Draft { id: "draft-9", label: "Sunday", archived: false },
                Draft { id: "draft-10", label: "Re: the thing", archived: false },
            ],
            selected: Some(1),
            message: String::new(),
            transcript: vec![
                "you: open the mail app and start a message".into(),
                "agent: opened Messages, the recipient field is empty".into(),
            ],
            status: "ready".into(),
            broker: None,
        }
    }

    /// Ask PID 1 for something, connecting the first time it is needed.
    ///
    /// This is the agentdesk's job and nobody else's in this file. It is by path
    /// because a process that has not been forked yet cannot have been handed a
    /// descriptor to the thing that will fork it.
    fn ask(&mut self, what: &str, request: impl FnOnce(&mut Broker) -> Result<String, String>) {
        if self.broker.is_none() {
            match Broker::connect() {
                Ok(broker) => self.broker = Some(broker),
                Err(err) => {
                    self.status = format!("cannot reach the supervisor: {err}");
                    log(&self.status);
                    return;
                }
            }
        }

        let Some(broker) = &mut self.broker else { return };
        match request(broker) {
            Ok(note) => self.status = note,
            Err(err) => self.status = format!("{what} refused: {err}"),
        }
        log(&self.status);
    }

    /// Decide whether an event should be acted on, and act on it.
    ///
    /// The version check is the whole reason events carry one. A click names a
    /// node and nothing else, so its meaning comes entirely from the tree it was
    /// generated against; if that tree is gone, so is the meaning, and acting on
    /// it would perform something the human did not ask for. Typed text is
    /// different: the payload *is* the intention, so a late one is still true.
    fn accept(&mut self, surface: &Surface, event: &Event) -> bool {
        if surface.is_stale(event) && event.action == display::ACTION_CLICK {
            self.status = format!(
                "discarded a click on {} against v{}, now on v{}",
                event.target,
                event.version,
                surface.version()
            );
            log(&self.status);
            return true;
        }

        match self.face {
            Face::Compose | Face::Notes => self.compose_event(event),
            Face::Workspace => self.workspace_event(event),
        }
    }

    fn compose_event(&mut self, event: &Event) -> bool {
        match (event.target.as_str(), event.action.as_str()) {
            ("to", display::ACTION_TYPE_TEXT) => {
                self.to = event.value.clone();
                self.status = "recipient edited".into();
            }
            ("to", display::ACTION_SUBMIT) => self.status = "recipient confirmed".into(),

            ("body", display::ACTION_TYPE_TEXT) => {
                self.body = event.value.clone();
                self.status = format!("{} characters of body", self.body.chars().count());
            }

            ("copy-self", display::ACTION_TOGGLE) => {
                self.copy_self = !self.copy_self;
                self.status = format!("copy to self is {}", self.copy_self);
            }

            ("send", display::ACTION_CLICK) => {
                self.status = format!("sent to {}", self.to);
                self.to.clear();
                self.body.clear();
            }

            ("discard", display::ACTION_CLICK) => {
                self.to.clear();
                self.body.clear();
                self.status = "draft discarded".into();
            }

            (target, display::ACTION_CLICK) => {
                match self.drafts.iter().position(|draft| draft.id == target) {
                    Some(at) => {
                        self.selected = Some(at);
                        self.status = format!("opened {}", self.drafts[at].label);
                    }
                    None => {
                        self.status = format!("nothing here answers to {target}");
                        return false;
                    }
                }
            }

            _ => {
                self.status = format!("ignored {} on {}", event.action, event.target);
                return false;
            }
        }
        true
    }

    fn workspace_event(&mut self, event: &Event) -> bool {
        let desk = self.desk;

        match (event.target.as_str(), event.action.as_str()) {
            ("message", display::ACTION_TYPE_TEXT) => {
                self.message = event.value.clone();
                self.status = "composing".into();
            }

            ("message", display::ACTION_SUBMIT) | ("send-message", display::ACTION_CLICK) => {
                if self.message.trim().is_empty() {
                    self.status = "nothing to send".into();
                    return false;
                }
                self.transcript.push(format!("you: {}", self.message));
                self.message.clear();

                // A message is what starts a turn, and the agentdesk is what
                // decides that, not the supervisor and not the start menu.
                self.ask("start-agent", move |broker| {
                    broker
                        .start_agent(desk)
                        .map(|pid| format!("agent started as pid {pid}"))
                });
                let note = self.status.clone();
                self.transcript.push(format!("system: {note}"));
            }

            // The launcher. Apps are forked by PID 1 into this workspace's
            // cgroup, so closing the workspace takes them with it.
            ("launch-notes", display::ACTION_CLICK) => {
                self.ask("open-app", move |broker| {
                    broker
                        .open_app(desk, "awnotes")
                        .map(|pid| format!("Notes opened as pid {pid}"))
                });
            }

            _ => return false,
        }
        true
    }

    fn render(&self) -> String {
        match self.face {
            Face::Compose | Face::Notes => self.render_compose(),
            Face::Workspace => self.render_workspace(),
        }
    }

    /// The whole interface, from the model, every time.
    ///
    /// Note what is absent: nothing here says which control has focus, where the
    /// caret is, or how far the draft list is scrolled. Those belong to the
    /// compositor, and an application that tried to describe them would be
    /// fighting it.
    fn render_compose(&self) -> String {
        let mut out = String::new();
        let _ = write!(
            out,
            r#"<window title="{title}" font="sans">
  <vstack gap="lg" grow="true">
    <text role="heading">{title}</text>
    <text role="caption" color="muted">{status}</text>

    <group label="Recipient">
      <field id="to" placeholder="name@example.com" value="{to}"
             description="Address the message will be sent to"/>
      <checkbox id="copy-self" label="Send me a copy"{copy}
                description="Also deliver this message to your own inbox"/>
    </group>

    <group label="Message">
      <editor id="body" placeholder="Write something" value="{body}"
              description="Body text of the message being composed"/>
    </group>

    <hstack gap="sm">
      <button id="send" label="Send" emphasis="primary"{send}
              description="Sends the composed message to its recipient"/>
      <button id="discard" label="Discard" emphasis="danger"
              description="Throws away the draft without sending it"/>
      <text grow="true" color="muted">tab moves focus, the wheel scrolls the list</text>
    </hstack>

    <divider/>

    <scroll grow="true">
      <list id="drafts" label="Saved drafts">
"#,
            title = if self.face == Face::Notes { "Notes" } else { "Compose" },
            status = display::escape(&self.status),
            to = display::escape(&self.to),
            body = display::escape(&self.body),
            copy = if self.copy_self { r#" checked="true""# } else { "" },
            // The button turns itself off when there is nowhere to send to. The
            // agent sees the same thing the human does: an element that exists,
            // says what it would do, and currently offers no actions.
            send = if self.to.trim().is_empty() { " disabled" } else { "" },
        );

        for (at, draft) in self.drafts.iter().enumerate() {
            let _ = write!(
                out,
                r#"        <item id="{id}" label="{label}"{selected}{disabled}
              description="Open the draft named {label}"/>
"#,
                id = draft.id,
                label = display::escape(draft.label),
                selected = if self.selected == Some(at) { r#" selected="true""# } else { "" },
                disabled = if draft.archived { " disabled" } else { "" },
            );
        }

        out.push_str("      </list>\n    </scroll>\n  </vstack>\n</window>\n");
        out
    }

    /// A stand-in for the workspace shell.
    ///
    /// The top-level nodes declare which region they belong to, and the
    /// compositor honours that only because this arrived on a desk connection.
    /// An application can write `region` into its markup and nothing will ever
    /// read it.
    ///
    /// There is no `region="apps"` here, and there cannot be. That one belongs
    /// to application processes; a workspace claiming it would be drawing over
    /// its own windows.
    fn render_workspace(&self) -> String {
        let mut out = String::new();
        let _ = write!(
            out,
            r##"<window title="Workspace {desk}" font="sans">
  <vstack region="background">
    <text role="heading" color="#222c40">agentware</text>
    <text role="caption" color="#1c2436">workspace {desk}</text>
  </vstack>

  <hstack region="taskbar" gap="sm">
    <button id="launch-notes" label="Open Notes"
            description="Opens the Notes application in this workspace"/>
    <text grow="true" color="muted">taskbar</text>
    <text color="muted">{status}</text>
  </hstack>

  <vstack region="pane" gap="sm">
    <text role="subheading">Conversation</text>
    <scroll grow="true">
      <vstack gap="sm">
"##,
            desk = self.desk,
            status = display::escape(&self.status),
        );

        for line in &self.transcript {
            let _ = writeln!(out, "        <text>{}</text>", display::escape(line));
        }

        let _ = write!(
            out,
            r##"      </vstack>
    </scroll>
    <field id="message" placeholder="Message the agent" value="{message}"
           description="Sends a message to the agent working in this workspace"/>
    <button id="send-message" label="Send" emphasis="primary"
            description="Sends the composed message, which starts an agent turn"/>
  </vstack>
</window>
"##,
            message = display::escape(&self.message),
        );
        out
    }
}

/// Log to the kernel ring buffer.
///
/// An application has no console worth writing to: the haimanager owns the
/// screen and stdout goes to a terminal that is no longer being displayed.
fn log(message: &str) {
    let line = format!("<6>awapp: {message}\n");
    if let Ok(mut kmsg) = std::fs::OpenOptions::new().write(true).open("/dev/kmsg") {
        let _ = kmsg.write_all(line.as_bytes());
    } else {
        eprintln!("awapp: {message}");
    }
}
