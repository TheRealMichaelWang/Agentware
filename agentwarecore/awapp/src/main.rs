//! A stand-in agentdesk: the reference client for the desk side of the display
//! protocol.
//!
//! A real agentdesk does not exist yet, and until it does nothing exercises the
//! desk connection: regions, the conversation pane, and the broker requests a
//! workspace makes on its own behalf. This is that path, written the way a
//! workspace is meant to be written, so the compositor is tested against the
//! contract rather than against a mock shaped to fit it.
//!
//! Applications are no longer stood in for here. The first real one lives in
//! `agentwareapps/awcalc`, and this workspace opens it at startup the way a
//! launcher would: by asking PID 1, never by forking.
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
//! **Its ids are written by hand and never change.** `message`, `send-message`.
//! They are what the human's caret is carried across. Generating them per frame
//! would break that, silently.
//!
//! Usage: awapp desk <id>

use std::fmt::Write as _;
use std::io::Write as IoWrite;

use awproto::broker::Broker;
use awproto::display::{self, Event, Surface};

struct App {
    /// Which workspace this process belongs to. A desk has to name it when it
    /// asks the broker for anything.
    desk: u32,
    message: String,
    transcript: Vec<String>,
    // Shown on screen so the app's own view of what happened can be compared
    // against the compositor's.
    status: String,
    /// Applications this workspace has open, for its taskbar.
    open: Vec<&'static str>,
    /// The control socket. An application never has one: apps are opened *into*
    /// a workspace by the workspace, and an app that could fork its own would be
    /// outside the boundary that owns it.
    broker: Option<Broker>,
}

fn main() {
    if std::env::args().nth(1).as_deref() != Some("desk") {
        log("usage: awapp desk <id>");
        std::process::exit(2);
    }

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

    let mut app = App::new(desk);

    // A workspace opens its own applications. That is what the launcher is, and
    // it is also the only way the taskbar can list them: nothing tells a
    // workspace what is running in it, because it is the thing that asked.
    app.launch("awcalc");

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
    fn new(desk: u32) -> Self {
        App {
            desk,
            message: String::new(),
            transcript: vec![
                "you: open the calculator and add two numbers".into(),
                "agent: opened Calculator, the display reads 0".into(),
            ],
            status: "ready".into(),
            open: Vec::new(),
            broker: None,
        }
    }

    /// Open an application into this workspace, and remember that it is open.
    fn launch(&mut self, name: &'static str) {
        let desk = self.desk;
        self.ask("open-app", move |broker| {
            broker.open_app(desk, name).map(|pid| format!("{name} opened as pid {pid}"))
        });
        if !self.open.contains(&name) {
            self.open.push(name);
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
            ("launch-calc", display::ACTION_CLICK) => self.launch("awcalc"),

            _ => return false,
        }
        true
    }

    /// The workspace shell, from the model, every time.
    ///
    /// The top-level nodes declare which region they belong to, and the
    /// compositor honours that only because this arrived on a desk connection.
    /// An application can write `region` into its markup and nothing will ever
    /// read it.
    ///
    /// There is no `region="apps"` here, and there cannot be. That one belongs
    /// to application processes; a workspace claiming it would be drawing over
    /// its own windows.
    ///
    /// Note what is absent: nothing here says which control has focus, where the
    /// caret is, or how far the transcript is scrolled. Those belong to the
    /// compositor, and an application that tried to describe them would be
    /// fighting it.
    fn render(&self) -> String {
        let mut out = String::new();
        let _ = write!(
            out,
            r##"<window title="Workspace {desk}" font="sans">
  <vstack region="pane" gap="sm">
    <text role="subheading">Conversation</text>
    <text role="caption" color="muted">{status}</text>
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
    <hstack gap="sm">
      <field id="message" grow="true" placeholder="Message the agent" value="{message}"
             description="Sends a message to the agent working in this workspace"/>
      <button id="send-message" label="Send" emphasis="primary"
              description="Sends the composed message, which starts an agent turn"/>
    </hstack>
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
/// A workspace has no console worth writing to: the haimanager owns the screen
/// and stdout goes to a terminal that is no longer being displayed.
fn log(message: &str) {
    let line = format!("<6>awapp: {message}\n");
    if let Ok(mut kmsg) = std::fs::OpenOptions::new().write(true).open("/dev/kmsg") {
        let _ = kmsg.write_all(line.as_bytes());
    } else {
        eprintln!("awapp: {message}");
    }
}
