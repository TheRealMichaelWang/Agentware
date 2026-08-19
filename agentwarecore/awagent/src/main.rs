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
//! `not-visible`) is an answer handed back to the model as a tool result to
//! reason about, not an error. It never names a workspace, cannot see the
//! agentdesk that started it, and cannot read the transcript of its own
//! streamed thoughts. It cannot open an application itself: it asks the
//! agentdesk, which asks PID 1, and it finds out whether that worked the way
//! it finds out everything, by asking the compositor.
//!
//! Usage: awagent <desk-id>

mod backend;
mod backends;
mod http;

use std::io::Write as _;
use std::time::{Duration, Instant};

use awproto::agent::{Link, Outcome};
use awproto::turn::{self, Turn};
use serde_json::{Value, json};

use backend::{Assistant, Backend, Block, Delta, ModelMessage, Role, Stop, ToolDef};

/// How long to wait for an application to appear after asking for it. An
/// application is forked by PID 1 and attaches when its first tree arrives,
/// which is quick, but not instant.
const OPENING: Duration = Duration::from_secs(5);

/// How often to look while waiting for one.
const OPENING_POLL: Duration = Duration::from_millis(300);

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
click, type-text, clear, submit, check, uncheck, toggle, select, deselect, set-value, \
open, close, scroll-into-view.
- The compositor stages every action itself: the target's window comes to the front and \
its siblings are put away before your action lands. You cannot move, resize or arrange \
windows, and never need to.
- type-text replaces a field's contents with the given value, typed on screen one \
keystroke at a time. submit is the Enter key. check and uncheck are unconditional; prefer \
them over toggle so the outcome does not depend on stale state.
- scroll-into-view makes a node visible when it is scrolled away; name the node you want \
seen, never a container.

Rejections are answers, not failures:
- disabled: the application has disabled it; something in the app must change first.
- blocked: a dialog is in front; read the view and answer the dialog.
- not-visible: scrolled out of view; scroll-into-view is the remedy.
- no-such-node, no-such-app: nothing by that name; read again before retrying.
- unsupported-action: that element does not take that action.
- needs-approval: the human must approve it; say what you wanted to do and stop.

Working style:
- Read before acting: list_apps, then read_app, then act. Open what the work needs with \
open_app.
- When an application's interface changes, after your actions or on its own, its fresh \
view is attached to your tool results automatically, marked as re-read for you. You \
therefore rarely need read_app to confirm a result; use it to look at an app you have \
not just seen.
- Plain text you write between tool calls is shown to the human as progress narration; \
keep it to a line.
- Your final message, with no tool call, ends the turn and joins the conversation as \
your reply. Lead with the outcome.";

