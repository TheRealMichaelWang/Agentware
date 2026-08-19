//! What an agent says to the haimanager, and what it hears back.
//!
//! An agent has exactly two channels and neither one is the supervisor. This is
//! the first: reading the workspace, and acting on it.
//!
//! ```text
//!   query  apps                              agent -> haimanager
//!   query  view <app>
//!   intent <app> <action> <target> [value]
//!
//!   apps     <awml>                          haimanager -> agent
//!   view     <app> <awml>
//!   done     <app> <target> <action>
//!   rejected <app> <target> <reason>
//!   changed  <app>                           haimanager -> agent, unsolicited
//! ```
//!
//! `changed` is the one unsolicited message: an application in the agent's
//! workspace re-rendered and its tree actually differed, so a view the agent
//! read before that moment no longer describes the screen. It carries the
//! name and nothing else, deliberately. The remedy is a fresh read, exactly
//! as a human notices movement and then looks; sending the difference itself
//! would reintroduce the failure the whole-tree protocol exists to avoid,
//! where one missed patch leaves two ends disagreeing with no resync path.
//! The [`Link`] collects these while waiting on replies, and hands them over
//! through [`Link::take_changed`] between actions.
//!
//! ## Intents, not events
//!
//! This is the distinction the whole input model rests on. An **intent** is what
//! an agent sends: "click node `send`". An **event** is what an application
//! receives: "a click occurred on node `send`". They are different schemas and an
//! agent may only produce the first.
//!
//! The compositor turns one into the other, and in between does everything that
//! makes the action legitimate: arrange the target's window to the front,
//! maximized, with its siblings put away, resolve the id to a rectangle, verify
//! the node exists and is visible and is enabled, move the fake cursor there so
//! the human sees what is about to happen, and only then synthesize exactly the
//! event a human would have produced. Arrangement is always the compositor's:
//! an agent cannot move, resize or raise a window, because no such intent
//! exists.
//!
//! If an agent could emit the event directly, every one of those steps would be
//! skippable. It could click a disabled button, or one scrolled off screen, or
//! one behind another window, and the application would receive something no
//! human could have produced.
//!
//! ## Rejections are answers
//!
//! Because those steps can fail, intents are rejectable. An agent that cannot be
//! told no acts blind and retries forever.
//!
//! ## Scoping is not a field on this wire
//!
//! Every answer is scoped to the agent's own workspace, and there is no
//! workspace id anywhere in the messages above. The compositor knows which
//! workspace a connection belongs to because the supervisor said so when it
//! handed the descriptor over. The agent is never asked and cannot lie.

use std::io::{self, Read, Write};
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::net::UnixStream;

use crate::display::MAX_TREE;
use crate::{Decoder, HAI_FD_ENV, encode};

pub const MSG_QUERY: &str = "query";
pub const MSG_INTENT: &str = "intent";
pub const MSG_APPS: &str = "apps";
pub const MSG_VIEW: &str = "view";
pub const MSG_DONE: &str = "done";
pub const MSG_REJECTED: &str = "rejected";
/// An application's tree changed since it was last read. Unsolicited, and
/// carrying only the name: the answer to it is a fresh `query view`.
pub const MSG_CHANGED: &str = "changed";

/// No application of that name is open in this workspace.
pub const REASON_NO_SUCH_APP: &str = "no-such-app";
/// The application is open, but nothing in it answers to that id.
pub const REASON_NO_SUCH_NODE: &str = "no-such-node";
pub const REASON_DISABLED: &str = "disabled";
/// Scrolled out of its container, which `scroll-into-view` remedies. Being
/// behind another window is not a reason an agent can ever receive: acting on
/// an app arranges its window to the front first.
pub const REASON_NOT_VISIBLE: &str = "not-visible";
/// Behind an open dialog. The control is fine and the application did not
/// disable it; something modal is in front, and answering that is what brings
/// it back. Told apart from `disabled` so the agent knows to look for the
/// dialog rather than wait for the application.
pub const REASON_BLOCKED: &str = "blocked";
/// The element does not offer that action in its current state. The action list
/// in the view is the authority, and it is derived rather than declared.
pub const REASON_UNSUPPORTED: &str = "unsupported-action";
/// The application marked this control as needing a human to approve it.
pub const REASON_NEEDS_APPROVAL: &str = "needs-approval";
/// Anything the agent may not touch at all: another workspace's applications,
/// and the agentdesk's own chrome. Enforced by which connection the request
/// arrived on, never by a claim the agent makes about itself.
pub const REASON_NOT_ADDRESSABLE: &str = "not-addressable";

/// What became of an intent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Done,
    Rejected(String),
}

/// An agent's end of its connection to the compositor.
pub struct Link {
    stream: UnixStream,
    decoder: Decoder,
    /// Applications the compositor said changed, in arrival order, collected
    /// while waiting on replies and drained by [`Link::take_changed`].
    changed: Vec<String>,
}

