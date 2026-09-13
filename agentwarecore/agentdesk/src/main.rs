//! The agentdesk: a workspace, as a process.
//!
//! One of these runs per tab in the navigation bar, forked by PID 1 when a
//! workspace is created and alive until the human closes it. It is the process
//! ARCHITECTURE.md describes as owning the conversation: the transcript lives
//! here and nowhere else, the decision to run a turn is taken here, and a
//! message that arrives while a turn is running is queued here rather than
//! refused. It is also the workspace's launcher for its agent, because it is
//! the only process in the workspace that may ask the broker for anything; the
//! human's launcher is the compositor's start menu, which asks PID 1 itself.
//!
//! It draws the way every client draws: the whole tree, every time, over the
//! descriptor it was handed at spawn. Three of its top-level nodes name regions,
//! which the compositor honours only because this arrived on a desk connection.
//!
//! * `background`: the wallpaper, an `image` whose source is whatever the
//!   settings say. The settings file is stat'd on the clock tick and re-read
//!   when it has changed, so a choice made in the settings application reaches
//!   every workspace within a second without any process being told about it.
//! * `taskbar`: the clock and date at the far right. The compositor draws the
//!   start button at the left end of the same band and the dock in the middle
//!   of it, so the desk keeps both clear: by design, not by protocol.
//! * `pane`: the conversation. What the human said, what the agent said back,
//!   and between the two everything the agent reported while it worked.
//!
//! ## What it does not do
//!
//! It never tracks focus, the caret or scroll position. Nothing about them
//! arrives here and nothing about them is sent; a re-render on every clock tick
//! does not move the human's cursor because that state is the compositor's. Its
//! ids are written by hand and never change, which is what carries that state
//! across the re-render.
//!
//! It never talks to the compositor about windows. It does not know which
//! applications are open, where their windows are, or what its own tab is
//! called. It knows what it asked PID 1 to open on its agent's behalf, and
//! nothing about what the human opened from the start menu.
//!
//! An agent cannot see it. Trees on a desk connection are chrome by
//! construction, so nothing here needs marking as private and the agent cannot
//! read the transcript of its own reports back out of the pane.
//!
//! ## The turn
//!
//! A turn begins when a message is sent and no turn is running: the desk asks
//! PID 1 for an agent, receives the desk end of a private channel on the reply,
//! and streams the conversation so far and the new message down it. What comes
//! back up is telemetry, shown as it arrives, and finally a reply, which joins
//! the conversation. The turn is over when the channel hangs up, whether the
//! agent finished, failed or was interrupted, so every ending looks the same
//! from here and none of them needs a separate message. Messages sent meanwhile
//! wait in a queue and become the next turn the moment this one ends.
//!
//! Usage: agentdesk <id> [opening prompt]

use std::fmt::Write as _;
use std::io::Write as IoWrite;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use awproto::broker::Broker;
use awproto::display::{self, Event, Surface, escape};
use awproto::settings;
use awproto::turn::{self, Channel, Message, Report};
use rustix::event::{PollFd, PollFlags, poll};

/// The directory of installed applications, in the package format: one folder
/// per app holding `exec`, `icon.svg`, `description.txt` and optionally
/// `name.txt`. The desk reads the last two; PID 1 reads only the first.
const APP_DIR: &str = "/apps";

/// How often the desk wakes on its own: to move the clock and to notice a
/// changed setting. Once a second is the finest the clock shows and coarse
/// enough that an idle workspace costs nothing anyone can measure.
const TICK: Duration = Duration::from_secs(1);

/// The most earlier conversation a turn begins with, in tokens. The one cut
/// the design makes into context, and it is made here because the desk owns
/// the conversation (see docs/OnDevice.md, "bound history at the turn
/// boundary").
///
/// The number rests on the padded-history measurements of 12 September
/// 2026 with the 35B MoE (docs/OnDevicePlan.md, Phase 3), which drew two
/// curves and they disagree. With ordinary conversation as the history
/// (questions answered in words), 38k tokens cost nothing on the short
/// tasks: 15 of 15 passed, exactly as with no history at all. With three
/// hundred copies of the agent's own action report as the history, the
/// worst history there could be, the suite fell from 17/18 at 2.5k to
/// 15/18 at 11k, 12/18 at 20k and 9/18 at 38k, always the same way: a new
/// request answered in one exchange with a report of work not done, the
/// shape of the replies in front of it. A real conversation is neither
/// curve, and the realistic one, padded from genuine transcripts, is not
/// measured yet. Until it is, this is the design's figure, the one the
/// ordinary-conversation curve supports. With the history carrying each
/// turn's tool calls (see `with_calls`), 64k of it scored 12/18 on 13
/// September 2026 with no fabricated report in any miss, so the budget is
/// 64k: the memory bound, since a long turn grew 58k tokens over its
/// history and the slot is 128k. What must not happen is setting this from
/// the pathological curve: a budget that forgets what the human said an
/// hour ago, every day, to guard against a failure only a synthetic
/// history has produced.
const HISTORY_BUDGET: usize = 65_536;

