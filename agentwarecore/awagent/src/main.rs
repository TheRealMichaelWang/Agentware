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
//! the transcript of its own streamed thoughts.
//!
//! Usage: awagent <desk-id>

use std::io::Write as _;
use std::time::Duration;

use awproto::agent::{Link, Outcome};

/// A pause between intents, standing in for the time a real agent spends
/// deciding what to do next.
///
/// Not cosmetic. An agent that fires intents as fast as the socket allows is
/// asking for actions against a screen it has not seen the result of, and it
/// makes the fake cursor a blur rather than something a human can follow.
const THINKING: Duration = Duration::from_millis(300);

fn main() {
    let mut link = match Link::inherited() {
        Ok(link) => link,
        Err(err) => {
            log(&format!("no interface connection: {err}"));
            std::process::exit(1);
        }
    };

    // What is open, and what one of them looks like. Both answers are scoped to
    // this agent's own workspace by the compositor, from what the supervisor
    // told it at handoff. Nothing here asserts which workspace that is.
    match link.apps() {
        Ok(markup) => report("apps", &markup),
        Err(err) => {
            log(&format!("could not read the workspace: {err}"));
            std::process::exit(1);
        }
    }

    match link.view("awcalc") {
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

    let mut surprises = 0;
    for &(app, action, target, value, expected) in script {
        std::thread::sleep(THINKING);

        let outcome = match link.act(app, action, target, value) {
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

        if got == expected {
            log(&format!("{action} {target} in {app} -> {got}"));
        } else {
            surprises += 1;
            log(&format!(
                "{action} {target} in {app} -> {got}, expected {expected}"
            ));
        }
    }

    // The last thing it does is read the screen again, because an agent that
    // acts without checking the result is the thing this whole design exists to
    // make unnecessary. The display should read 46.
    if let Ok(markup) = link.view("awcalc") {
        report("view awcalc after the turn", &markup);
    }

    if surprises == 0 {
        log("turn complete, every intent resolved as expected");
    } else {
        log(&format!("turn complete with {surprises} surprise(s)"));
    }
}

fn report(what: &str, markup: &str) {
    log(&format!("--- {what} ---"));
    for line in markup.lines() {
        log(&format!("| {line}"));
    }
}

/// Log to the kernel ring buffer.
///
/// An agent's real output is telemetry to its agentdesk, over the second
/// descriptor it was handed. That channel is not used here: this stand-in has no
/// conversation to stream, and the agentdesk stand-in has no pane to stream it
/// into yet.
fn log(message: &str) {
    let line = format!("<6>awagent: {message}\n");
    if let Ok(mut kmsg) = std::fs::OpenOptions::new().write(true).open("/dev/kmsg") {
        let _ = kmsg.write_all(line.as_bytes());
    } else {
        eprintln!("awagent: {message}");
    }
}