impl Link {
    /// Adopt the descriptor named by `AGENTWARE_HAI_FD`.
    pub fn inherited() -> io::Result<Link> {
        let raw: RawFd = std::env::var(HAI_FD_ENV)
            .map_err(|_| io::Error::other(format!("{HAI_FD_ENV} is not set")))?
            .parse()
            .map_err(|_| io::Error::other(format!("{HAI_FD_ENV} is not a descriptor number")))?;

        // SAFETY: the supervisor created this descriptor before forking us and
        // named it in our environment. Nothing else in this process owns it.
        let stream = unsafe { UnixStream::from_raw_fd(raw) };
        Ok(Link { stream, decoder: Decoder::with_limit(MAX_TREE), changed: Vec::new() })
    }

    /// The applications whose trees changed since the last drain, oldest
    /// first, each named once.
    ///
    /// Notices arrive whenever the compositor sends them: some while a reply
    /// was being awaited, already collected, and some sitting unread in the
    /// socket because nothing was being awaited at all. Both are gathered
    /// here, which is why this reads the socket without blocking first.
    pub fn take_changed(&mut self) -> io::Result<Vec<String>> {
        self.stream.set_nonblocking(true)?;
        let mut result = Ok(());
        loop {
            let mut buf = [0u8; 8192];
            match self.stream.read(&mut buf) {
                Ok(0) => {
                    result = Err(io::Error::other("the compositor closed the connection"));
                    break;
                }
                Ok(n) => self.decoder.feed(&buf[..n]),
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) => {
                    result = Err(err);
                    break;
                }
            }
        }
        self.stream.set_nonblocking(false)?;
        result?;

        while let Some(fields) = self
            .decoder
            .next_frame()
            .map_err(|err| io::Error::other(err.to_string()))?
        {
            self.collect(&fields);
            // Anything that is not a notice has no business arriving while no
            // request is in flight; `collect` keeps the notices and the rest
            // is dropped as the protocol violation it is.
        }

        let mut names: Vec<String> = Vec::new();
        for name in std::mem::take(&mut self.changed) {
            if !names.contains(&name) {
                names.push(name);
            }
        }
        Ok(names)
    }

    /// Keep a `changed` notice; say whether the frame was one.
    fn collect(&mut self, fields: &[String]) -> bool {
        if fields.first().map(String::as_str) != Some(MSG_CHANGED) {
            return false;
        }
        if let Some(name) = fields.get(1)
            && self.changed.last() != Some(name)
        {
            self.changed.push(name.clone());
        }
        true
    }

    /// What applications are open, as AWML.
    pub fn apps(&mut self) -> io::Result<String> {
        let reply = self.round_trip(&[MSG_QUERY, MSG_APPS])?;
        Ok(reply.get(1).cloned().unwrap_or_default())
    }

    /// The state of one application, in the reduced schema.
    pub fn view(&mut self, app: &str) -> io::Result<String> {
        let reply = self.round_trip(&[MSG_QUERY, MSG_VIEW, app])?;
        Ok(reply.get(2).cloned().unwrap_or_default())
    }

    /// Name a node and an action, and wait to be told what happened.
    ///
    /// Blocking, and deliberately one at a time. The compositor animates the
    /// cursor to the target before it synthesizes anything, so an agent firing
    /// intents faster than they can be performed would be asking for actions
    /// against a screen it has not seen the result of.
    pub fn act(&mut self, app: &str, action: &str, target: &str, value: &str) -> io::Result<Outcome> {
        let reply = self.round_trip(&[MSG_INTENT, app, action, target, value])?;
        match reply.first().map(String::as_str) {
            Some(MSG_DONE) => Ok(Outcome::Done),
            Some(MSG_REJECTED) => {
                Ok(Outcome::Rejected(reply.get(3).cloned().unwrap_or_default()))
            }
            other => Err(io::Error::other(format!("unexpected reply {other:?}"))),
        }
    }

    fn round_trip(&mut self, fields: &[&str]) -> io::Result<Vec<String>> {
        self.stream.write_all(&encode(fields))?;

        loop {
            if let Some(reply) = self
                .decoder
                .next_frame()
                .map_err(|err| io::Error::other(err.to_string()))?
            {
                // A change notice may arrive while a reply is awaited; it is
                // collected rather than mistaken for the answer, and handed
                // over by `take_changed` when the caller next asks.
                if self.collect(&reply) {
                    continue;
                }
                return Ok(reply);
            }

            let mut buf = [0u8; 8192];
            match self.stream.read(&mut buf) {
                Ok(0) => return Err(io::Error::other("the compositor closed the connection")),
                Ok(n) => self.decoder.feed(&buf[..n]),
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(err),
            }
        }
    }
}