/// Characters per token for estimating the budget, set low on purpose: the
/// calibrated figure for this model is about 3.2, and 3 makes the estimate
/// err towards counting more tokens than there are, so the cut is never
/// later than it should be.
const CHARS_PER_TOKEN: usize = 3;

/// The most calls a turn's history entry keeps. A traverse is sixty to a
/// hundred; what the next turn needs to see is that the work was done with
/// tools, not every step of it, so the first and last of a long turn stay.
const CALLS_KEPT: usize = 40;

/// An agent's reply as it goes into the history: the reply, then the tool
/// calls that earned it, one JSON object per line after
/// `turn::HISTORY_CALLS`. The agent turns these back into tool-use blocks
/// when it builds the next turn's conversation (`awagent/src/history.rs`).
///
/// History used to hold the reply alone, and that was a flaw measured on
/// 12 September 2026: a conversation of forty turns, each a request and an
/// instant report, shows a model no tool call anywhere, however many
/// hundreds were made, and under such a history it answered new requests
/// with a report and no tool call, 0 of 15 short tasks at 8k tokens. Listing
/// the actions as text above the reply made it write that text itself. The
/// calls go in as calls. The results stay out, since they are the tokens.
fn with_calls(calls: &[String], reply: &str) -> String {
    if calls.is_empty() {
        return reply.to_owned();
    }
    let mut text = format!("{reply}\n{}\n", turn::HISTORY_CALLS);
    if calls.len() <= CALLS_KEPT {
        for call in calls {
            let _ = writeln!(text, "{call}");
        }
    } else {
        let head = CALLS_KEPT / 2;
        for call in &calls[..head] {
            let _ = writeln!(text, "{call}");
        }
        for call in &calls[calls.len() - (CALLS_KEPT - head)..] {
            let _ = writeln!(text, "{call}");
        }
    }
    text
}

/// Where the history a turn is given starts: the index of the oldest
/// message that fits the budget, counting back from the newest, moved
/// forward to the next human message so what the agent reads begins with
/// a request rather than an answer to one it cannot see. Whole messages,
/// oldest dropped first, nothing summarised: a summary is a model call the
/// desk would have to make, and the design leaves that for the day it is
/// needed.
fn kept_from(history: &[Message]) -> usize {
    let mut spent = 0;
    let mut start = history.len();
    while start > 0 {
        let cost = history[start - 1].text.len() / CHARS_PER_TOKEN + 1;
        if spent + cost > HISTORY_BUDGET {
            break;
        }
        spent += cost;
        start -= 1;
    }
    while start < history.len() && history[start].role != turn::ROLE_HUMAN {
        start += 1;
    }
    start
}

/// An installed application, by package.
struct Installed {
    /// The folder name, which is what the broker is asked for.
    name: String,
    /// What people see. `name.txt` if the package has one, else the folder.
    label: String,
    /// What it is for. Already required to be non-empty, since a package
    /// without one is a self-test stand-in rather than an application; kept
    /// now because the agent is told it too, and a name alone is a guess.
    description: String,
}

/// One line of the pane.
enum Line {
    /// Something the human typed.
    Human(String),
    /// The agent's reply at the end of a turn.
    Agent(String),
    /// What the agent reported while working: `kind`, then the text.
    Telemetry(String, String),
    /// The workspace's own notes: turns starting and ending, apps opening.
    System(String),
}

/// A turn in progress.
struct Running {
    pid: i32,
    channel: Channel,
    /// Whether the agent has replied yet. One that hangs up without replying
    /// ended some other way, and the human is told so.
    replied: bool,
    /// Every tool call the agent reported this turn (`turn::KIND_CALL`), in
    /// order, so the reply that ends the turn carries them into the
    /// history. See `with_calls`.
    calls: Vec<String>,
    started: Instant,
    /// What the turn runs as, for the working line under the transcript. Held
    /// here rather than read from the selector, which may already say what
    /// the *next* turn will run.
    label: &'static str,
}

