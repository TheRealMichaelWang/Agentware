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

    match link.view("awapp") {
        Ok(markup) => report("view awapp", &markup),
        Err(err) => log(&format!("could not read awapp: {err}")),
    }

    // The turn. Each line is an intent and the outcome it is expected to have,
    // so a run that behaves differently is visible in the log rather than
    // needing to be reasoned about.
    let script: &[(&str, &str, &str, &str, &str)] = &[
        // The compose window opened behind the notes window, and this agent
        // does not know and cannot ask. Acting on an app brings it forward,
        // maximizes it, and puts the workspace's other windows away: window
        // arrangement is translation the compositor performs, never something
        // an agent reasons about. So a click into a covered window simply
        // works, and there is no covered-window rejection left to demonstrate.
        ("awapp", "click", "discard", "", "done"),
        ("awapp", "scroll-into-view", "to", "", "done"),
        ("awapp", "type-text", "to", "alice@example.com", "done"),
        ("awapp", "type-text", "body", "Sent by an agent.", "done"),
        // Already checked, so this produces no event at all. `check` is
        // unconditional on purpose: an agent that wants a box checked should not
        // depend on a state it read a moment ago.
        ("awapp", "check", "copy-self", "", "done"),
        ("awapp", "click", "send", "", "done"),
        // Sending cleared the recipient, so the application disabled the button.
        // A disabled control offers no actions and this is refused.
        ("awapp", "click", "send", "", "disabled"),
        // An archived draft, disabled by the application.
        ("awapp", "click", "draft-3", "", "disabled"),
        // Nothing in that window answers to this name.
        ("awapp", "click", "no-such-thing", "", "no-such-node"),
        // The agentdesk's own chrome. Not missing: forbidden, and told apart
        // from missing so the agent does not go looking for it.
        ("workspace", "click", "send-message", "", "not-addressable"),
        // A button offers focus and click, and nothing else. The action list is
        // derived from the element and its state, so this cannot be talked into
        // existing. Aimed at `discard` rather than `send` because `send` is
        // disabled by now, and disabled is checked first: a control that cannot
        // be used at all is a better answer than a list of what it would have
        // offered.
        ("awapp", "type-text", "discard", "hello", "unsupported-action"),
        // The far end of the draft list. Whether this needs scrolling depends on
        // the size of the display, so the outcome expected is the same either
        // way and the interesting part is that the compositor works out what to
        // move without ever being told which container to scroll.
        ("awapp", "scroll-into-view", "draft-24", "", "done"),
        ("awapp", "click", "draft-24", "", "done"),
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
    // make unnecessary.
    if let Ok(markup) = link.view("awapp") {
        report("view awapp after the turn", &markup);
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
