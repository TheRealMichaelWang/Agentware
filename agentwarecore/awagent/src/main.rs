//! A stand-in agent: one turn, scripted, then gone.
//!
//! The per-turn worker is the shortest-lived thing in Agentware and owns nothing
//! durable. Interrupting it is a signal to a process with no state, crashing it
//! takes nothing with it, and it is never restarted, because restarting one
//! would silently re-run side effects it had already performed.
//!
//! There is no model here. Two scripts, and the prompt picks: one that mentions
//! a file walks the file browser and its open dialog, anything else adds two
//! numbers on the calculator. Both are chosen to walk every branch of the
//! compositor's intent resolution: the ones that succeed, and each way one can
//! be refused. A rejection is an answer, and an agent that cannot be told no
//! acts blind and retries forever, so the refusals matter at least as much as
//! the successes.
//!
//! Notice what this file cannot do. It never produces an event, only intents. It
//! never names a workspace, because it has no way to name one and is never asked
//! which it is in. It cannot see the agentdesk that started it, so it cannot read
//! the transcript of its own streamed thoughts. It cannot open an application
//! itself: it asks the agentdesk, which asks PID 1, and it finds out whether
//! that worked the way it finds out everything, by asking the compositor.
//!
//! What it says to the agentdesk is the other half of the turn. The context
//! comes down the private channel first, and everything the agent does goes back
//! up it as telemetry, so the human watches the turn in the pane rather than in
//! the kernel log. The reply at the end is what joins the conversation.
//!
//! Usage: awagent <desk-id>

use std::io::Write as _;
use std::time::{Duration, Instant};

use awproto::agent::{Link, Outcome};
use awproto::turn::{self, Turn};

/// A pause between intents, standing in for the time a real agent spends
/// deciding what to do next.
///
/// Not cosmetic. An agent that fires intents as fast as the socket allows is
/// asking for actions against a screen it has not seen the result of, and it
/// makes the fake cursor a blur rather than something a human can follow.
const THINKING: Duration = Duration::from_millis(300);

/// How long to wait for the calculator to appear after asking for it. An
/// application is forked by PID 1 and attaches when its first tree arrives,
/// which is quick, but not instant.
const OPENING: Duration = Duration::from_secs(5);

/// The agent's two channels, and the words that go up the second.
struct Agent {
    link: Link,
    /// The agentdesk that started this turn. Absent only when something other
    /// than an agentdesk did, which is what the self-test does; the turn then
    /// runs with nobody to tell.
    desk: Option<Turn>,
}