struct Desk {
    id: u32,
    broker: Option<Broker>,
    /// Everything shown in the pane, in order.
    lines: Vec<Line>,
    /// What goes to the agent as context: what was said, by whom. A subset of
    /// `lines`, because telemetry and system notes are for the human.
    history: Vec<Message>,
    /// How many of the oldest messages no longer reach the agent, so the
    /// pane says so once when the number grows and not on every turn.
    forgotten: usize,
    /// The message being composed.
    composing: String,
    /// Which of the backend configurations this desk's turns run with. Chosen
    /// in the pane, per workspace, because which model answers is a property
    /// of the conversation; the key that authenticates it is the machine's
    /// and lives in settings. Read when a turn starts, so changing it
    /// mid-turn applies to the next one.
    backend: &'static turn::BackendConfig,
    /// Whether the backend dropdown is showing its options.
    backend_open: bool,
    /// Messages sent while a turn was running. They start the next one.
    queued: Vec<String>,
    turn: Option<Running>,
    /// The last thing that happened, shown under the pane's heading so the
    /// desk's own account can be compared with the compositor's.
    status: String,
    installed: Vec<Installed>,
    /// What this desk has asked PID 1 to open, by label, for the record.
    opened: Vec<String>,
    /// The wallpaper path currently shown, or none for a plain background.
    wallpaper: Option<String>,
    /// The time zone, as minutes east of UTC, from the same settings file.
    utc_offset: i32,
    /// When the settings file was last read, so a tick re-reads it only when
    /// it has changed.
    settings_seen: Option<SystemTime>,
    /// The clock as last rendered, so a tick that changes nothing sends nothing.
    clock: String,
}

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(id) = args.next().and_then(|id| id.parse::<u32>().ok()) else {
        log("usage: agentdesk <id> [opening prompt] [backend]");
        std::process::exit(2);
    };
    let opening: Option<String> = args.next().filter(|text| !text.trim().is_empty());
    // Which model this workspace answers with, chosen in the start menu
    // before the conversation existed. A name from outside the table is not
    // an error worth refusing a workspace over: it falls back to the default,
    // which is what a workspace nobody chose for gets anyway.
    let backend = args.next().and_then(|id| turn::backend_config(&id));

    let mut surface = match Surface::inherited() {
        Ok(surface) => surface,
        Err(err) => {
            log(&format!("no interface connection: {err}"));
            std::process::exit(1);
        }
    };
    if let Err(err) = surface.set_nonblocking(true) {
        log(&format!("could not make the interface connection non-blocking: {err}"));
        std::process::exit(1);
    }

    let mut desk = Desk::new(id);
    if let Some(backend) = backend {
        desk.backend = backend;
    }
    log(&format!("desk {id}: up, {} application(s) installed", desk.installed.len()));

    // The opening prompt is the reason the workspace exists, so it is the
    // first message and it starts a turn at once. Creating a workspace never
    // started that turn; this is the agentdesk deciding to, which is what keeps
    // the decision with the process that owns the conversation.
    if let Some(prompt) = opening {
        desk.submit(prompt);
    }

    if let Err(err) = surface.render(&desk.render()) {
        log(&format!("could not send the first tree: {err}"));
        std::process::exit(1);
    }

    loop {
        // Everything the desk waits on, together: the compositor, the agent
        // if there is one, and a deadline for the clock.
        let mut fds = vec![PollFd::new(&surface, PollFlags::IN)];
        if let Some(running) = &desk.turn {
            fds.push(PollFd::new(&running.channel, PollFlags::IN));
        }
        let timeout = until_next_tick();
        match poll(&mut fds, Some(&to_timespec(timeout))) {
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => continue,
            Err(err) => {
                log(&format!("poll failed: {err}"));
                std::process::exit(1);
            }
        }
        let surface_ready = fds[0].revents().intersects(PollFlags::IN | PollFlags::HUP | PollFlags::ERR);
        let agent_ready = fds
            .get(1)
            .is_some_and(|fd| fd.revents().intersects(PollFlags::IN | PollFlags::HUP | PollFlags::ERR));
        drop(fds);

        let mut dirty = false;

        if surface_ready {
            match surface.pump() {
                Ok(true) => {}
                // The compositor hung up. There is nothing left to draw on, and
                // nothing to save: the conversation was never meant to survive
                // its workspace.
                Ok(false) => {
                    log("the compositor closed the connection");
                    return;
                }
                Err(err) => {
                    log(&format!("connection failed: {err}"));
                    std::process::exit(1);
                }
            }
            loop {
                match surface.take_event() {
                    Ok(Some(event)) => dirty |= desk.accept(&surface, &event),
                    Ok(None) => break,
                    Err(err) => {
                        log(&format!("connection failed: {err}"));
                        std::process::exit(1);
                    }
                }
            }
        }

        if agent_ready {
            dirty |= desk.hear_agent();
        }

        dirty |= desk.tick();

        if dirty && let Err(err) = surface.render(&desk.render()) {
            log(&format!("could not send a tree: {err}"));
            return;
        }
    }
}

