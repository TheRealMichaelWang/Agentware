//! What an agent says to the haimanager, and what it hears back.
//!
//! An agent has exactly two channels and neither one is the supervisor. This is
//! the first: reading the workspace, and acting on it.
//!
//! ```text
//!   query  apps                              agent -> haimanager
//!   query  view <app>
//!   query  cells <app> <sheet> <range>
//!   query  clipboard
//!   intent <app> <action> <target> [value]
//!
//!   apps      <awml>                         haimanager -> agent
//!   view      <app> <awml>
//!   cells     <app> <sheet> <rows>
//!   clipboard <kind> <content>
//!   done      <app> <target> <action>
//!   rejected  <app> <target> <reason>
//!   changed      <app>                        haimanager -> agent, unsolicited
//!   data-changed <app> <source> <element> <range>
//! ```
//!
//! The last two are the unsolicited messages, and they are two because an
//! application changes in two ways that cost an agent very different things
//! to look at.
//!
//! `changed` says the application's **interface** moved: its tree re-rendered
//! and what an agent would see of it actually differs, so a view read before
//! that moment no longer describes the screen. It carries the name and
//! nothing else, deliberately. The remedy is a fresh read, exactly as a human
//! notices movement and then looks; sending the difference itself would
//! reintroduce the failure the whole-tree protocol exists to avoid, where one
//! missed patch leaves two ends disagreeing with no resync path. The judgement
//! is made on the agent's view rather than on the pixels, so a re-render that
//! changed nothing an agent can see, a colour or a spreadsheet's version
//! number, says nothing.
//!
//! `data-changed` says a **sheet** the application publishes moved: cells
//! arrived on their own frames, and the rectangle they landed in is named so
//! an agent can read exactly that much rather than the sheet. The element
//! showing the source is named too, when one does, because `query cells` reads
//! through an element; an empty range means the sheet was replaced whole.
//! A sheet is the one thing in the system too large to re-read on every
//! change, which is why its changes are a different word.
//!
//! The [`Link`] collects both while waiting on replies, can wait for them
//! ([`Link::wait_notice`]), and hands them over through [`Link::take_notices`].
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
//! ## Reading a collection too large to send
//!
//! A spreadsheet's cells are not in its view: a screenful of a grid is four
//! hundred of them, and four hundred elements carrying a description and an
//! action list each is forty kilobytes to learn twenty numbers. `query cells`
//! is how an agent reads the part it wants, by naming a rectangle.
//!
//! It is a query rather than an action because it is a read, and it asks the
//! application nothing at all: the compositor holds the sheet, published to
//! it on its own frames, so the answer is a lookup rather than a round trip
//! and a wait. The element in the view says how far the sheet runs and where
//! anything has been put in it, which is what tells an agent what to ask for.
//!
//! ## Scrolling is not an action
//!
//! There is no verb for it. Acting on anything scrolls whatever has to move
//! first, exactly as acting on an application arranges its window, so an
//! agent never expresses a scroll and never hears that something was out of
//! view. Reachability is something the compositor makes true rather than a
//! question it answers.
//!
//! ## The clipboard is read, never written
//!
//! Cut, copy and paste are the human's, done with the keyboard against the
//! compositor's own copy of a text control, and they never appear in the
//! display protocol at all: a paste is an ordinary `type-text` event, which
//! is exactly what it would be if the human had typed the words. An agent
//! reads the clipboard because what the human copied is context it may need;
//! it has no way to write one, because it has no need of a place to put text
//! it already holds. `type-text` says what it wants said.
//!
//! ## Scoping is not a field on this wire
//!
//! Every answer is scoped to the agent's own workspace, and there is no
//! workspace id anywhere in the messages above. The compositor knows which
//! workspace a connection belongs to because the supervisor said so when it
//! handed the descriptor over. The agent is never asked and cannot lie.

use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use crate::display::MAX_TREE;
use crate::{Decoder, HAI_FD_ENV, encode};

