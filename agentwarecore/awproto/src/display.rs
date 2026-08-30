//! The display protocol: what a drawing client and the haimanager say to each
//! other over the descriptor the supervisor handed them at spawn.
//!
//! Two messages, in opposite directions, and nothing else:
//!
//! ```text
//!   render <version> <awml>                     client -> haimanager
//!   event  <version> <target> <action> [value]  haimanager -> client
//! ```
//!
//! **A client sends its whole tree, every time.** It does not send patches, does
//! not track what it sent last, and does not diff. The haimanager diffs the new
//! tree against the one it is holding. That makes applications immediate-mode
//! and trivially correct: describe the present state, done. The alternative,
//! where every app computes deltas, makes every app author reimplement diffing
//! and introduces a failure nothing can detect: one misapplied patch and the
//! app's model and the compositor's model diverge silently with no resync path.
//!
//! **The version is the client's own counter**, stamped on every tree it sends
//! and echoed back on every event generated against that tree. Without it there
//! is a race: the app sends v1, the human clicks, the app has already sent v2 in
//! which that node means something else, and the event arrives describing an
//! intention the human never had. Because the number is the app's own, checking
//! it is a comparison against a counter the app already has rather than a
//! mapping it has to maintain.
//!
//! Note what is *not* here. There is no message for focus, caret position,
//! scroll offset or selection, because none of those are the application's to
//! know. The haimanager owns them. If they were in the tree, a full-tree resend
//! would reset the human's cursor on every keystroke and every app would
//! reimplement their preservation slightly differently.

use std::io::{self, Read, Write};
use std::os::fd::{AsFd, BorrowedFd, FromRawFd, RawFd};
use std::os::unix::net::UnixStream;

use crate::{Decoder, HAI_FD_ENV, encode};

/// A client describing its interface.
pub const MSG_RENDER: &str = "render";
/// The haimanager reporting something that happened to a node.
pub const MSG_EVENT: &str = "event";

// The subset of the action vocabulary that reaches an application as an event.
// An agent's intent is turned into exactly one of these, which is the point: the
// application cannot tell the two apart, and there is no second event path for
// an agent to reach that a human could not.
//
// Some verbs in the vocabulary never appear here. `focus` does not, because
// focus is the compositor's and an application that tracked it would fight the
// compositor for it. `check` and `uncheck` do not, because they are the
// unconditional forms: the compositor compares them against the current state
// and sends a `toggle` only if the state actually has to change, which is
// exactly the event a human would have produced.
pub const ACTION_CLICK: &str = "click";
pub const ACTION_TYPE_TEXT: &str = "type-text";
pub const ACTION_SUBMIT: &str = "submit";
pub const ACTION_TOGGLE: &str = "toggle";
pub const ACTION_SELECT: &str = "select";
pub const ACTION_DESELECT: &str = "deselect";
/// A dropdown asked to show or hide its options. The application owns the
/// `open` state, as it owns every other, and answers by re-rendering.
pub const ACTION_OPEN: &str = "open";
pub const ACTION_CLOSE: &str = "close";
/// A run of cells chosen at once: the target is one corner and the value is
/// the id of the other. One event for one gesture, because a drag across a
/// grid is one thing a person did, and because an agent saying "A1 through
/// C5" in a single intent is legible in a way fifteen selects are not.
pub const ACTION_SELECT_RANGE: &str = "select-range";
/// The other mouse button, on the node under it. Carries no meaning of its
/// own: an application answers by opening one of its menus, or ignores it.
///
/// Not in the agent's vocabulary. An agent opens a menu by naming it, which
/// is the same command reached without a pointer, so nothing here needs a
/// second path for it.
pub const ACTION_CONTEXT: &str = "context";
/// A tab carried into another position: the value is the slot it should take
/// among its strip's tabs, counting from zero.
///
/// The order is the application's, unlike the navigation bar's, whose order
/// is chrome and never told to a workspace. So the compositor does not
/// rearrange anything here: it says where the hand put the tab and the
/// application answers by re-rendering, exactly as it answers a `scroll`.
/// One event each time the pointer crosses another tab's middle, on the same
/// principle as one event per keystroke.
pub const ACTION_MOVE: &str = "move";

/// A table asked to move its window: the value is the row that should now be
/// first. The application owns which rows it has sent, exactly as it owns a
/// dropdown's `open`, and answers by re-rendering with a new `first-row`.
///
/// This is the one place a scroll position crosses the protocol, and it does
/// so because it is not a scroll position: it is which slice of a thousand
/// rows the application chose to describe. Offsets inside a `scroll`
/// container remain the compositor's and are still never sent anywhere.
pub const ACTION_SCROLL: &str = "scroll";

/// Longest tree the haimanager will accept from one client.
///
/// Far larger than the supervisor's [`crate::MAX_FRAME`], because this carries a
/// whole document rather than a five-field command, and far smaller than
/// unbounded, because a client should not be able to make the compositor
/// allocate until the machine dies.
pub const MAX_TREE: usize = 1024 * 1024;

/// One thing that happened to one node, against one version of the tree.
#[derive(Clone, Debug)]
pub struct Event {
    /// The version of the tree this was generated against. An application whose
    /// current version is higher has already moved on and should discard it.
    pub version: u64,
    /// The `id` of the node it happened to.
    pub target: String,
    /// One of the action verbs above.
    pub action: String,
    /// The payload, for the actions that carry one. Empty otherwise.
    pub value: String,
}