impl Desk {
    fn new(id: u32) -> Self {
        let stored = settings::Settings::load();
        let mut desk = Desk {
            id,
            broker: None,
            lines: Vec::new(),
            history: Vec::new(),
            forgotten: 0,
            composing: String::new(),
            backend: turn::default_backend(stored.anthropic_key.is_some()),
            backend_open: false,
            queued: Vec::new(),
            turn: None,
            status: "ready".into(),
            installed: installed_apps(),
            opened: Vec::new(),
            wallpaper: None,
            utc_offset: 0,
            settings_seen: settings::modified(),
            clock: String::new(),
        };
        desk.wallpaper = stored.wallpaper;
        desk.utc_offset = stored.utc_offset;
        desk.clock = clock_text(desk.utc_offset);
        desk
    }

    // ---- the broker ----------------------------------------------------------

    /// Ask PID 1 for something, connecting the first time it is needed.
    ///
    /// By path, because a process that has not been forked yet cannot have been
    /// handed a descriptor to the thing that will fork it. This is the
    /// agentdesk's job and nobody else's in the workspace: applications are
    /// opened *into* a workspace by the workspace, and an app that could fork
    /// its own would be outside the boundary that owns it.
    fn ask<T>(&mut self, request: impl FnOnce(&mut Broker) -> Result<T, String>) -> Result<T, String> {
        if self.broker.is_none() {
            match Broker::connect() {
                Ok(broker) => self.broker = Some(broker),
                Err(err) => return Err(format!("cannot reach the supervisor: {err}")),
            }
        }
        let Some(broker) = &mut self.broker else { unreachable!() };
        let result = request(broker);
        // A failed request may mean the connection is unusable; the next
        // request reconnects rather than failing the same way forever.
        if result.is_err() {
            self.broker = None;
        }
        result
    }

    /// Open an application into this workspace, on the agent's behalf.
    fn launch(&mut self, name: &str) {
        let desk = self.id;
        let label = self
            .installed
            .iter()
            .find(|app| app.name == name)
            .map(|app| app.label.clone())
            .unwrap_or_else(|| name.to_owned());
        match self.ask(|broker| broker.open_app(desk, name)) {
            Ok(pid) => {
                self.status = format!("opened {label} as pid {pid}");
                self.opened.push(label);
            }
            Err(err) => {
                self.status = format!("could not open {label}: {err}");
                self.note(format!("Could not open {label}: {err}"));
            }
        }
        log(&self.status);
    }

    // ---- the conversation ------------------------------------------------------

    /// The human said something. It joins the conversation and either starts
    /// a turn or waits for the running one to end.
    fn submit(&mut self, text: String) {
        self.lines.push(Line::Human(text.clone()));
        if self.turn.is_some() {
            self.queued.push(text);
            self.status = format!("{} message(s) waiting for the turn to end", self.queued.len());
            self.note("Queued until the current turn ends.".to_owned());
            return;
        }
        self.begin_turn(vec![text]);
    }

    /// Start a turn on one or more new messages.
    ///
    /// Everything before them is history; the last one is the prompt, and any
    /// before it are messages that arrived while the previous turn ran. All
    /// of them are the human's, and the agent sees them in the order they were
    /// said.
    fn begin_turn(&mut self, mut messages: Vec<String>) {
        let Some(prompt) = messages.pop() else { return };
        for text in messages {
            self.history.push(Message { role: turn::ROLE_HUMAN.into(), text });
        }

        // The one cut into context: what the agent begins with is bounded
        // here, at the turn boundary, where the prompt is assembled fresh
        // anyway. The pane is told once, when the cut first moves.
        let start = kept_from(&self.history);
        if start > self.forgotten {
            self.note(format!(
                "The conversation is longer than the agent is given: its first {start} \
                 message(s) no longer reach it."
            ));
            self.forgotten = start;
        }

        let desk = self.id;
        match self.ask(|broker| broker.start_agent(desk)) {
            Ok((pid, stream)) => match Channel::new(stream) {
                Ok(mut channel) => {
                    // What is installed goes down with the prompt. The desk
                    // already reads the directory for its own launcher, and
                    // an agent that has to guess an application's name will
                    // guess wrong and keep guessing.
                    let installed: Vec<turn::InstalledApp> = self
                        .installed
                        .iter()
                        .map(|app| turn::InstalledApp {
                            name: app.name.clone(),
                            label: app.label.clone(),
                            description: app.description.clone(),
                        })
                        .collect();
                    if let Err(err) = channel.send_context(
                        &self.history[start..],
                        &prompt,
                        self.backend.id,
                        &installed,
                    )
                    {
                        self.status = format!("could not brief the agent: {err}");
                        self.note(self.status.clone());
                        log(&self.status);
                    }
                    self.history.push(Message { role: turn::ROLE_HUMAN.into(), text: prompt });
                    self.turn = Some(Running {
                        pid,
                        channel,
                        replied: false,
                        calls: Vec::new(),
                        started: Instant::now(),
                        label: self.backend.label,
                    });
                    self.status = format!("agent running as pid {pid}");
                    log(&format!("desk {desk}: {}", self.status));
                }
                Err(err) => {
                    self.history.push(Message { role: turn::ROLE_HUMAN.into(), text: prompt });
                    self.status = format!("agent started but its channel is unusable: {err}");
                    self.note(self.status.clone());
                    log(&self.status);
                }
            },
            Err(err) => {
                // The message stays in the conversation. It was said; the
                // failure was in answering it, and the next attempt carries it.
                self.history.push(Message { role: turn::ROLE_HUMAN.into(), text: prompt });
                self.status = format!("could not start an agent: {err}");
                self.note(self.status.clone());
                log(&format!("desk {desk}: {}", self.status));
            }
        }
    }