/// The agent's two channels, and the words that go up the second.
struct Agent {
    link: Link,
    desk: Turn,
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
    fn finish(&mut self, reply: &str) {
        if let Err(err) = self.desk.reply(reply) {
            log(&format!("could not deliver the reply: {err}"));
        }
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

    let context = match desk.context() {
        Ok(context) => context,
        Err(err) => {
            log(&format!("could not read the context: {err}"));
            std::process::exit(1);
        }
    };
    let mut agent = Agent { link, desk };
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

    let tools = tool_definitions();

    // The agentic loop: ask the model, do what it asks, hand back what
    // happened, until it answers with no tool calls. Every iteration resends
    // the whole conversation; the API's prompt cache makes the resend cheap.
    //
    // No exchange ceiling, deliberately: a long-running turn is the point of
    // an agent, and how long is worth spending is the human's call, made
    // with the working line in front of them and the stop button beside it.
    // Every ending this loop can reach is honest: the model finishes, the
    // backend errors, or the human interrupts.
    loop {
        let assistant = match exchange(&mut agent, model.as_mut(), &messages, &tools) {
            Ok(assistant) => assistant,
            Err(err) => {
                agent.say(turn::KIND_ERROR, &err.message);
                agent.finish(&format!("The turn failed: {}.", err.message));
                return;
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

        let mut acted = false;
        let mut results = Vec::new();
        for (id, name, input) in calls {
            acted |= matches!(name.as_str(), "act" | "open_app");
            let (content, is_error) = run_tool(&mut agent, &name, &input);
            results.push(Block::ToolResult { id, content, is_error });
        }

        // The automatic re-read. The compositor says which applications'
        // trees genuinely changed while the tools ran; their present views
        // ride back with the results, so the model sees the consequences of
        // its actions without spending an exchange asking.
        if let Some(refreshed) = refreshed_views(&mut agent, acted) {
            results.push(Block::Text(refreshed));
        }

        messages.push(ModelMessage { role: Role::Assistant, content: assistant.content });
        messages.push(ModelMessage { role: Role::User, content: results });
    }
}

/// How long an application gets to re-render after an action before the
/// changed set is drained. An app answers an event in milliseconds; this is
/// generous for that and nothing against a model exchange.
const SETTLE: Duration = Duration::from_millis(150);

/// The present views of whatever changed while the tools ran, or `None` when
/// nothing did.
///
/// This is the pull model kept honest rather than replaced: the compositor
/// never pushes a tree, only the name of an app whose tree moved, and the
/// harness answers with the same `read_app` the model would have had to
/// spend a whole model exchange asking for. The model receives state, never
/// a diff, so there is nothing to misapply.
fn refreshed_views(agent: &mut Agent, acted: bool) -> Option<String> {
    if acted {
        // An action's consequences appear one app round trip later, which is
        // moments after the intent resolved; without the pause the drain
        // would race the very re-render it exists to catch.
        std::thread::sleep(SETTLE);
    }
    let changed = match agent.link.take_changed() {
        Ok(changed) => changed,
        Err(err) => {
            log(&format!("could not drain change notices: {err}"));
            return None;
        }
    };
    if changed.is_empty() {
        return None;
    }

    let mut text = String::from(
        "The workspace changed while you worked. The present state, re-read for you:\n",
    );
    for app in changed {
        agent.say(turn::KIND_ACTION, &format!("re-reading {app} (it changed)"));
        match agent.link.view(&app) {
            Ok(markup) if !markup.is_empty() => {
                text.push('\n');
                text.push_str(&markup);
                text.push('\n');
            }
            Ok(_) => {}
            Err(err) => {
                log(&format!("could not re-read {app}: {err}"));
                return Some(text);
            }
        }
    }
    Some(text)
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
    {
        let mut on = |delta: Delta| match delta {
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
        };
        // The deltas are collected rather than spoken inside the callback,
        // because speaking needs the desk channel and the callback runs
        // inside the backend. Flushed the moment the exchange returns; a
        // turn's pacing comes from the model's own streaming.
        let result = model.respond(SYSTEM, messages, tools, &mut on);
        let rest = thinking.trim();
        if !rest.is_empty() {
            pending.push((turn::KIND_THOUGHT, rest.to_owned()));
        }
        for (kind, line) in pending {
            agent.say(kind, &line);
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
fn tool_definitions() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "list_apps",
            description: "List the applications open in this workspace, as markup naming \
                          each app. Call this first, and again after open_app.",
            schema: json!({"type": "object", "properties": {}, "additionalProperties": false}),
        },
        ToolDef {
            name: "read_app",
            description: "Read one open application's interface as reduced semantic \
                          markup: every control's id, description, state, and the actions \
                          it currently accepts. Read again after acting rather than \
                          assuming a result.",
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
                                 "uncheck", "toggle", "select", "deselect", "set-value",
                                 "open", "close", "scroll-into-view"],
                        "description": "What to do"
                    },
                    "target": {"type": "string", "description": "The control's id"},
                    "value": {"type": "string", "description": "The text for type-text, or the number for set-value"}
                },
                "required": ["app", "action", "target"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "open_app",
            description: "Ask the workspace to open an installed application, then wait \
                          for it to appear. The result lists what is open afterwards.",
            schema: json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "The application to open, e.g. awcalc, awfiles, awsettings"}
                },
                "required": ["name"],
                "additionalProperties": false
            }),
        },
    ]
}

