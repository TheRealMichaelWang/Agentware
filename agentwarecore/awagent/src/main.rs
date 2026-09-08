//! The agent: one turn of real work, with a model behind it, then gone.
//!
//! The per-turn worker is the shortest-lived thing in Agentware and owns
//! nothing durable. Interrupting it is a signal to a process with no state,
//! crashing it takes nothing with it, and it is never restarted, because
//! restarting one would silently re-run side effects it had already
//! performed.
//!
//! This file is the harness: it owns the agentic loop and both wires. The
//! context comes down the private channel from the agentdesk, naming which
//! backend configuration to run; the conversation goes to the model through
//! a [`backend::Backend`]; and everything the model wants done on screen is
//! executed here, over the compositor link, as intents. What the model says
//! while it works goes back up the channel as telemetry, so the human
//! watches the turn in the pane, and the model's final message is the reply
//! that joins the conversation.
//!
//! Notice what this file still cannot do, model or no model. It never
//! produces an event, only intents, so a rejection (`disabled`, `blocked`,
//! `unsupported-action`) is an answer handed back to the model as a tool result to
//! reason about, not an error. It never names a workspace, cannot see the
//! agentdesk that started it, and cannot read the transcript of its own
//! streamed thoughts. It cannot open an application itself: it asks the
//! agentdesk, which asks PID 1, and it finds out whether that worked the way
//! it finds out everything, by asking the compositor.
//!
//! Usage: awagent <desk-id>

mod backend;
mod backends;
mod consequences;
mod http;
mod interrupt;

use std::collections::HashSet;
use std::io::Write as _;
use std::time::{Duration, Instant};

use awproto::agent::{Link, Outcome};
use awproto::turn::{self, Turn};
use serde_json::{Value, json};

use backend::{
    Assistant, Backend, BackendError, Block, Delta, ModelMessage, Role, Stop, ToolDef, Usage,
};
use interrupt::Watch;

/// How many times one exchange may be started again because the workspace
/// changed under it, before the change waits for the exchange instead.
///
/// An interruption is the right answer to an application that moved once
/// while the model was answering. It is the wrong answer to one that moves
/// continuously, a progress bar say, which would otherwise keep the model
/// from ever finishing a sentence. After this many, the exchange runs to its
/// end and whatever arrived is attached to its results in the ordinary way.
const INTERRUPTIONS: u32 = 3;

/// How many exchanges in a row may reach a state this turn has already been
/// in before the harness says so in the tool result, and before it gives up
/// on the turn.
///
/// Not an exchange ceiling: a turn reaching somewhere new stays unbounded,
/// however long it takes. What is counted is a turn going in circles, which
/// is the shape a stuck one actually has. Both numbers are guesses until the
/// task suite tunes them.
const NUDGE: u32 = 3;
const STUCK: u32 = 6;

/// How long to wait for an application to appear after asking for it. An
/// application is forked by PID 1 and attaches when its first tree arrives,
/// which is quick, but not instant.
const OPENING: Duration = Duration::from_secs(5);

/// How often to look while waiting for one.
///
/// A look is one round trip down a socket to a compositor that answers it out
/// of memory, so the poll is cheap and the interval is set by how long a wait
/// is worth rather than by what the asking costs. An application attaches
/// within a couple of hundred milliseconds; at the old 300ms this rounded a
/// fast open up to a slow one.
const OPENING_POLL: Duration = Duration::from_millis(50);

/// What the model is told about the machine it is driving. Everything here
/// restates a contract that holds elsewhere in the system; the model is the
/// one part that cannot read the source.
const SYSTEM: &str = "\
You are the agent of an Agentware agentdesk: an AI-native operating system where you and \
the human share one workspace and use the same applications through the same interface. \
You act for the human, visibly: everything you do is performed on their screen with a \
cursor they can watch.

How the interface reads:
- read_app returns an application's interface as reduced semantic markup. Every control \
carries an id, a description of what it does, its state, and an actions attribute listing \
what it accepts right now. Layout is stripped; what you see is what exists.
- The actions attribute is the authority. A disabled control lists none. A control marked \
blocked is behind an open dialog.
- Dialogs appear as controls nested in <dialog>. While one is up, everything outside it \
answers blocked, for you and the human alike; answer the dialog and the rest comes back.

How acting works:
- act names an application, a control id, and one action from a closed vocabulary: focus, \
click, type-text, clear, submit, check, uncheck, toggle, select, select-range, deselect, \
set-value, open, close, move.
- The compositor stages every action itself: the target's window comes to the front and \
its siblings are put away before your action lands. You cannot move, resize or arrange \
windows, and never need to.
- type-text replaces a field's contents with the given value, typed on screen one \
keystroke at a time. submit is the Enter key. check and uncheck are unconditional; prefer \
them over toggle so the outcome does not depend on stale state.
- Scrolling is not something you do. Acting on a control scrolls whatever has to move so \
that it is on screen first, exactly as the window is brought to the front for you. You \
never ask for it and are never told a control was out of view.

Rejections are answers, not failures:
- disabled: the application has disabled it; something in the app must change first.
- blocked: a dialog is in front; read the view and answer the dialog.
- unreachable: the compositor could not bring it on screen. This is a fault in the \
system rather than a step you missed; report it and try something else.
- no-such-node, no-such-app: nothing by that name; read again before retrying.
- unsupported-action: that element does not take that action.
- needs-approval: the human must approve it; say what you wanted to do and stop.