    /// Something arrived from the agent, or it hung up.
    fn hear_agent(&mut self) -> bool {
        let Some(running) = &mut self.turn else { return false };

        let alive = match running.channel.pump() {
            Ok(alive) => alive,
            Err(err) => {
                log(&format!("agent channel failed: {err}"));
                false
            }
        };

        // Drained first, acted on second: acting may need the whole desk, and
        // the channel is inside it.
        let mut reports = Vec::new();
        loop {
            match running.channel.take_report() {
                Ok(Some(report)) => reports.push(report),
                Ok(None) => break,
                Err(err) => {
                    log(&format!("agent channel protocol error: {err}"));
                    break;
                }
            }
        }

        let mut dirty = !reports.is_empty();
        for report in reports {
            match report {
                // A call is kept for the history and never shown: the pane
                // has the action lines for that.
                Report::Telemetry { kind, text } if kind == turn::KIND_CALL => {
                    if let Some(running) = &mut self.turn {
                        running.calls.push(text);
                    }
                }
                Report::Telemetry { kind, text } => self.lines.push(Line::Telemetry(kind, text)),
                Report::Reply(text) => {
                    let calls = match &mut self.turn {
                        Some(running) => {
                            running.replied = true;
                            std::mem::take(&mut running.calls)
                        }
                        None => Vec::new(),
                    };
                    self.history.push(Message {
                        role: turn::ROLE_AGENT.into(),
                        text: with_calls(&calls, &text),
                    });
                    self.lines.push(Line::Agent(text));
                }
                // The agent may open applications, but only through the
                // workspace, which is what asks PID 1. The agent has no broker
                // connection and no way to get one.
                Report::OpenApp(name) => {
                    self.launch(&name);
                    let label = self.opened.last().cloned().unwrap_or(name);
                    self.note(format!("The agent opened {label}."));
                }
            }
        }

        if !alive {
            self.end_turn();
            dirty = true;
        }
        dirty
    }

    /// The channel hung up: the turn is over, however it ended.
    fn end_turn(&mut self) {
        let Some(running) = self.turn.take() else { return };
        let took = running.started.elapsed().as_secs_f32();
        if running.replied {
            self.status = format!("turn finished in {took:.1}s");
        } else {
            self.status = format!("agent pid {} ended without replying after {took:.1}s", running.pid);
            self.note("The agent stopped without replying.".to_owned());
        }
        log(&format!("desk {}: {}", self.id, self.status));

        // Whatever arrived meanwhile is the next turn, at once. A message is
        // never refused and never forgotten; it only ever waits.
        if !self.queued.is_empty() {
            let waiting = std::mem::take(&mut self.queued);
            self.begin_turn(waiting);
        }
    }

    fn note(&mut self, text: String) {
        self.lines.push(Line::System(text));
    }

    // ---- the clock, and settings -----------------------------------------------

    /// Once a second: the clock, and the wallpaper setting. Returns whether
    /// anything on screen changed.
    fn tick(&mut self) -> bool {
        let mut dirty = false;
        let clock = clock_text(self.utc_offset);
        if clock != self.clock {
            self.clock = clock;
            dirty = true;
        }
        // While a turn runs, the working line under the transcript animates
        // on this same tick, so a model that is quietly thinking still looks
        // alive. The clock alone moves once a minute; this is once a second,
        // and only while there is something to show for it.
        if self.turn.is_some() {
            dirty = true;
        }
        // The settings file, re-read only when its clock says it changed: a
        // stat a second is nothing, a parse a second is needless.
        let seen = settings::modified();
        if seen != self.settings_seen {
            self.settings_seen = seen;
            let stored = settings::Settings::load();
            if stored.wallpaper != self.wallpaper {
                log(&format!(
                    "desk {}: wallpaper is now {}",
                    self.id,
                    stored.wallpaper.as_deref().unwrap_or("none")
                ));
                self.wallpaper = stored.wallpaper;
                dirty = true;
            }
            if stored.utc_offset != self.utc_offset {
                log(&format!(
                    "desk {}: time zone is now UTC{}",
                    self.id,
                    settings::format_offset(stored.utc_offset)
                ));
                self.utc_offset = stored.utc_offset;
                self.clock = clock_text(self.utc_offset);
                dirty = true;
            }
        }
        dirty
    }