pub const MSG_QUERY: &str = "query";
pub const MSG_INTENT: &str = "intent";
pub const MSG_APPS: &str = "apps";
pub const MSG_VIEW: &str = "view";
pub const MSG_CLIPBOARD: &str = "clipboard";
/// Read a rectangle of a spreadsheet. Answered with a `cells` block.
///
/// A sheet is the one thing too big to put in a view: a screenful is four
/// hundred cells, and four hundred elements with a description and an action
/// list each is forty kilobytes to learn twenty numbers. The element in the
/// view says how far the sheet runs and where it is used; this is how the
/// part that matters is read.
pub const MSG_CELLS: &str = "cells";
pub const MSG_DONE: &str = "done";
pub const MSG_REJECTED: &str = "rejected";
/// An application's interface changed since it was last read. Unsolicited,
/// and carrying only the name: the answer to it is a fresh `query view`.
pub const MSG_CHANGED: &str = "changed";
/// A sheet an application publishes changed. Unsolicited, carrying the
/// application, the sheet's source, the element showing it (or nothing) and
/// the rectangle written (or nothing, for a sheet replaced whole): the answer
/// to it is a `query cells` for that much.
pub const MSG_DATA_CHANGED: &str = "data-changed";

/// No application of that name is open in this workspace.
pub const REASON_NO_SUCH_APP: &str = "no-such-app";
/// The application is open, but nothing in it answers to that id.
pub const REASON_NO_SUCH_NODE: &str = "no-such-node";
pub const REASON_DISABLED: &str = "disabled";
/// The compositor could not make the node reachable.
///
/// This is a fault, not an instruction. Acting on a node scrolls whatever has
/// to move and arranges whatever has to be arranged, so every ordinary reason
/// a node might be off screen is dealt with before an agent hears anything.
/// What is left is a node the compositor tried to reveal and could not, which
/// means a bug here rather than a step the agent missed: there is nothing it
/// could usefully do differently, and the compositor says so in the log.
pub const REASON_UNREACHABLE: &str = "unreachable";
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

/// Nothing has been copied yet.
pub const CLIPBOARD_NONE: &str = "none";
/// Words. The content is the text itself.
pub const CLIPBOARD_TEXT: &str = "text";
/// A picture, as base64. Nothing produces one yet; the kind exists so that
/// the day something does, an agent that only understands text says so
/// instead of reading the bytes as words.
pub const CLIPBOARD_IMAGE: &str = "image";

/// What became of an intent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Done,
    Rejected(String),
}

/// One unsolicited word from the haimanager: something in the agent's
/// workspace is no longer as the agent last saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// The application's interface changed. Read its view again.
    View { app: String },
    /// A sheet the application publishes changed. Read that much of it.
    Data {
        app: String,
        /// The stream the cells arrived on, as the element's `source` names it.
        source: String,
        /// The `spreadsheet` element showing that source, which is what
        /// `query cells` reads through. `None` when no element does, which
        /// is a sheet on a tab that is not the one in front.
        element: Option<String>,
        /// The rectangle written, as `A1:C5`. `None` when the sheet was
        /// replaced whole and everything in it may differ.
        range: Option<String>,
    },
}

impl Notice {
    /// The application the notice is about.
    pub fn app(&self) -> &str {
        match self {
            Notice::View { app } | Notice::Data { app, .. } => app,
        }
    }
}