A spreadsheet is not in the view:
- A <spreadsheet> says how far it runs, which cell the cursor is on, what is selected, \
and used, the rectangle anything has been put in. Its cells are not elements: a \
screenful of a grid is four hundred of them, which is forty kilobytes to learn twenty \
numbers.
- read_cells is how you read one: name the app, the element's id, and a range like \
A1:D20 or a single cell like B7. What comes back is one line per row, values separated \
by tabs. Ask for the part you need, and let used tell you where the sheet stops.
- Act on a cell by naming it after the element and an exclamation mark: target \
sheet!B7. The cell does not have to be on screen; the compositor scrolls to it, as it \
does for anything else.
- Cells take select to choose one, and type-text, clear and submit. There is no click \
on a cell. **type-text does not need a select first**: name the cell and write to it. \
Selecting is for saying which cell you are looking at, not for permission.
- select-range chooses a run at once: target sheet!A1 with value C5 is A1 through C5. \
One action, because a person dragging across a grid did one thing.
- Tabs take select, and one listing move can be reordered: name the tab and give the \
position it should take, counting from zero.
- A menu takes open and close, and its items take click while it is open. Opening a menu \
by name is how you reach a command a human would right-click for; there is no \
right-click in your vocabulary and you do not need one.

Working style:
- Read before acting: list_apps, then read_app, then act.
- When the work needs an application that is not open, call search_apps first and say what \
you want to do, not what you think it is called. It answers with what this machine has, \
what each one is for, and whether it is already open. Never guess an application's name: \
open_app takes only the names that exist, and a guess is a wasted exchange.
- Issue every action you already know you need in ONE message, as several tool calls. Do \
not send one action and wait for it. A sequence of buttons, the fields of a form, a set of \
cells: none of these depend on each other's results, so they are one message. They are \
performed in the order you give them, each the moment the last was accepted, and you are \
told the outcome of each. The first one refused stops the batch: the rest are answered as \
not attempted, so the machine is exactly as far as the last accepted action. Split only when \
you genuinely cannot know the next step until you have seen this one's result. Every extra \
message is a wait the human sits through.
- When an application's interface changes, after your actions or on its own, its fresh \
view is attached to your tool results automatically, marked as re-read for you. A \
spreadsheet's cells changing is reported separately, naming the cells and, for a small \
rectangle, what they now hold. You therefore rarely need read_app or read_cells to confirm \
a result; use them to look at what you have not just been shown.
- If the workspace changes while you are composing an answer, that answer is discarded \
and you are asked again with the change attached. Nothing from the discarded answer was \
performed.
- Plain text you write between tool calls is shown to the human as progress narration; \
keep it to a line.
- Your final message, with no tool call, ends the turn and joins the conversation as \
your reply. Lead with the outcome.";

/// The agent's two channels, the words that go up the second, and the clock
/// held against the whole of it.
struct Agent {
    link: Link,
    desk: Turn,
    meter: Meter,
    /// What this machine has: what `search_apps` answers from, and what makes
    /// a name that is not one of them answerable at once rather than after
    /// five seconds of waiting for something that was never going to start.
    installed: Vec<turn::InstalledApp>,
}

impl Agent {
    /// Tell the human, in the pane and in the log.
    fn say(&mut self, kind: &str, text: &str) {
        log(text);
        if let Err(err) = self.desk.telemetry(kind, text) {
            log(&format!("could not reach the agentdesk: {err}"));
        }
    }

    /// The reply ends the turn; the agentdesk sees the hangup when this
    /// process exits.
    ///
    /// Every way a turn can end passes through here, which is why the
    /// accounting is printed here: a turn that failed on its second exchange
    /// is exactly as worth knowing the shape of as one that finished.
    fn finish(&mut self, reply: &str) {
        log(&self.meter.summary());
        // The reply goes in the log too, on one line however many it takes in
        // the pane. Everything else the agent says is already copied here so
        // that a serial capture tells the same story the pane does, and the
        // reply was the one thing missing, which left a headless run able to
        // see that a turn ended but not what it concluded.
        log(&format!("reply: {}", reply.replace('\n', " ")));
        if let Err(err) = self.desk.reply(reply) {
            log(&format!("could not deliver the reply: {err}"));
        }
    }
}

// ---- where a turn's seconds go ---------------------------------------------------

/// The accounting for one turn.
///
/// Nothing in the harness timed itself before this, so every claim about what
/// a turn spends its wall clock on was arithmetic or an impression. The parts
/// are chosen to be disjoint and to add up to the whole, which is the only
/// property that makes the numbers worth anything: `model` is the network and
/// the model together, `acting` is the haimanager performing an intent,
/// `reading` is answering a query out of the haimanager's memory, `waiting`
/// is this process waiting for something other than the model (an
/// application answering an action, an application starting), and whatever
/// the four do not account for is the harness itself and had better stay
/// small.
struct Meter {
    started: Instant,
    exchanges: u32,
    actions: u32,
    /// Exchanges abandoned because the workspace changed under them. Their
    /// time is in `model`, since that is where it went; this says how much
    /// of `model` was said twice.
    interrupts: u32,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    model: Duration,
    acting: Duration,
    reading: Duration,
    waiting: Duration,
}

impl Meter {
    fn new() -> Meter {
        Meter {
            started: Instant::now(),
            exchanges: 0,
            actions: 0,
            interrupts: 0,
            input: 0,
            output: 0,
            cache_read: 0,
            cache_write: 0,
            model: Duration::ZERO,
            acting: Duration::ZERO,
            reading: Duration::ZERO,
            waiting: Duration::ZERO,
        }
    }

    /// One completed exchange, logged as its own line so a slow turn can be
    /// read exchange by exchange rather than only in total.
    fn exchange(&mut self, spent: Duration, first: Option<Duration>, usage: &Usage, calls: usize) {
        self.exchanges += 1;
        self.model += spent;
        self.input += u64::from(usage.input);
        self.output += u64::from(usage.output);
        self.cache_read += u64::from(usage.cache_read);
        self.cache_write += u64::from(usage.cache_write);

        // Time to first token separates the model thinking from the wire
        // being slow, and a cache hit shows up here before it shows up
        // anywhere else.
        let ttft = match first {
            Some(at) => format!("{}ms to first token", at.as_millis()),
            None => "nothing streamed".to_owned(),
        };
        log(&format!(
            "exchange {}: {}ms, {ttft}; in {} (cache read {}, write {}) out {}; {calls} tool call(s)",
            self.exchanges,
            spent.as_millis(),
            usage.input,
            usage.cache_read,
            usage.cache_write,
            usage.output,
        ));
    }