    // ---- events ----------------------------------------------------------------

    /// Decide whether an event should be acted on, and act on it.
    ///
    /// The version check is the whole reason events carry one. A click names a
    /// node and nothing else, so its meaning comes entirely from the tree it
    /// was generated against; if that tree is gone, so is the meaning. Typed
    /// text is different: the payload *is* the intention, so a late one is
    /// still true. That matters more here than in an application, because the
    /// desk re-renders on its own clock, and a click that landed while the
    /// minute changed must not be thrown away for it: the check compares
    /// versions, and a tick that changes only the clock still moves the
    /// version, so a click on a control that did not move is discarded rather
    /// than acted on. The clock is one line of text; the cost is one wasted
    /// click a minute at worst, and the alternative is acting on a stale tree.
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

        match (event.target.as_str(), event.action.as_str()) {
            ("message", display::ACTION_TYPE_TEXT) => {
                self.composing = event.value.clone();
                self.status = "composing".into();
            }
            ("message", display::ACTION_SUBMIT) | ("send-message", display::ACTION_CLICK) => {
                let text = self.composing.trim().to_owned();
                if text.is_empty() {
                    self.status = "nothing to send".into();
                    return true;
                }
                self.composing.clear();
                self.submit(text);
            }

            // The pane's stop square. The same `interrupt` to PID 1 the nav
            // bar's stop sends, so a wedged agent cannot keep it from
            // working; the desk merely offers it where the conversation is.
            ("stop-turn", display::ACTION_CLICK) => {
                let Some(pid) = self.turn.as_ref().map(|running| running.pid) else {
                    self.status = "no turn to stop".into();
                    return true;
                };
                let desk = self.id;
                self.status = match self.ask(|broker| broker.interrupt(desk)) {
                    Ok(()) => format!("stopping agent pid {pid}"),
                    Err(err) => format!("could not stop the agent: {err}"),
                };
                log(&format!("desk {desk}: {}", self.status));
            }

            // The backend selector. Chrome an agent cannot see, so an agent
            // is told what it runs as and can never change it. A choice made
            // while a turn runs applies from the next turn; the selector
            // stays live for the same reason the chat input does.
            ("backend", display::ACTION_OPEN) => self.backend_open = true,
            ("backend", display::ACTION_CLOSE) => self.backend_open = false,
            (target, display::ACTION_SELECT) if target.starts_with("backend-") => {
                self.backend_open = false;
                if let Some(config) = target["backend-".len()..]
                    .parse::<usize>()
                    .ok()
                    .and_then(|index| turn::BACKENDS.get(index))
                    && config.id != self.backend.id
                {
                    self.backend = config;
                    self.status = format!("next turn runs {}", config.label);
                    log(&format!("desk {}: {}", self.id, self.status));
                }
            }

            _ => return false,
        }
        true
    }

    // ---- the tree --------------------------------------------------------------

    /// The workspace shell, from the model, every time.
    ///
    /// There is no `region="apps"` here, and there cannot be. That one belongs
    /// to application processes; a workspace claiming it would be drawing over
    /// its own windows.
    fn render(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, r#"<window title="Agentdesk {}" font="sans">"#, self.id);

        // The wallpaper. An image the compositor loads and fits over the whole
        // workspace, under everything. Left out entirely for "none": the
        // compositor's own background is the plain choice.
        if let Some(path) = &self.wallpaper {
            let _ = writeln!(
                out,
                r#"  <image region="background" src="{}" alt="The workspace wallpaper" fit="cover"/>"#,
                escape(path)
            );
        }

        // The taskbar: the clock and date, at the far right. The rest of the
        // band is the compositor's, the start button at its left end and the
        // dock in its middle, and stays empty here.
        let _ = write!(
            out,
            r#"  <hstack region="taskbar" gap="sm">
    <text grow="true"/>
    <text weight="bold" font="mono">{clock}</text>
    <text color="muted" role="caption">{date}</text>
  </hstack>
"#,
            clock = escape(&self.clock),
            date = escape(&date_text(self.utc_offset)),
        );

        // The pane. The transcript, the field that adds to it, and the
        // backend selector: which model answers is chosen here, beside the
        // conversation it applies to, and an agent never sees the control
        // because the pane is chrome.
        let _ = write!(
            out,
            r#"  <vstack region="pane" gap="sm">
    <text role="subheading">Conversation</text>
    <text role="caption" color="muted">{status}</text>
    <select id="backend" value="{value}"{open} description="Chooses the model that answers in this agentdesk. A change applies from the next turn">
"#,
            status = escape(&self.status),
            value = turn::BACKENDS
                .iter()
                .position(|config| config.id == self.backend.id)
                .unwrap_or(0),
            open = if self.backend_open { r#" open="true""# } else { "" },
        );
        for (index, config) in turn::BACKENDS.iter().enumerate() {
            let _ = writeln!(
                out,
                r#"      <option id="backend-{index}" label="{label}" value="{index}"{selected} description="Runs this agentdesk's turns as {label}"/>"#,
                label = config.label,
                selected = if config.id == self.backend.id { r#" selected="true""# } else { "" },
            );
        }
        let _ = write!(
            out,
            r#"    </select>
    <scroll grow="true" anchor="end">
      <vstack gap="sm">
"#,
        );
        if self.lines.is_empty() {
            out.push_str(
                "        <text color=\"muted\">Say what this agentdesk is for, and an agent will get to work.</text>\n",
            );
        }
        for line in &self.lines {
            match line {
                Line::Human(text) => {
                    let _ = writeln!(
                        out,
                        "        <text role=\"caption\" color=\"accent\" weight=\"bold\">You</text>\n        <text>{}</text>",
                        escape(text)
                    );
                }
                Line::Agent(text) => {
                    let _ = writeln!(
                        out,
                        "        <text role=\"caption\" color=\"ok\" weight=\"bold\">Agent</text>\n        <text>{}</text>",
                        escape(text)
                    );
                }
                Line::Telemetry(kind, text) => {
                    let color = match kind.as_str() {
                        turn::KIND_ERROR => "danger",
                        turn::KIND_ACTION => "text",
                        _ => "muted",
                    };
                    let _ = writeln!(
                        out,
                        "        <text role=\"caption\" color=\"{color}\">{} {}</text>",
                        telemetry_mark(kind),
                        escape(text)
                    );
                }
                Line::System(text) => {
                    let _ = writeln!(
                        out,
                        "        <text role=\"caption\" color=\"muted\" italic=\"true\">{}</text>",
                        escape(text)
                    );
                }
            }
        }
        // The working line: while a turn runs, the transcript ends with what
        // is running and for how long, dots moving on the clock tick, so a
        // model that is thinking without telemetry still visibly exists. The
        // scroll's anchor keeps it in view, and it vanishes with the turn.
        if let Some(running) = &self.turn {
            let dots = ".".repeat(1 + (now_secs().rem_euclid(3)) as usize);
            let _ = writeln!(
                out,
                "        <text role=\"caption\" color=\"accent\" italic=\"true\">{} is working{dots} ({}s)</text>",
                escape(running.label),
                running.started.elapsed().as_secs(),
            );
        }
        // The composer: a multi-line editor, because a message to an agent is
        // as often a paragraph as a phrase; Enter starts a new line and the
        // plane sends. Beside it, the send button always, because a message
        // mid-turn queues rather than being refused; and while a turn runs, a
        // stop square too, which ends the turn the way the nav bar's stop
        // does: through PID 1, never through the agent being stopped.
        let _ = write!(
            out,
            r#"      </vstack>
    </scroll>
    <hstack gap="sm">
      <editor id="message" grow="true" enter-submits="true" placeholder="Message the agent" value="{message}"
             description="Composes a message to the agent working in this workspace. Enter sends; Shift and Enter starts a new line"/>
      <button id="send-message" glyph="send" emphasis="primary"
              description="Sends the composed message, which starts an agent turn or queues for the running one"/>
"#,
            message = escape(&self.composing),
        );
        if self.turn.is_some() {
            out.push_str(
                "      <button id=\"stop-turn\" glyph=\"stop\" emphasis=\"danger\"\n              description=\"Stops the running turn; the workspace and the conversation stay as they are\"/>\n",
            );
        }
        out.push_str("    </hstack>\n  </vstack>\n</window>\n");
        out
    }
}

/// A short prefix telling a telemetry line's kind apart at a glance.
fn telemetry_mark(kind: &str) -> &'static str {
    match kind {
        turn::KIND_THOUGHT => "thinking:",
        turn::KIND_ACTION => ">",
        turn::KIND_RESULT => "=",
        turn::KIND_ERROR => "error:",
        _ => "-",
    }
}