/// An agent's end of its connection to the haimanager.
pub struct Link {
    stream: UnixStream,
    decoder: Decoder,
    /// What the haimanager said changed, in arrival order, collected while
    /// waiting on replies and drained by [`Link::take_notices`].
    notices: Vec<Notice>,
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
        Ok(Link { stream, decoder: Decoder::with_limit(MAX_TREE), notices: Vec::new() })
    }

    /// The descriptor under the link, for a caller that wants to be woken
    /// when the haimanager says something while it is busy elsewhere.
    ///
    /// Nothing but a notice can arrive while no request is in flight, so
    /// readability here means exactly "a notice is waiting".
    pub fn fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }

    /// Everything the haimanager said changed since the last drain, oldest
    /// first, each said once.
    ///
    /// Notices arrive whenever the haimanager sends them: some while a reply
    /// was being awaited, already collected, and some sitting unread in the
    /// socket because nothing was being awaited at all. Both are gathered
    /// here, which is why this reads the socket without blocking first.
    pub fn take_notices(&mut self) -> io::Result<Vec<Notice>> {
        self.stream.set_nonblocking(true)?;
        let result = self.pull();
        self.stream.set_nonblocking(false)?;
        // Drained before the error is raised, so a hangup still hands over
        // whatever arrived ahead of it.
        let taken = self.drain();
        result?;
        Ok(taken)
    }

    /// The notices collected so far, without draining them.
    pub fn pending(&self) -> &[Notice] {
        &self.notices
    }

    /// Wait up to `timeout` for the haimanager to say anything at all, and
    /// say whether it did.
    ///
    /// "Anything" is bytes, not whole frames: a read that came back with the
    /// first half of a notice is the haimanager talking, and a caller
    /// measuring silence must not count it as quiet. What arrives is
    /// collected, not returned: [`Link::pending`] shows it and
    /// [`Link::take_notices`] hands it over. Waiting is separate from taking
    /// because a caller deciding how long to wait wants to look at what has
    /// come so far and keep waiting, and a drain in the middle of that would
    /// lose the order.
    pub fn wait_notice(&mut self, timeout: Duration) -> io::Result<bool> {
        // A zero timeout would mean "forever" to the socket, and this caller
        // means "not at all".
        if timeout.is_zero() {
            return Ok(false);
        }
        self.stream.set_read_timeout(Some(timeout))?;
        let mut buf = [0u8; 8192];
        let result = match self.stream.read(&mut buf) {
            Ok(0) => Err(io::Error::other("the haimanager closed the connection")),
            Ok(n) => {
                self.decoder.feed(&buf[..n]);
                Ok(true)
            }
            Err(err)
                if matches!(err.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) =>
            {
                Ok(false)
            }
            Err(err) => Err(err),
        };
        self.stream.set_read_timeout(None)?;
        let arrived = result?;
        self.collect_frames()?;
        Ok(arrived)
    }

    /// Read whatever is in the socket now, on a socket already set not to
    /// block, and collect the notices in it.
    fn pull(&mut self) -> io::Result<()> {
        loop {
            let mut buf = [0u8; 8192];
            match self.stream.read(&mut buf) {
                Ok(0) => return Err(io::Error::other("the haimanager closed the connection")),
                Ok(n) => self.decoder.feed(&buf[..n]),
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) => return Err(err),
            }
        }
        self.collect_frames()
    }

    /// Every complete frame the decoder holds, kept if it is a notice.
    ///
    /// Anything that is not a notice has no business arriving while no
    /// request is in flight; `collect` keeps the notices and the rest is
    /// dropped as the protocol violation it is.
    fn collect_frames(&mut self) -> io::Result<()> {
        while let Some(fields) = self
            .decoder
            .next_frame()
            .map_err(|err| io::Error::other(err.to_string()))?
        {
            self.collect(&fields);
        }
        Ok(())
    }

    /// Hand over what has been collected, each notice once.
    ///
    /// A view notice repeated says nothing new: the remedy is one fresh read
    /// whether the application re-rendered once or seventeen times. A data
    /// notice repeated with the same rectangle is the same; two rectangles
    /// are two, and adding them up is the reader's business, since the
    /// arithmetic for it is not this file's.
    fn drain(&mut self) -> Vec<Notice> {
        let mut taken: Vec<Notice> = Vec::new();
        for notice in std::mem::take(&mut self.notices) {
            if !taken.contains(&notice) {
                taken.push(notice);
            }
        }
        taken
    }

    /// Keep a notice; say whether the frame was one.
    fn collect(&mut self, fields: &[String]) -> bool {
        let notice = match fields.first().map(String::as_str) {
            Some(MSG_CHANGED) => match fields.get(1) {
                Some(app) => Notice::View { app: app.clone() },
                None => return true,
            },
            Some(MSG_DATA_CHANGED) => match (fields.get(1), fields.get(2)) {
                (Some(app), Some(source)) => Notice::Data {
                    app: app.clone(),
                    source: source.clone(),
                    element: fields.get(3).filter(|name| !name.is_empty()).cloned(),
                    range: fields.get(4).filter(|range| !range.is_empty()).cloned(),
                },
                _ => return true,
            },
            _ => return false,
        };
        // Back to back repeats are folded here, so a burst of seventeen
        // re-renders is one entry rather than seventeen; everything else is
        // folded at the drain.
        if self.notices.last() != Some(&notice) {
            self.notices.push(notice);
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

    /// Read a rectangle of a spreadsheet, as `A1:D20` or a single `B7`.
    ///
    /// Answered with one line per row, values separated by tabs, so twenty
    /// numbers read as twenty numbers.
    pub fn cells(&mut self, app: &str, id: &str, range: &str) -> io::Result<String> {
        let reply = self.round_trip(&[MSG_QUERY, MSG_CELLS, app, id, range])?;
        Ok(reply.get(4).cloned().unwrap_or_default())
    }

    /// What the human last copied, as a kind and its content.
    ///
    /// The kind is `none` when nothing has been copied, `text` for words. It
    /// exists so that a kind this agent does not understand is something it
    /// can say it cannot read, rather than something it misreads as text.
    pub fn clipboard(&mut self) -> io::Result<(String, String)> {
        let reply = self.round_trip(&[MSG_QUERY, MSG_CLIPBOARD])?;
        Ok((
            reply.get(1).cloned().unwrap_or_else(|| CLIPBOARD_NONE.to_owned()),
            reply.get(2).cloned().unwrap_or_default(),
        ))
    }

    /// Name a node and an action, and wait to be told what happened.
    ///
    /// Blocking, and deliberately one at a time. The compositor animates the
    /// cursor to the target before it synthesizes anything, so an agent firing
    /// intents faster than they can be performed would be asking for actions
    /// against a screen it has not seen the result of.
    pub fn act(&mut self, app: &str, action: &str, target: &str, value: &str) -> io::Result<Outcome> {
        self.send_intent(app, action, target, value)?;
        self.next_outcome()
    }

    /// Send an intent without waiting for its outcome.
    ///
    /// The two halves of [`Link::act`] exist apart so a run of actions can be
    /// **pipelined**: sent one after another, and their outcomes collected
    /// afterwards in the same order. This is not fire and forget and nothing
    /// about the answers changes. Each intent is still validated when it
    /// reaches the front of the compositor's queue, after everything sent
    /// before it has been performed, and still answers `done` or `rejected`
    /// truthfully. What goes away is the agent sitting idle between them.
    ///
    /// That idling is what made the queue always empty, which is what stopped
    /// the compositor from ever knowing that more was coming. A compositor
    /// that can see six actions waiting can perform them at the pace of a
    /// sequence rather than at the pace of six separate gestures, which is
    /// the whole point of sending them this way.
    ///
    /// The caller must take exactly one outcome per intent sent, in order.
    pub fn send_intent(
        &mut self,
        app: &str,
        action: &str,
        target: &str,
        value: &str,
    ) -> io::Result<()> {
        self.stream.write_all(&encode(&[MSG_INTENT, app, action, target, value]))
    }

    /// The next outcome, for the oldest intent that has not been answered.
    pub fn next_outcome(&mut self) -> io::Result<Outcome> {
        let reply = self.await_reply()?;
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
        self.await_reply()
    }

    /// A link over one end of a socketpair, for tests.
    #[cfg(test)]
    fn over(stream: UnixStream) -> Link {
        Link { stream, decoder: Decoder::with_limit(MAX_TREE), notices: Vec::new() }
    }

    /// Block until something that is not a change notice arrives.
    fn await_reply(&mut self) -> io::Result<Vec<String>> {
        loop {
            if let Some(reply) = self
                .decoder
                .next_frame()
                .map_err(|err| io::Error::other(err.to_string()))?
            {
                // A notice may arrive while a reply is awaited; it is
                // collected rather than mistaken for the answer, and handed
                // over by `take_notices` when the caller next asks.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn data(app: &str, source: &str, element: Option<&str>, range: Option<&str>) -> Notice {
        Notice::Data {
            app: app.into(),
            source: source.into(),
            element: element.map(str::to_owned),
            range: range.map(str::to_owned),
        }
    }

    /// Both words arrive on the wire and come out as what they are, with the
    /// empty fields the haimanager sends for "no element" and "the whole
    /// sheet" read back as absent rather than as empty names.
    #[test]
    fn both_notices_are_collected_and_read_back() {
        let (ours, mut haimanager) = UnixStream::pair().unwrap();
        let mut link = Link::over(ours);
        haimanager.write_all(&encode(&[MSG_CHANGED, "awsheet"])).unwrap();
        haimanager
            .write_all(&encode(&[MSG_DATA_CHANGED, "awsheet", "book/1", "sheet", "B7"]))
            .unwrap();
        haimanager.write_all(&encode(&[MSG_DATA_CHANGED, "awsheet", "book/2", "", ""])).unwrap();

        let notices = link.take_notices().unwrap();
        assert_eq!(
            notices,
            vec![
                Notice::View { app: "awsheet".into() },
                data("awsheet", "book/1", Some("sheet"), Some("B7")),
                data("awsheet", "book/2", None, None),
            ]
        );
        // Taken means taken.
        assert!(link.take_notices().unwrap().is_empty());
    }

    /// A burst of re-renders is one notice, and a reply in the middle of
    /// them is still the reply.
    #[test]
    fn repeats_fold_and_a_reply_is_not_a_notice() {
        let (ours, mut haimanager) = UnixStream::pair().unwrap();
        let mut link = Link::over(ours);
        for _ in 0..17 {
            haimanager.write_all(&encode(&[MSG_CHANGED, "awtext"])).unwrap();
        }
        haimanager.write_all(&encode(&[MSG_DONE, "awtext", "body", "type-text"])).unwrap();
        haimanager.write_all(&encode(&[MSG_CHANGED, "awtext"])).unwrap();

        assert_eq!(link.next_outcome().unwrap(), Outcome::Done);
        assert_eq!(link.pending(), &[Notice::View { app: "awtext".into() }]);
        assert_eq!(link.take_notices().unwrap().len(), 1);
    }

    /// Waiting is bounded, and what arrives while waiting is collected
    /// rather than returned, so a caller can look and keep waiting.
    #[test]
    fn waiting_collects_and_a_quiet_socket_times_out() {
        let (ours, mut haimanager) = UnixStream::pair().unwrap();
        let mut link = Link::over(ours);

        let started = std::time::Instant::now();
        assert!(!link.wait_notice(Duration::from_millis(30)).unwrap());
        assert!(started.elapsed() >= Duration::from_millis(30));
        assert!(link.pending().is_empty());

        haimanager.write_all(&encode(&[MSG_CHANGED, "awcalc"])).unwrap();
        assert!(link.wait_notice(Duration::from_secs(5)).unwrap());
        assert_eq!(link.pending().len(), 1);

        // Half a frame is the haimanager talking, not silence.
        let frame = encode(&[MSG_CHANGED, "awsheet"]);
        haimanager.write_all(&frame[..5]).unwrap();
        assert!(link.wait_notice(Duration::from_secs(5)).unwrap());
        assert_eq!(link.pending().len(), 1, "half a frame is not a notice yet");
        haimanager.write_all(&frame[5..]).unwrap();
        assert!(link.wait_notice(Duration::from_secs(5)).unwrap());
        assert_eq!(link.pending().len(), 2);
        // And the socket is blocking again afterwards, which the next reply
        // depends on.
        haimanager.write_all(&encode(&[MSG_APPS, "<apps/>"])).unwrap();
        assert_eq!(link.await_reply().unwrap()[0], MSG_APPS);
    }
}