    /// The turn, in one line.
    fn summary(&self) -> String {
        let total = self.started.elapsed();
        let accounted = self.model + self.acting + self.reading + self.waiting;
        // Saturating, because the four parts are measured separately and a
        // rounding disagreement must not print as an enormous number.
        let harness = total.saturating_sub(accounted);
        format!(
            "turn: {}ms = model {}ms + acting {}ms + reading {}ms + waiting {}ms + harness {}ms; \
             {} exchange(s), {} action(s); in {} (cache read {}, write {}) out {}; \
             interrupted {} time(s)",
            total.as_millis(),
            self.model.as_millis(),
            self.acting.as_millis(),
            self.reading.as_millis(),
            self.waiting.as_millis(),
            harness.as_millis(),
            self.exchanges,
            self.actions,
            self.input,
            self.cache_read,
            self.cache_write,
            self.output,
            self.interrupts,
        )
    }
}

fn main() {
    let link = match Link::inherited() {
        Ok(link) => link,
        Err(err) => {
            log(&format!("no interface connection: {err}"));
            std::process::exit(1);
        }
    };
    let mut desk = match Turn::inherited() {
        Ok(turn) => turn,
        Err(err) => {
            // Without the channel there is no prompt and nobody to answer.
            log(&format!("no agentdesk channel: {err}"));
            std::process::exit(1);
        }
    };

    // The clock starts before the context is read, because a turn's wall
    // clock is what the human waits through and they are already waiting.
    let meter = Meter::new();
    let context = match desk.context() {
        Ok(context) => context,
        Err(err) => {
            log(&format!("could not read the context: {err}"));
            std::process::exit(1);
        }
    };
    let mut agent =
        Agent { link, desk, meter, installed: context.installed.clone() };
    log(&format!(
        "turn starting: {} earlier message(s), backend {}",
        context.history.len(),
        context.backend
    ));

    let settings = awproto::settings::Settings::load();
    let mut model = match backends::from_config(&context.backend, &settings) {
        Ok(backend) => backend,
        Err(why) => {
            agent.say(turn::KIND_ERROR, &why);
            agent.finish(&format!("I could not start: {why}."));
            return;
        }
    };

    // The conversation as the model sees it: what was said, in order, and
    // the prompt as the newest message. Telemetry never appears here; the
    // model's own working notes from earlier turns ended when those turns
    // did, exactly as the architecture intends.
    let mut messages: Vec<ModelMessage> = context
        .history
        .iter()
        .filter(|message| !message.text.trim().is_empty())
        .map(|message| {
            if message.role == turn::ROLE_AGENT {
                ModelMessage::assistant_text(&message.text)
            } else {
                ModelMessage::user_text(&message.text)
            }
        })
        .collect();
    messages.push(ModelMessage::user_text(&context.prompt));

    let tools = tool_definitions(&context.installed);
    // Every state this turn has left the workspace in, and how many exchanges
    // in a row have produced one that was already in it.
    let mut states: HashSet<u64> = HashSet::new();
    let mut stale: u32 = 0;

    // The agentic loop: ask the model, do what it asks, hand back what
    // happened, until it answers with no tool calls. Every iteration resends
    // the whole conversation; the API's prompt cache makes the resend cheap.
    //
    // No exchange ceiling, deliberately: a long-running turn is the point of
    // an agent, and how long is worth spending is the human's call, made
    // with the working line in front of them and the stop button beside it.
    // Every ending this loop can reach is honest: the model finishes, the
    // backend errors, or the human interrupts.
    //
    // There is one more ending, and it is not a ceiling. A turn that asks the
    // same question over and over is not spending time, it is stuck, and the
    // two are worth telling apart: sixty-four exchanges of alternating
    // `open_app awspreadsheet` and `open_app awcalc` is not work anybody
    // chose to pay for. `states` holds every state the workspace has been
    // left in, so a repeat means the same place reached again and not merely
    // the same verb.
    //
    // While the model answers, a notice from the haimanager ends the exchange:
    // the model is reasoning about a workspace that has just changed, and the
    // rest of that reasoning is about a workspace that no longer exists.
    let watch = Watch::new(agent.link.fd());
    loop {
        let mut interruptions: u32 = 0;
        let assistant = loop {
            model.watch((interruptions < INTERRUPTIONS).then_some(watch));
            match exchange(&mut agent, model.as_mut(), &messages, &tools) {
                Ok(assistant) => break assistant,
                Err(BackendError::Interrupted) => {
                    interruptions += 1;
                    agent.meter.interrupts += 1;
                    let notices = match agent.link.take_notices() {
                        Ok(notices) => notices,
                        Err(err) => connection_lost(&err),
                    };
                    // What changed goes onto the end of the message the
                    // model was answering, so the answer it gives instead
                    // is to the same question with more known about it.
                    // Nothing before the end of that message moves, and the
                    // prefix cache holds.
                    if let (Some(text), Some(last)) =
                        (consequences::describe(&mut agent, &notices), messages.last_mut())
                    {
                        agent.say(
                            turn::KIND_ACTION,
                            "the workspace changed while the model was answering; asking again",
                        );
                        last.content.push(Block::Text(text));
                    }
                }
                Err(err) => {
                    agent.say(turn::KIND_ERROR, &err.to_string());
                    agent.finish(&format!("The turn failed: {err}."));
                    return;
                }
            }
        };

        let calls: Vec<(String, String, Value)> = assistant
            .tool_uses()
            .map(|(id, name, input)| (id.to_owned(), name.to_owned(), input.clone()))
            .collect();

        if calls.is_empty() {
            finish_turn(&mut agent, &assistant);
            return;
        }

        // The narration between tool calls, now that it is known to be
        // narration rather than the reply.
        let narration = assistant.text();
        for line in narration.lines().filter(|line| !line.trim().is_empty()) {
            agent.say(turn::KIND_RESULT, line.trim());
        }

        // The applications this exchange acted on and was told `done`: the
        // ones whose answer is worth waiting for before asking the model
        // anything else.
        let mut expected: HashSet<String> = HashSet::new();
        let mut results = Vec::new();
        // A run of consecutive actions is sent as a run, and their outcomes
        // collected afterwards. See `pipelined_acts` for why that is worth
        // doing and why it changes none of the answers.
        let mut pending = calls.iter().cloned().peekable();
        while let Some((id, name, input)) = pending.next() {
            if name == "act" {
                let mut run = vec![(id, input)];
                while pending.peek().is_some_and(|(_, next, _)| next == "act") {
                    let (id, _, input) = pending.next().expect("peeked");
                    run.push((id, input));
                }
                results.extend(batched_acts(&mut agent, run, &mut expected));
                continue;
            }
            let (content, is_error) = run_tool(&mut agent, &name, &input);
            // An application that opened answers with its first tree, which
            // is worth the wait for the same reason a click's answer is.
            if name == "open_app"
                && !is_error
                && let Some(app) = field(&input, "name")
            {
                expected.insert(app);
            }
            results.push(Block::ToolResult { id, content, is_error });
        }

        // The automatic re-read. The haimanager says which applications
        // changed while the tools ran, and how; the present state of each
        // rides back with the results, so the model sees the consequences
        // of its actions without spending an exchange asking. The wait is
        // for those consequences to have arrived, and no longer.
        let notices = consequences::await_notices(&mut agent, &expected);
        if let Some(refreshed) = consequences::describe(&mut agent, &notices) {
            results.push(Block::Text(refreshed));
        }

        // Whether this turn is getting anywhere, which is a property of the
        // turn and not of any one call.
        //
        // Two earlier versions of this got the unit wrong. Counting a call by
        // its name and arguments called a directory walk a loop, because `up`
        // is clicked from every folder. Counting whether that call changed
        // anything called it a loop too, because `up` from anywhere under
        // /apps lands back at /apps every time, an identical state reached
        // honestly. Both were asking of one call a question only the turn can
        // answer: an exchange that reaches a state this turn has never been
        // in is progress, however ordinary its calls look, and a run of
        // exchanges that reach only states already visited is a cycle,
        // however varied.
        let fresh = states.insert(digest(&results));
        stale = if fresh { 0 } else { stale + 1 };

        if stale >= STUCK {
            agent.say(
                turn::KIND_ERROR,
                &format!("stopping: {stale} exchanges in a row changed nothing"),
            );
            agent.finish(&format!(
                "I got stuck. The last {stale} things I tried all left the workspace exactly as \
                 it already was, so I stopped rather than keep going. Something I assumed about \
                 this machine is wrong, and I could not work out what from here."
            ));
            return;
        }

        // Say so, before giving up on it. A model going in circles has
        // usually not noticed that it is, and being told plainly is often
        // enough; the alternative is a turn that ends in silence with no
        // account of why.
        if let (true, Some(Block::ToolResult { content, .. })) =
            (stale >= NUDGE, results.last_mut())
        {
            {
                content.push_str(&format!(
                    "\n\n(Nothing has changed on this machine for {stale} exchanges: everything \
                     you have tried lately leaves it in a state you have already seen. Repeating \
                     will not change it. Try something different, or say what is blocking you \
                     and stop.)"
                ));
            }
        }

        messages.push(ModelMessage { role: Role::Assistant, content: assistant.content });
        messages.push(ModelMessage { role: Role::User, content: results });
    }
}