impl Agent {
    fn say(&mut self, kind: &str, text: &str) {
        log(text);
        if let Some(desk) = &mut self.desk
            && let Err(err) = desk.telemetry(kind, text)
        {
            log(&format!("could not reach the agentdesk: {err}"));
            self.desk = None;
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
    let desk = match Turn::inherited() {
        Ok(turn) => Some(turn),
        Err(err) => {
            log(&format!("no agentdesk channel ({err}); the turn runs untold"));
            None
        }
    };
    let mut agent = Agent { link, desk };

    // The context. A real agent would think about it; this one reports what
    // it was given so the channel can be seen working end to end, and reads
    // one word of it to pick which of its two scripts to run.
    let mut prompt = String::new();
    if let Some(desk) = &mut agent.desk {
        match desk.context() {
            Ok((history, text)) => {
                let earlier = history.len();
                agent.say(
                    turn::KIND_THOUGHT,
                    &format!("read {earlier} earlier message(s); the prompt is {text:?}"),
                );
                prompt = text;
            }
            Err(err) => {
                log(&format!("could not read the context: {err}"));
                agent.desk = None;
            }
        }
    }

    if prompt.to_lowercase().contains("file") {
        files_turn(&mut agent);
        return;
    }
    calculator_turn(&mut agent);
}

/// Make sure an application is open, asking the workspace for it if not, and
/// waiting until the compositor says it is there. `false` if it never came.
fn ensure_open(agent: &mut Agent, app: &str, label: &str) -> bool {
    let apps = match agent.link.apps() {
        Ok(markup) => markup,
        Err(err) => {
            log(&format!("could not read the workspace: {err}"));
            std::process::exit(1);
        }
    };
    report("apps", &apps);
    if apps.contains(app) {
        return true;
    }

    agent.say(turn::KIND_ACTION, &format!("opening the {label}"));
    if let Some(desk) = &mut agent.desk
        && let Err(err) = desk.open_app(app)
    {
        log(&format!("could not ask for the {label}: {err}"));
    }
    let waited = Instant::now();
    loop {
        std::thread::sleep(THINKING);
        match agent.link.apps() {
            Ok(markup) if markup.contains(app) => break,
            Ok(_) if waited.elapsed() < OPENING => continue,
            Ok(_) => {
                agent.say(turn::KIND_ERROR, &format!("the {label} did not open"));
                return false;
            }
            Err(err) => {
                log(&format!("could not read the workspace: {err}"));
                std::process::exit(1);
            }
        }
    }
    agent.say(turn::KIND_RESULT, &format!("the {label} is open"));
    true
}

/// One intent, told to the human, with the outcome as a string.
fn act(agent: &mut Agent, app: &str, action: &str, target: &str, value: &str) -> String {
    std::thread::sleep(THINKING);
    let outcome = match agent.link.act(app, action, target, value) {
        Ok(outcome) => outcome,
        Err(err) => {
            log(&format!("connection failed: {err}"));
            std::process::exit(1);
        }
    };
    let got = match &outcome {
        Outcome::Done => "done".to_owned(),
        Outcome::Rejected(reason) => reason.clone(),
    };
    let what = if value.is_empty() {
        format!("{action} {target} in {app}")
    } else {
        format!("{action} {value:?} into {target} in {app}")
    };
    agent.say(turn::KIND_ACTION, &format!("{what}: {got}"));
    got
}

/// The file script: open the explorer, mark a file, copy it into a folder
/// chosen through the dialog.
///
/// This is the dialog contract seen from the agent's side. The dialog is
/// controls nested in `<dialog>` in the explorer's own view, so choosing a
/// folder is reading the view and clicking what it lists; and while the
/// dialog is up, the explorer's own controls answer `blocked`, which is the
/// compositor keeping the agent to the same rule the human is under.
fn files_turn(agent: &mut Agent) {
    if !ensure_open(agent, "awfiles", "file explorer") {
        finish(agent, "I could not open the file explorer.");
        return;
    }

    let view = |agent: &mut Agent, why: &str| -> String {
        match agent.link.view("awfiles") {
            Ok(markup) => {
                report(why, &markup);
                markup
            }
            Err(err) => {
                log(&format!("could not read awfiles: {err}"));
                String::new()
            }
        }
    };

    // Mark welcome.txt. A checkbox, so `check`: unconditional, and the
    // compositor turns it into a toggle only if the box is not already checked.
    let markup = view(agent, "view awfiles");
    let Some(mark) = find_control(&markup, |d| d.starts_with("Marks the file welcome.txt")) else {
        finish(agent, "I could not find welcome.txt to mark.");
        return;
    };
    agent.say(turn::KIND_THOUGHT, "marking welcome.txt, then copying it into notes");
    act(agent, "awfiles", "check", &mark, "");
    act(agent, "awfiles", "click", "copy-to", "");

    let markup = view(agent, "view awfiles with the dialog open");
    let dialog_seen = markup.contains("<dialog");
    agent.say(
        turn::KIND_RESULT,
        if dialog_seen { "the view shows a <dialog> asking for a folder" } else { "no dialog in the view" },
    );

    // The explorer's own Up is behind the dialog now. Told `blocked`, not
    // `disabled`: the control is fine, something is in front of it.
    let blocked = act(agent, "awfiles", "click", "up", "");
    if blocked != "blocked" {
        agent.say(turn::KIND_ERROR, &format!("expected blocked for a control behind the dialog, got {blocked}"));
    }

    // Into notes, then choose it.
    if let Some(folder) = find_control(&markup, |d| d == "Enters the folder notes") {
        act(agent, "awfiles", "click", &folder, "");
    }
    act(agent, "awfiles", "click", "file-dialog-confirm", "");

    let after = view(agent, "view awfiles after the dialog");
    let dialog_gone = !after.contains("<dialog");
    let copied = after.contains("copied 1 item(s) to /home/notes");
    agent.say(
        turn::KIND_RESULT,
        if dialog_gone { "the dialog is gone and the explorer answers again" } else { "the dialog is still up" },
    );

    let reply = match (dialog_gone, copied) {
        (true, true) => "Copied welcome.txt into notes through the folder dialog.".to_owned(),
        (true, false) => "The dialog closed, but the explorer does not say the copy happened.".to_owned(),
        (false, _) => "The dialog did not close.".to_owned(),
    };
    finish(agent, &reply);
}

/// The id of the first control in a view whose description satisfies `wanted`
/// and that can currently be acted on. The action list is the authority: a
/// control that is disabled, or behind a dialog, has an empty one, and an
/// agent that reads it never sends an intent that will be refused.
fn find_control(view: &str, wanted: impl Fn(&str) -> bool) -> Option<String> {
    view.lines().find_map(|line| {
        let description = attribute(line, "description")?;
        let actions = attribute(line, "actions")?;
        (wanted(&description) && !actions.is_empty())
            .then(|| attribute(line, "id"))
            .flatten()
    })
}

/// One attribute's value out of a line of markup. Enough of a parser for a
/// view the compositor wrote; a real agent would have a real one.
fn attribute(line: &str, name: &str) -> Option<String> {
    let start = line.find(&format!(" {name}=\""))? + name.len() + 3;
    let end = line[start..].find('"')? + start;
    Some(unescape(&line[start..end]))
}

fn unescape(text: &str) -> String {
    text.replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// The calculator script: 12 + 34, one press at a time.
fn calculator_turn(agent: &mut Agent) {
    if !ensure_open(agent, "awcalc", "calculator") {
        finish(agent, "I could not open the calculator, so I could not add the numbers.");
        return;
    }

    match agent.link.view("awcalc") {
        Ok(markup) => report("view awcalc", &markup),
        Err(err) => log(&format!("could not read awcalc: {err}")),
    }

    // The turn: work out 12 + 34 on the calculator, one press at a time, the
    // way a human would. Each line is an intent and the outcome it is expected
    // to have, so a run that behaves differently is visible in the log rather
    // than needing to be reasoned about.
    let script: &[(&str, &str, &str, &str, &str)] = &[
        // Nothing has been entered, so the calculator has disabled its equals
        // button. A disabled control offers no actions and this is refused: the
        // agent can see that `=` exists and that pressing it now would mean
        // nothing.
        ("awcalc", "click", "equals", "", "disabled"),
        ("awcalc", "click", "digit-1", "", "done"),
        ("awcalc", "click", "digit-2", "", "done"),
        ("awcalc", "click", "add", "", "done"),
        ("awcalc", "click", "digit-3", "", "done"),
        ("awcalc", "click", "digit-4", "", "done"),
        // The whole calculator fits on screen, so there is nothing to move and
        // this succeeds by already being true. The point is the contract: the
        // agent names the node it wants visible and never a scroll container,
        // and the compositor works out whether anything has to happen.
        ("awcalc", "scroll-into-view", "equals", "", "done"),
        // 12 + 34. The display should read 46 in the view read back below.
        ("awcalc", "click", "equals", "", "done"),
        // Nothing in that window answers to this name.
        ("awcalc", "click", "no-such-thing", "", "no-such-node"),
        // The agentdesk's own chrome. Not missing: forbidden, and told apart
        // from missing so the agent does not go looking for it.
        ("workspace", "click", "send-message", "", "not-addressable"),
        // A button offers focus and click, and nothing else. The action list is
        // derived from the element and its state, so this cannot be talked into
        // existing.
        ("awcalc", "type-text", "digit-7", "hello", "unsupported-action"),
    ];

    agent.say(turn::KIND_THOUGHT, "working out 12 + 34 on the calculator, one press at a time");

    let mut surprises = 0;
    for &(app, action, target, value, expected) in script {
        let got = act(agent, app, action, target, value);
        if got != expected {
            surprises += 1;
            agent.say(turn::KIND_ERROR, &format!("{action} {target}: expected {expected}, got {got}"));
        }
    }

    // The last thing it does is read the screen again, because an agent that
    // acts without checking the result is the thing this whole design exists to
    // make unnecessary. The display should read 46.
    let display = match agent.link.view("awcalc") {
        Ok(markup) => {
            report("view awcalc after the turn", &markup);
            display_of(&markup)
        }
        Err(_) => None,
    };
    if let Some(shown) = &display {
        agent.say(turn::KIND_RESULT, &format!("the calculator's display reads {shown}"));
    }

    let reply = match (surprises, display) {
        (0, Some(shown)) => format!("12 + 34 = {shown}. Every step went as expected."),
        (0, None) => "Done, every step went as expected, but I could not read the result back.".to_owned(),
        (n, Some(shown)) => format!("The display reads {shown}, but {n} step(s) did not go as expected."),
        (n, None) => format!("{n} step(s) did not go as expected and I could not read the result back."),
    };
    log(if surprises == 0 {
        "turn complete, every intent resolved as expected"
    } else {
        "turn complete with surprises"
    });
    finish(agent, &reply);
}

/// The reply ends the turn. The agentdesk sees the hangup when this process
/// exits, which is what tells it the turn is over.
fn finish(agent: &mut Agent, reply: &str) {
    if let Some(desk) = &mut agent.desk
        && let Err(err) = desk.reply(reply)
    {
        log(&format!("could not deliver the reply: {err}"));
    }
}

/// The number the calculator is showing, read out of the agent's view: the
/// first text that parses as one. The pending line above it is text too, but
/// reads "12 +" rather than a number, so this finds the display.
fn display_of(view: &str) -> Option<String> {
    view.lines()
        .filter_map(|line| {
            let line = line.trim();
            let inner = line.strip_prefix("<text>")?.strip_suffix("</text>")?;
            inner.parse::<f64>().ok().map(|_| inner.to_owned())
        })
        .next()
}

fn report(what: &str, markup: &str) {
    log(&format!("--- {what} ---"));
    for line in markup.lines() {
        log(&format!("| {line}"));
    }
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