/// Every application installed, read from the package directory, so the
/// transcript can name what the agent opened by its label rather than its
/// folder. The same rule as the start menu: a package without a description
/// is a stand-in or a leftover, not an application.
fn installed_apps() -> Vec<Installed> {
    let Ok(entries) = std::fs::read_dir(APP_DIR) else { return Vec::new() };
    let mut apps: Vec<Installed> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let dir = entry.path();
            if !dir.join("exec").is_file() {
                return None;
            }
            let read = |file: &str| {
                std::fs::read_to_string(dir.join(file))
                    .map(|text| text.trim().to_owned())
                    .unwrap_or_default()
            };
            let description = read("description.txt");
            if description.is_empty() {
                return None;
            }
            let label = read("name.txt");
            Some(Installed {
                label: if label.is_empty() { name.clone() } else { label },
                name,
                description,
            })
        })
        .collect();
    apps.sort_by(|a, b| a.label.cmp(&b.label));
    apps
}

// ---- the clock -------------------------------------------------------------------

/// Seconds since the epoch, as the kernel has it: UTC. The kernel keeps one
/// clock and knows nothing of zones; the offset from settings is applied
/// here, by the thing showing the time, and nowhere else.
fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The wall clock's seconds since the epoch: UTC shifted by the zone.
fn local_secs(utc_offset: i32) -> i64 {
    now_secs() + utc_offset as i64 * 60
}

