//! A stand-in agent: one turn, scripted, then gone.
//!
//! The per-turn worker is the shortest-lived thing in Agentware and owns nothing
//! durable. Interrupting it is a signal to a process with no state, crashing it
//! takes nothing with it, and it is never restarted, because restarting one
//! would silently re-run side effects it had already performed.
//!
//! There is no model here. The script below is fixed, and it is chosen to walk
//! every branch of the compositor's intent resolution: the ones that succeed,
//! and each way one can be refused. A rejection is an answer, and an agent that
//! cannot be told no acts blind and retries forever, so the refusals matter at
//! least as much as the successes.
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
    // it was given so the channel can be seen working end to end.
    if let Some(desk) = &mut agent.desk {
        match desk.context() {
            Ok((history, prompt)) => {
                let earlier = history.len();
                agent.say(
                    turn::KIND_THOUGHT,
                    &format!("read {earlier} earlier message(s); the prompt is {prompt:?}"),
                );
            }
            Err(err) => {
                log(&format!("could not read the context: {err}"));
                agent.desk = None;
            }
        }
    }

    // What is open. The answer is scoped to this agent's own workspace by the
    // compositor, from what the supervisor told it at handoff. Nothing here
    // asserts which workspace that is.
    let apps = match agent.link.apps() {
        Ok(markup) => markup,
        Err(err) => {
            log(&format!("could not read the workspace: {err}"));
            std::process::exit(1);
        }
    };
    report("apps", &apps);

    // The turn needs the calculator. If it is not open, ask the workspace to
    // open it, then wait until the compositor says it is there.
    if !apps.contains("awcalc") {
        agent.say(turn::KIND_ACTION, "opening the calculator");
        if let Some(desk) = &mut agent.desk
            && let Err(err) = desk.open_app("awcalc")
        {
            log(&format!("could not ask for the calculator: {err}"));
        }
        let waited = Instant::now();
        loop {
            std::thread::sleep(THINKING);
            match agent.link.apps() {
                Ok(markup) if markup.contains("awcalc") => break,
                Ok(_) if waited.elapsed() < OPENING => continue,
                Ok(_) => {
                    agent.say(turn::KIND_ERROR, "the calculator did not open");
                    finish(&mut agent, "I could not open the calculator, so I could not add the numbers.");
                    return;
                }
                Err(err) => {
                    log(&format!("could not read the workspace: {err}"));
                    std::process::exit(1);
                }
            }
        }
        agent.say(turn::KIND_RESULT, "the calculator is open");
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
        if got == expected {
            agent.say(turn::KIND_ACTION, &format!("{what}: {got}"));
        } else {
            surprises += 1;
            agent.say(turn::KIND_ERROR, &format!("{what}: {got}, expected {expected}"));
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
    finish(&mut agent, &reply);
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