/// Execute one tool call: the model's request becomes a query or an intent,
/// and whatever comes back becomes the tool result, rejections included.
fn run_tool(agent: &mut Agent, name: &str, input: &Value) -> (String, bool) {
    // Absent is `None`, not an empty string: the schema marks what is
    // required, but a schema is a request, and a model that omits a field
    // anyway is told which one rather than having "" forwarded to the
    // compositor as if it were a name.
    let field = |key: &str| -> Option<String> {
        match &input[key] {
            Value::String(text) => Some(text.clone()),
            Value::Number(number) => Some(number.to_string()),
            _ => None,
        }
    };

    match name {
        "list_apps" => {
            agent.say(turn::KIND_ACTION, "reading what is open");
            match agent.link.apps() {
                Ok(markup) => (markup, false),
                Err(err) => connection_lost(&err),
            }
        }
        "read_app" => {
            let Some(app) = field("app") else {
                return ("read_app needs an app name".to_owned(), true);
            };
            agent.say(turn::KIND_ACTION, &format!("reading {app}"));
            match agent.link.view(&app) {
                Ok(markup) if markup.is_empty() => {
                    (format!("no view came back for {app:?}; is it open?"), true)
                }
                Ok(markup) => (markup, false),
                Err(err) => connection_lost(&err),
            }
        }
        "act" => {
            let (Some(app), Some(action), Some(target)) =
                (field("app"), field("action"), field("target"))
            else {
                return ("act needs an app, an action and a target".to_owned(), true);
            };
            // The one genuinely optional field: most actions carry no payload,
            // and an absent one is the wire's empty value.
            let value = field("value").unwrap_or_default();
            let outcome = match agent.link.act(&app, &action, &target, &value) {
                Ok(outcome) => outcome,
                Err(err) => return connection_lost(&err),
            };
            let told = if value.is_empty() {
                format!("{action} {target} in {app}")
            } else {
                format!("{action} {value:?} into {target} in {app}")
            };
            match outcome {
                Outcome::Done => {
                    agent.say(turn::KIND_ACTION, &format!("{told}: done"));
                    ("done".to_owned(), false)
                }
                Outcome::Rejected(reason) => {
                    agent.say(turn::KIND_ACTION, &format!("{told}: {reason}"));
                    // A rejection is an answer the model reasons about, not
                    // an error: `blocked` says to look for the dialog,
                    // `not-visible` says to scroll. is_error stays false.
                    (format!("rejected: {reason}"), false)
                }
            }
        }
        "open_app" => {
            let Some(name) = field("name") else {
                return ("open_app needs an application name".to_owned(), true);
            };
            agent.say(turn::KIND_ACTION, &format!("opening {name}"));
            if let Err(err) = agent.desk.open_app(&name) {
                return (format!("could not ask the workspace to open {name:?}: {err}"), true);
            }
            let waited = Instant::now();
            loop {
                std::thread::sleep(OPENING_POLL);
                match agent.link.apps() {
                    Ok(markup) if markup.contains(&name) => {
                        agent.say(turn::KIND_RESULT, &format!("{name} is open"));
                        return (format!("{name} is open. Open applications:\n{markup}"), false);
                    }
                    Ok(markup) if waited.elapsed() >= OPENING => {
                        agent.say(turn::KIND_ERROR, &format!("{name} did not open"));
                        return (
                            format!(
                                "{name} did not appear within {}s. Open applications:\n{markup}",
                                OPENING.as_secs()
                            ),
                            true,
                        );
                    }
                    Ok(_) => continue,
                    Err(err) => return connection_lost(&err),
                }
            }
        }
        other => (format!("there is no tool named {other:?}"), true),
    }
}

/// The compositor hung up. There is no screen left to act on, so the turn
/// cannot mean anything more; exit rather than have the model reason about a
/// workspace that is gone.
fn connection_lost(err: &std::io::Error) -> (String, bool) {
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