/// One model exchange, with the deltas streamed into the pane as they come:
/// thinking line by line as it arrives, and a note when a tool call starts
/// composing.
fn exchange(
    agent: &mut Agent,
    model: &mut dyn Backend,
    messages: &[ModelMessage],
    tools: &[ToolDef],
) -> Result<Assistant, backend::BackendError> {
    // Thinking is spoken a line at a time: a delta is a fragment, and a pane
    // line per fragment would be confetti.
    let mut thinking = String::new();
    let mut pending: Vec<(&'static str, String)> = Vec::new();
    let started = Instant::now();
    // When the first anything arrived. Measured here rather than in a backend
    // so that every backend reports it the same way, and so that it covers
    // the whole of what the harness waited for, retries and handshakes
    // included.
    let mut first: Option<Duration> = None;
    {
        let mut on = |delta: Delta| {
            first.get_or_insert_with(|| started.elapsed());
            match delta {
                Delta::Thinking(piece) => {
                    thinking.push_str(&piece);
                    while let Some(at) = thinking.find('\n') {
                        let line: String = thinking.drain(..=at).collect();
                        let line = line.trim();
                        if !line.is_empty() {
                            pending.push((turn::KIND_THOUGHT, line.to_owned()));
                        }
                    }
                }
                // Text is not spoken as it streams: whether it is narration or
                // the reply is only known once the exchange is complete, and the
                // pane should not show the reply twice.
                Delta::Text(_) => {}
                Delta::ToolCallStarted(_) => {
                    let rest = thinking.trim();
                    if !rest.is_empty() {
                        pending.push((turn::KIND_THOUGHT, rest.to_owned()));
                    }
                    thinking.clear();
                }
            }
        };
        // The deltas are collected rather than spoken inside the callback,
        // because speaking needs the desk channel and the callback runs
        // inside the backend. Flushed the moment the exchange returns; a
        // turn's pacing comes from the model's own streaming.
        let result = model.respond(SYSTEM, messages, tools, &mut on);
        let spent = started.elapsed();
        let rest = thinking.trim();
        if !rest.is_empty() {
            pending.push((turn::KIND_THOUGHT, rest.to_owned()));
        }
        for (kind, line) in pending {
            agent.say(kind, &line);
        }
        match &result {
            Ok(assistant) => {
                let calls = assistant.tool_uses().count();
                agent.meter.exchange(spent, first, &assistant.usage, calls);
            }
            // A failed exchange still cost what it cost, and a turn that ends
            // on one should say where the time went rather than nothing.
            Err(_) => agent.meter.exchange(spent, first, &Usage::default(), 0),
        }
        result
    }
}

/// End the turn on a completed assistant answer, honestly for each way the
/// model can have stopped.
fn finish_turn(agent: &mut Agent, assistant: &Assistant) {
    let text = assistant.text();
    let reply = match &assistant.stop {
        Stop::EndTurn if text.is_empty() => "I finished, but the model returned no text.".to_owned(),
        Stop::EndTurn => text,
        Stop::MaxTokens => {
            agent.say(turn::KIND_ERROR, "the model ran out of room mid-answer");
            if text.is_empty() {
                "The model ran out of room before it could answer.".to_owned()
            } else {
                format!("{text}\n\n(I ran out of room there.)")
            }
        }
        Stop::Refusal => {
            agent.say(turn::KIND_ERROR, "the model declined this request");
            "The model declined this request.".to_owned()
        }
        Stop::ToolUse => {
            // Cannot happen: a turn with tool calls loops instead. If it
            // does, say so rather than inventing an answer.
            "The model stopped mid-work.".to_owned()
        }
        Stop::Other(reason) => {
            agent.say(turn::KIND_ERROR, &format!("the model stopped for a reason this agent does not know: {reason}"));
            if text.is_empty() { format!("The turn ended ({reason}).") } else { text }
        }
    };
    log("turn complete");
    agent.finish(&reply);
}

// ---- the tools -------------------------------------------------------------------

/// What the model may do, which is exactly what the agent surface offers: read
/// the workspace, act on it by intent, and ask the workspace to open an
/// application. The closed action vocabulary becomes a closed schema, which is
/// the payoff of the whole design: the model cannot ask for anything the
/// compositor would not police.
fn tool_definitions(installed: &[turn::InstalledApp]) -> Vec<ToolDef> {
    let names: Vec<&str> = installed.iter().map(|app| app.name.as_str()).collect();
    // The applications this machine has, as a closed set, exactly as the
    // action vocabulary is. A model cannot ask for an application that is not
    // here, which is the same guarantee `act` gets from its enum and for the
    // same reason: the schema is where a request stops being possible, not
    // where it starts being refused.
    //
    // This is what a whole turn was lost to. Asked for a spreadsheet with
    // nothing open, the model had `list_apps`, which answers with what is
    // *running*, and an example list in this description that happened not to
    // mention `awsheet`. So it guessed `awspreadsheet`, was told it did not
    // open, and guessed again, and there was nothing it could have called
    // that would have told it the truth.
    let opens = if names.is_empty() {
        json!({"type": "string", "description": "The application to open"})
    } else {
        json!({
            "type": "string",
            "enum": names,
            "description": "The application to open. These are all of them; \
                            search_apps says what each one is for.",
        })
    };
    vec![
        ToolDef {
            name: "list_apps",
            description: "List the applications open in this workspace, as markup naming \
                          each app. Call this first, and again after open_app.",
            schema: json!({"type": "object", "properties": {}, "additionalProperties": false}),
        },
        ToolDef {
            name: "read_app",
            // Deliberately does not say to read again after acting: the
            // system prompt says the opposite, because a changed application's
            // fresh view rides back with the tool results already. When this
            // description said so too, the model obeyed the nearer of the two
            // and spent a whole exchange re-reading what it had just been given.
            description: "Read one open application's interface as reduced semantic \
                          markup: every control's id, description, state, and the actions \
                          it currently accepts.",
            schema: json!({
                "type": "object",
                "properties": {
                    "app": {"type": "string", "description": "The application's name as list_apps gave it"}
                },
                "required": ["app"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "search_apps",
            description: "Find the application for a job, by saying what you want to do: \
                          \"spreadsheet\", \"edit text\", \"browse files\". Answers with the \
                          applications this machine has that match, what each is for, and \
                          whether it is already open. Use this before open_app whenever you \
                          are not certain which application you want; guessing a name wastes \
                          the turn.",
            schema: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "What you want to do, or part of an application's \
                                        name. Empty lists everything."
                    }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "read_cells",
            description: "Read part of a spreadsheet. Its cells are not in the view, \
                          because a screenful of a grid is hundreds of them; this reads \
                          the rectangle you name, as one line per row with values \
                          separated by tabs. The element's used attribute says where \
                          anything has been put in the sheet.",
            schema: json!({
                "type": "object",
                "properties": {
                    "app": {"type": "string", "description": "The application's name"},
                    "id": {"type": "string", "description": "The spreadsheet's id, as the view gives it"},
                    "range": {
                        "type": "string",
                        "description": "A rectangle like A1:D20, or one cell like B7"
                    }
                },
                "required": ["app", "id", "range"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "act",
            description: "Perform one action on one control, by id. The compositor brings \
                          the app's window to the front itself, moves the visible cursor \
                          to the control, and performs the action as a human would. The \
                          result is done, or a rejection naming why.",
            schema: json!({
                "type": "object",
                "properties": {
                    "app": {"type": "string", "description": "The application's name"},
                    "action": {
                        "type": "string",
                        "enum": ["focus", "click", "type-text", "clear", "submit", "check",
                                 "uncheck", "toggle", "select", "select-range", "deselect",
                                 "set-value", "open", "close", "move"],
                        "description": "What to do"
                    },
                    "target": {"type": "string", "description": "The control's id"},
                    "value": {"type": "string", "description": "The text for type-text, the number for set-value, or the other corner's cell id for select-range, or the position for move"}
                },
                "required": ["app", "action", "target"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "open_app",
            description: "Ask the workspace to open one of this machine's installed \
                          applications, then wait for it to appear. The result lists what \
                          is open afterwards.",
            schema: json!({
                "type": "object",
                "properties": {"name": opens},
                "required": ["name"],
                "additionalProperties": false
            }),
        },
    ]
}

/// One field of a tool call's input.
///
/// Absent is `None`, not an empty string: the schema marks what is required,
/// but a schema is a request, and a model that omits a field anyway is told
/// which one rather than having "" forwarded to the compositor as if it were
/// a name.
fn field(input: &Value, key: &str) -> Option<String> {
    match &input[key] {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// A number standing for what an exchange left behind: every tool result, and
/// the fresh views that rode back with them.
///
/// Two exchanges with the same digest changed nothing between them, whatever
/// their calls claimed to do.
fn digest(results: &[Block]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for block in results {
        match block {
            Block::ToolResult { content, .. } => content.hash(&mut hasher),
            Block::Text(text) => text.hash(&mut hasher),
            _ => {}
        }
    }
    hasher.finish()
}

/// The applications matching a query, with what each is for and whether it is
/// already running.
///
/// Matching is by word against the name, the label and the description
/// together, because the useful query is what someone wants to *do* rather
/// than what an application is called. That is the whole point: `awsheet` is
/// a guess and "A spreadsheet. Reads and writes CSV files" is an answer, and
/// the descriptions were already written and shipped for exactly this.
///
/// A query that matches nothing answers with everything rather than with
/// nothing, because a search is how the agent finds out what is here and a
/// dead end sends it back to guessing, which is what this exists to stop.
fn search_apps(installed: &[turn::InstalledApp], query: &str, open: &str) -> String {
    if installed.is_empty() {
        return "This agentdesk did not say what is installed, so nothing can be searched. \
                Use list_apps to see what is already open."
            .to_owned();
    }

    let words: Vec<String> = query
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| word.len() > 2)
        .map(str::to_owned)
        .collect();

    let score = |app: &turn::InstalledApp| -> usize {
        let haystack =
            format!("{} {} {}", app.name, app.label, app.description).to_lowercase();
        words.iter().filter(|word| haystack.contains(word.as_str())).count()
    };

    let mut ranked: Vec<(usize, &turn::InstalledApp)> =
        installed.iter().map(|app| (score(app), app)).collect();
    let any = ranked.iter().any(|(score, _)| *score > 0);
    if any {
        ranked.retain(|(score, _)| *score > 0);
    }
    // Stable within a score, so the order is the desk's alphabetical one and
    // does not shuffle between identical queries.
    ranked.sort_by_key(|(score, _)| std::cmp::Reverse(*score));

    let mut out = if any {
        format!("Applications matching {query:?}:\n")
    } else {
        format!("Nothing matched {query:?}. Everything this machine has:\n")
    };
    for (_, app) in ranked {
        // The compositor's answer names the open applications; a name it
        // contains is one that is running.
        let running = if open.contains(&app.name) { "already open" } else { "not open" };
        out.push_str(&format!(
            "\n{} ({}), {}\n  {}\n",
            app.name, app.label, running, app.description
        ));
    }
    out.push_str("\nOpen one with open_app, naming it exactly as above.");
    out
}

/// Perform a batch of actions: in order, each the moment the last was
/// answered, until every one is done or one is refused.
///
/// A batch is the model saying "these, in this order", and it is performed
/// exactly so. Nothing waits between them: the haimanager answers an intent
/// when it has checked it and synthesized its events, which is microseconds,
/// and the application receives the events vouched for so it acts on every
/// one whatever version it has moved on to. The first refusal ends the batch,
/// because what the model asked for after that was asked for on the
/// assumption the refused step had happened; those are answered as not
/// attempted, naming the step that stopped them, so the model knows exactly
/// where the machine is.
///
/// Every application told `done` is added to `expected`: its answer to the
/// action is on its way, and the exchange waits for it.
fn batched_acts(
    agent: &mut Agent,
    run: Vec<(String, Value)>,
    expected: &mut HashSet<String>,
) -> Vec<Block> {
    let started = Instant::now();
    let mut results = Vec::new();
    // What stopped the batch, once something has.
    let mut stopped: Option<String> = None;
    for (id, input) in &run {
        let (Some(app), Some(action), Some(target)) = (
            field(input, "app"),
            field(input, "action"),
            field(input, "target"),
        ) else {
            results.push(Block::ToolResult {
                id: id.clone(),
                content: "act needs an app, an action and a target".to_owned(),
                is_error: true,
            });
            continue;
        };
        // The one genuinely optional field: most actions carry no payload,
        // and an absent one is the wire's empty value.
        let value = field(input, "value").unwrap_or_default();
        let told = if value.is_empty() {
            format!("{action} {target} in {app}")
        } else {
            format!("{action} {value:?} into {target} in {app}")
        };
        if let Some(why) = &stopped {
            agent.say(turn::KIND_ACTION, &format!("{told}: not attempted"));
            results.push(Block::ToolResult {
                id: id.clone(),
                content: format!("not attempted: the batch stopped when {why}"),
                is_error: false,
            });
            continue;
        }
        let outcome = match agent.link.act(&app, &action, &target, &value) {
            Ok(outcome) => outcome,
            Err(err) => connection_lost(&err),
        };
        agent.meter.actions += 1;
        let content = match outcome {
            Outcome::Done => {
                agent.say(turn::KIND_ACTION, &format!("{told}: done"));
                expected.insert(app);
                "done".to_owned()
            }
            Outcome::Rejected(reason) => {
                agent.say(turn::KIND_ACTION, &format!("{told}: {reason}"));
                stopped = Some(format!("{told} was rejected ({reason})"));
                // A rejection is an answer the model reasons about, not an
                // error: `blocked` says to look for the dialog and `disabled`
                // says the application must change first. is_error stays
                // false.
                format!("rejected: {reason}. The rest of this batch was not attempted.")
            }
        };
        results.push(Block::ToolResult { id: id.clone(), content, is_error: false });
    }

    let spent = started.elapsed();
    agent.meter.acting += spent;
    log(&format!("{} action(s) in {}ms", run.len(), spent.as_millis()));
    results
}

/// Execute one tool call: the model's request becomes a query or an intent,
/// and whatever comes back becomes the tool result, rejections included.
fn run_tool(agent: &mut Agent, name: &str, input: &Value) -> (String, bool) {
    let field = |key: &str| field(input, key);

    match name {
        "list_apps" => {
            agent.say(turn::KIND_ACTION, "reading what is open");
            let asked = Instant::now();
            let answer = agent.link.apps();
            agent.meter.reading += asked.elapsed();
            match answer {
                Ok(markup) => (markup, false),
                Err(err) => connection_lost(&err),
            }
        }
        "search_apps" => {
            let query = field("query").unwrap_or_default();
            agent.say(turn::KIND_ACTION, &format!("looking for an app for {query:?}"));
            // Which of them are running, so the answer settles both questions
            // at once: an application that is already open needs no open_app,
            // and finding that out used to be a second call.
            let asked = Instant::now();
            let open = agent.link.apps().unwrap_or_default();
            agent.meter.reading += asked.elapsed();
            (search_apps(&agent.installed, &query, &open), false)
        }
        "read_app" => {
            let Some(app) = field("app") else {
                return ("read_app needs an app name".to_owned(), true);
            };
            agent.say(turn::KIND_ACTION, &format!("reading {app}"));
            let asked = Instant::now();
            let answer = agent.link.view(&app);
            agent.meter.reading += asked.elapsed();
            match answer {
                Ok(markup) if markup.is_empty() => {
                    (format!("no view came back for {app:?}; is it open?"), true)
                }
                Ok(markup) => (markup, false),
                Err(err) => connection_lost(&err),
            }
        }
        "read_cells" => {
            let (Some(app), Some(id), Some(range)) =
                (field("app"), field("id"), field("range"))
            else {
                return ("read_cells needs an app, an id and a range".to_owned(), true);
            };
            agent.say(turn::KIND_ACTION, &format!("reading {range} of {id} in {app}"));
            let asked = Instant::now();
            let answer = agent.link.cells(&app, &id, &range);
            agent.meter.reading += asked.elapsed();
            match answer {
                Ok(block) if block.is_empty() => {
                    (format!("nothing came back for {range} of {id:?} in {app:?}"), true)
                }
                Ok(block) => (block, false),
                Err(err) => connection_lost(&err),
            }
        }
        // "act" never arrives here: the loop gathers a run of them and hands
        // it to `pipelined_acts`, so that a run of one and a run of six go
        // down the same path.
        "open_app" => {
            let Some(name) = field("name") else {
                return ("open_app needs an application name".to_owned(), true);
            };
            // An application that is not installed will not appear however
            // long anyone waits, and waiting the full five seconds to say so
            // is five seconds of a turn spent learning nothing.
            if !agent.installed.is_empty()
                && !agent.installed.iter().any(|app| app.name == name)
            {
                let known: Vec<&str> =
                    agent.installed.iter().map(|app| app.name.as_str()).collect();
                let known = known.join(", ");
                agent.say(turn::KIND_ERROR, &format!("there is no application called {name}"));
                return (
                    format!(
                        "There is no application called {name:?} on this machine. \
                         The installed ones are: {known}."
                    ),
                    true,
                );
            }
            agent.say(turn::KIND_ACTION, &format!("opening {name}"));
            if let Err(err) = agent.desk.open_app(&name) {
                return (format!("could not ask the workspace to open {name:?}: {err}"), true);
            }
            let waited = Instant::now();
            // Look before sleeping. An application the model asked for that
            // is already open answers on the first look, and used to cost a
            // poll interval for nothing.
            loop {
                match agent.link.apps() {
                    Ok(markup) if markup.contains(&name) => {
                        agent.meter.waiting += waited.elapsed();
                        agent.say(turn::KIND_RESULT, &format!("{name} is open"));
                        log(&format!("open {name}: {}ms", waited.elapsed().as_millis()));
                        return (format!("{name} is open. Open applications:\n{markup}"), false);
                    }
                    Ok(markup) if waited.elapsed() >= OPENING => {
                        agent.meter.waiting += waited.elapsed();
                        agent.say(turn::KIND_ERROR, &format!("{name} did not open"));
                        return (
                            format!(
                                "{name} did not appear within {}s. Open applications:\n{markup}",
                                OPENING.as_secs()
                            ),
                            true,
                        );
                    }
                    Ok(_) => std::thread::sleep(OPENING_POLL),
                    Err(err) => connection_lost(&err),
                }
            }
        }
        other => (format!("there is no tool named {other:?}"), true),
    }
}

/// The compositor hung up. There is no screen left to act on, so the turn
/// cannot mean anything more; exit rather than have the model reason about a
/// workspace that is gone.
fn connection_lost(err: &std::io::Error) -> ! {
    log(&format!("the compositor connection failed: {err}"));
    std::process::exit(1);
}

/// Log to the kernel ring buffer.
///
/// The agent's real output is telemetry to its agentdesk; the log carries a
/// copy so a serial capture tells the same story the pane does.
fn log(message: &str) {
    let line = format!("<6>awagent: {message}\n");
    if let Ok(mut kmsg) = std::fs::OpenOptions::new().write(true).open("/dev/kmsg") {
        let _ = kmsg.write_all(line.as_bytes());
    } else {
        eprintln!("awagent: {message}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(name: &str, label: &str, description: &str) -> turn::InstalledApp {
        turn::InstalledApp {
            name: name.into(),
            label: label.into(),
            description: description.into(),
        }
    }

    fn machine() -> Vec<turn::InstalledApp> {
        vec![
            app("awcalc", "Calculator", "A pocket calculator. Adds, subtracts, multiplies \
                 and divides, one press at a time, the way a human would use it."),
            app("awfiles", "Files", "A file explorer. Walks folders, marks files and \
                 folders, and makes, renames, copies, moves and deletes them."),
            app("awsheet", "Sheet", "A spreadsheet. Reads and writes CSV files, one open \
                 file per tab."),
            app("awtext", "Text", "A text editor. Reads and writes plain text files, one \
                 open file per tab."),
        ]
    }

    #[test]
    fn the_installed_set_is_closed() {
        // A model cannot ask for an application this machine does not have,
        // for the same reason it cannot ask for a verb the compositor does
        // not police: the schema is where the request stops being possible.
        let tools = tool_definitions(&machine());
        let open = tools.iter().find(|tool| tool.name == "open_app").expect("open_app");
        assert_eq!(
            open.schema["properties"]["name"]["enum"],
            json!(["awcalc", "awfiles", "awsheet", "awtext"])
        );

        // A machine that said nothing about what it has takes any name, which
        // is what an older agentdesk gets rather than a turn that cannot open
        // anything at all.
        let tools = tool_definitions(&[]);
        let open = tools.iter().find(|tool| tool.name == "open_app").expect("open_app");
        assert!(open.schema["properties"]["name"].get("enum").is_none());
    }

    #[test]
    fn read_app_does_not_tell_the_model_to_read_again() {
        // The system prompt says the opposite, because a changed app's fresh
        // view rides back with the results already. When both said it, the
        // model obeyed the nearer one and spent an exchange on nothing.
        let tools = tool_definitions(&[]);
        let read = tools.iter().find(|tool| tool.name == "read_app").expect("read_app");
        assert!(!read.description.contains("again"));
    }

    #[test]
    fn searching_finds_an_app_by_what_it_is_for() {
        // The exact failure this exists for: the model wanted a spreadsheet,
        // the application is called awsheet, and nothing it could call would
        // have told it so. It guessed awspreadsheet and looped until the
        // human stopped it.
        let found = search_apps(&machine(), "spreadsheet", "");
        let first = found.lines().find(|line| line.starts_with("aw")).expect("a match");
        assert!(first.starts_with("awsheet"), "wanted awsheet first, got {first:?}");
        assert!(found.contains("Reads and writes CSV files"));

        // What someone wants to do, not what it is called.
        assert!(search_apps(&machine(), "edit some text", "").contains("awtext"));
        assert!(search_apps(&machine(), "browse folders on disk", "").contains("awfiles"));
    }

    #[test]
    fn searching_says_what_is_already_open() {
        // Both questions answered at once: an app that is already open needs
        // no open_app, and finding that out used to be a second call.
        let open = "<apps><app name=\"awfiles\"/></apps>";
        let found = search_apps(&machine(), "files", open);
        assert!(found.contains("awfiles (Files), already open"), "{found}");
        let found = search_apps(&machine(), "spreadsheet", open);
        assert!(found.contains("awsheet (Sheet), not open"), "{found}");
    }

    #[test]
    fn a_search_that_matches_nothing_still_answers() {
        // A dead end sends the agent back to guessing, which is the whole
        // failure this replaces. Nothing matching lists everything instead.
        let found = search_apps(&machine(), "photoshop", "");
        assert!(found.contains("Nothing matched"));
        for name in ["awcalc", "awfiles", "awsheet", "awtext"] {
            assert!(found.contains(name), "{name} missing from {found:?}");
        }
        // An empty query is a listing, not an error.
        assert!(search_apps(&machine(), "", "").contains("awsheet"));
        // A desk that said nothing says so rather than pretending to be empty.
        assert!(search_apps(&[], "spreadsheet", "").contains("did not say"));
    }

    /// The staleness counter, exactly as the loop keeps it.
    fn walk(states: &mut HashSet<u64>, stale: &mut u32, after: u64) -> u32 {
        *stale = if states.insert(after) { 0 } else { *stale + 1 };
        *stale
    }

    #[test]
    fn a_turn_reaching_new_states_is_never_stuck() {
        // Walking a directory tree returns to the parent constantly, so the
        // same state recurs; what makes it a search rather than a loop is
        // that new ones keep appearing between the repeats. This is the
        // exact shape that tripped two earlier versions of the detector.
        let (mut states, mut stale) = (HashSet::new(), 0);
        let apps = 100;
        for folder in 1..=12 {
            // Into a folder nobody has seen, then back up to /apps.
            assert_eq!(walk(&mut states, &mut stale, folder), 0);
            assert!(walk(&mut states, &mut stale, apps) <= NUDGE, "up looked like a loop");
        }
    }

    #[test]
    fn a_turn_that_only_revisits_is_stuck() {
        // Asking for an application that does not exist, then opening one
        // that does, forever: two states, alternating, neither new after the
        // first pass. Varied calls, no progress.
        let (mut states, mut stale) = (HashSet::new(), 0);
        assert_eq!(walk(&mut states, &mut stale, 1), 0);
        assert_eq!(walk(&mut states, &mut stale, 2), 0);
        let mut rounds = 0;
        while stale < STUCK {
            walk(&mut states, &mut stale, if rounds % 2 == 0 { 1 } else { 2 });
            rounds += 1;
            assert!(rounds < 50, "an alternating cycle should have been caught");
        }
        assert_eq!(stale, STUCK);

        // One genuinely new state clears it, however deep in it was.
        assert_eq!(walk(&mut states, &mut stale, 3), 0);

        const { assert!(NUDGE < STUCK) };
    }

    #[test]
    fn the_digest_is_of_what_came_back() {
        let done = |text: &str| {
            vec![Block::ToolResult { id: "c1".into(), content: text.into(), is_error: false }]
        };
        assert_eq!(digest(&done("done")), digest(&done("done")));
        assert_ne!(digest(&done("done")), digest(&done("rejected: blocked")));
        // The same result with a different view is a different state, which
        // is the whole point: `done` says nothing about what moved.
        let mut with_view = done("done");
        with_view.push(Block::Text("<view app=\"awfiles\">/home</view>".into()));
        let mut elsewhere = done("done");
        elsewhere.push(Block::Text("<view app=\"awfiles\">/apps</view>".into()));
        assert_ne!(digest(&with_view), digest(&elsewhere));
    }
}