impl Event {
    pub fn encode(&self) -> Vec<u8> {
        encode(&[
            MSG_EVENT,
            &self.version.to_string(),
            &self.target,
            &self.action,
            &self.value,
        ])
    }

    pub fn from_fields(fields: &[String]) -> Option<Event> {
        if fields.first().map(String::as_str) != Some(MSG_EVENT) {
            return None;
        }
        Some(Event {
            version: fields.get(1)?.parse().ok()?,
            target: fields.get(2)?.clone(),
            action: fields.get(3)?.clone(),
            value: fields.get(4).cloned().unwrap_or_default(),
        })
    }
}

/// A parsed `render` message: the version the client stamped, and the markup.
pub fn parse_render(fields: &[String]) -> Option<(u64, &str)> {
    if fields.first().map(String::as_str) != Some(MSG_RENDER) {
        return None;
    }
    Some((fields.get(1)?.parse().ok()?, fields.get(2)?.as_str()))
}

/// A client's end of its connection to the haimanager.
///
/// Every process that draws is forked already holding this descriptor, so there
/// is no socket to open, no path to reach, and no startup race: the connection
/// existed before the process did.
pub struct Surface {
    stream: UnixStream,
    decoder: Decoder,
    version: u64,
}

impl Surface {
    /// Adopt the descriptor named by `AGENTWARE_HAI_FD`.
    pub fn inherited() -> io::Result<Self> {
        let raw: RawFd = std::env::var(HAI_FD_ENV)
            .map_err(|_| io::Error::other(format!("{HAI_FD_ENV} is not set")))?
            .parse()
            .map_err(|_| io::Error::other(format!("{HAI_FD_ENV} is not a descriptor number")))?;

        // SAFETY: the supervisor created this descriptor before forking us and
        // named it in our environment. Nothing else in this process owns it.
        let stream = unsafe { UnixStream::from_raw_fd(raw) };
        Ok(Self { stream, decoder: Decoder::with_limit(MAX_TREE), version: 0 })
    }

    /// The version of the most recently sent tree.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// True if an event was generated against a tree this client has since
    /// replaced, and should therefore be thrown away rather than acted on.
    pub fn is_stale(&self, event: &Event) -> bool {
        event.version != self.version
    }

    /// Send the whole current interface. Returns the version it was stamped
    /// with.
    pub fn render(&mut self, awml: &str) -> io::Result<u64> {
        self.version += 1;
        let version = self.version.to_string();
        self.stream.write_all(&encode(&[MSG_RENDER, &version, awml]))?;
        Ok(self.version)
    }

    /// The descriptor, for a client that waits on more than one thing.
    ///
    /// An application blocks in [`Surface::next_event`] and needs nothing else.
    /// An agentdesk cannot: it is also listening to its agent and to a clock, so
    /// it polls this alongside the rest and pulls events with
    /// [`Surface::pump`].
    pub fn set_nonblocking(&self, on: bool) -> io::Result<()> {
        self.stream.set_nonblocking(on)
    }

    /// Read whatever the compositor has sent without waiting for more.
    ///
    /// Only meaningful on a non-blocking surface. Returns `false` once the
    /// compositor has hung up; the events already read are still available
    /// through [`Surface::take_event`].
    pub fn pump(&mut self) -> io::Result<bool> {
        loop {
            let mut buf = [0u8; 4096];
            match self.stream.read(&mut buf) {
                Ok(0) => return Ok(false),
                Ok(n) => self.decoder.feed(&buf[..n]),
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => return Ok(true),
                Err(err) => return Err(err),
            }
        }
    }

    /// The next event already read, if there is one.
    pub fn take_event(&mut self) -> io::Result<Option<Event>> {
        loop {
            let frame = self
                .decoder
                .next_frame()
                .map_err(|err| io::Error::other(err.to_string()))?;
            match frame {
                Some(fields) => match Event::from_fields(&fields) {
                    Some(event) => return Ok(Some(event)),
                    None => continue,
                },
                None => return Ok(None),
            }
        }
    }

    /// Block until the next event arrives, or `None` if the compositor hung up.
    ///
    /// A message that is not an event is skipped rather than treated as fatal.
    /// A client should not die because a future version of the compositor
    /// started saying something it does not understand yet.
    pub fn next_event(&mut self) -> io::Result<Option<Event>> {
        loop {
            let frame = self
                .decoder
                .next_frame()
                .map_err(|err| io::Error::other(err.to_string()))?;

            if let Some(fields) = frame {
                match Event::from_fields(&fields) {
                    Some(event) => return Ok(Some(event)),
                    None => continue,
                }
            }

            let mut buf = [0u8; 4096];
            match self.stream.read(&mut buf) {
                Ok(0) => return Ok(None),
                Ok(n) => self.decoder.feed(&buf[..n]),
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(err),
            }
        }
    }
}

impl AsFd for Surface {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.stream.as_fd()
    }
}

/// Escape text for an attribute value.
///
/// Clients build markup by formatting strings, and the strings include whatever
/// the human typed. Without this, typing `<` into a field makes the app emit a
/// document that no longer parses, and the interface stops updating for a reason
/// that looks nothing like its cause.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(character),
        }
    }
    out
}