/// Hours and minutes, twenty-four hour, in the zone.
fn clock_text(utc_offset: i32) -> String {
    let secs = local_secs(utc_offset).rem_euclid(86_400);
    format!("{:02}:{:02}", secs / 3600, (secs % 3600) / 60)
}

/// Weekday, day and month, like `Sat 16 Aug`, in the zone.
fn date_text(utc_offset: i32) -> String {
    let days = local_secs(utc_offset).div_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let _ = year;
    const WEEKDAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] =
        ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    // Day zero of the epoch was a Thursday.
    let weekday = WEEKDAYS[days.rem_euclid(7) as usize];
    format!("{weekday} {day} {}", MONTHS[(month - 1) as usize])
}

/// Days since the epoch to a calendar date, in the proleptic Gregorian
/// calendar. Howard Hinnant's algorithm, which is exact for any day the
/// machine's clock could plausibly report.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// How long until the next whole second, so the clock changes on the second
/// rather than up to a second late.
fn until_next_tick() -> Duration {
    let into = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    TICK.saturating_sub(Duration::from_nanos(into as u64)) + Duration::from_millis(5)
}

fn to_timespec(duration: Duration) -> rustix::event::Timespec {
    rustix::event::Timespec {
        tv_sec: duration.as_secs() as _,
        tv_nsec: duration.subsec_nanos() as _,
    }
}

/// Log to the kernel ring buffer.
///
/// A workspace has no console worth writing to: the haimanager owns the screen
/// and stdout goes to a terminal that is no longer being displayed.
fn log(message: &str) {
    let line = format!("<6>agentdesk: {message}\n");
    if let Ok(mut kmsg) = std::fs::OpenOptions::new().write(true).open("/dev/kmsg") {
        let _ = kmsg.write_all(line.as_bytes());
    } else {
        eprintln!("agentdesk: {message}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn said(role: &str, chars: usize) -> Message {
        Message { role: role.into(), text: "x".repeat(chars) }
    }

    #[test]
    fn a_short_conversation_is_kept_whole() {
        let history: Vec<Message> = (0..10)
            .map(|i| said(if i % 2 == 0 { turn::ROLE_HUMAN } else { turn::ROLE_AGENT }, 300))
            .collect();
        assert_eq!(kept_from(&history), 0);
        assert_eq!(kept_from(&[]), 0);
    }

    #[test]
    fn the_oldest_go_first_and_what_is_kept_starts_with_the_human() {
        // Each pair is 2 * (3000 / 3 + 1) = 2002 tokens by the estimate, so
        // the budget holds about sixteen pairs of the forty.
        let history: Vec<Message> = (0..80)
            .map(|i| said(if i % 2 == 0 { turn::ROLE_HUMAN } else { turn::ROLE_AGENT }, 3000))
            .collect();
        let start = kept_from(&history);
        assert!(start > 0 && start < history.len(), "{start}");
        assert_eq!(history[start].role, turn::ROLE_HUMAN);
        let kept: usize = history[start..].iter().map(|m| m.text.len() / CHARS_PER_TOKEN + 1).sum();
        assert!(kept <= HISTORY_BUDGET);
        // One more pair would not have fitted.
        let one_more = kept + 2 * (3000 / CHARS_PER_TOKEN + 1);
        assert!(one_more > HISTORY_BUDGET);
    }

    #[test]
    fn a_single_message_over_budget_leaves_nothing_but_never_panics() {
        let history = vec![said(turn::ROLE_HUMAN, HISTORY_BUDGET * CHARS_PER_TOKEN * 2)];
        assert_eq!(kept_from(&history), 1);
    }
}
