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
/// A run of cells put into one of an application's sheets.
///
/// `sheet <source> <version> <base> <at> <value>...`
///
/// The whole of the spreadsheet wire, and deliberately one shape rather than a
/// vocabulary of operations: put these values in, starting at this cell and
/// running across. A single cell is a run of one, a filled row is a run of
/// many, and emptying a cell is a run carrying an empty string, because in a
/// sheet an empty cell and a cleared one are the same cell.
///
/// `version` is what the sheet becomes; `base` is what it must already be for
/// this to mean anything. A base of zero says "forget what you have for this
/// source and start from here", which is what a snapshot is, so there is one
/// code path rather than two. The compositor never compares one sheet with
/// another to work out what moved: it is told, and it writes what it is told.
pub const MSG_SHEET: &str = "sheet";
/// `sheet-resend <source> <have>`: the compositor could not apply what it was
/// sent and needs the sheet from the beginning. `have` is the version it holds,
/// which is enough for an application to know how far behind it is.
pub const MSG_SHEET_RESEND: &str = "sheet-resend";
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
/// One number chosen, on an element that holds a range. The value travels as
/// text like every other value on this wire, and the application parses it.
///
/// A human dragging a slider's thumb produces a run of these, one per value
/// the thumb passes through, exactly as typing produces one `type-text` per
/// character: an application sees a value moving rather than one that
/// appeared, and an agent's single `set-value` is one of the same events.
pub const ACTION_SET_VALUE: &str = "set-value";
pub const ACTION_SELECT: &str = "select";
pub const ACTION_DESELECT: &str = "deselect";
/// A dropdown asked to show or hide its options. The application owns the
/// `open` state, as it owns every other, and answers by re-rendering.
pub const ACTION_OPEN: &str = "open";
/// Something asked to close: a dropdown's options, a `closable` tab, or, when
/// the **target is empty, the window itself**.
///
/// The window is the only one of those with no node to name, because its cross
/// is compositor chrome and an application never sees it. It is still the same
/// word doing the same thing: a request the application answers by going away,
/// or by rendering a question the way it answers a tab's cross. Both are a
/// human asking to close something the application knows more about than the
/// compositor does.
///
/// Nothing forces it. There is no second press that closes the window
/// regardless and no verb at PID 1 for ending an application, so an application
/// that hears this and does nothing keeps its window: that is a bug in the
/// application, and the machinery to defend against it cost more than it was
/// worth. The window goes when the process does.
///
/// It arrives as an ordinary event, and that is deliberate rather than
/// incidental: an application's loop is built around `next_event`, which blocks
/// until one arrives. A message of its own would have been read, remembered and
/// never acted on, because there is no event coming after it.
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
    /// Which cell of a `spreadsheet` it happened to, as `B7`. Empty for
    /// everything else.
    ///
    /// A field of its own rather than folded into `target` because a
    /// spreadsheet's cells are not nodes: there is one element with one id,
    /// and the cell is a coordinate inside it. Joining the two into one string
    /// would mean inventing a separator that no application's id may contain,
    /// which is a rule nobody would remember.
    pub cell: String,
}

impl Event {
    pub fn encode(&self) -> Vec<u8> {
        encode(&[
            MSG_EVENT,
            &self.version.to_string(),
            &self.target,
            &self.action,
            &self.value,
            &self.cell,
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
            cell: fields.get(5).cloned().unwrap_or_default(),
        })
    }
}

/// A parsed `sheet` message: the source, the version it makes, the version it
/// applies to, the cell the run starts at, and the values.
pub fn parse_sheet(fields: &[String]) -> Option<(&str, u64, u64, &str, &[String])> {
    if fields.first().map(String::as_str) != Some(MSG_SHEET) {
        return None;
    }
    Some((
        fields.get(1)?.as_str(),
        fields.get(2)?.parse().ok()?,
        fields.get(3)?.parse().ok()?,
        fields.get(4)?.as_str(),
        fields.get(5..).unwrap_or(&[]),
    ))
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
    /// Sheets the compositor could not follow and wants from the beginning.
    /// Drained by [`Surface::resend`].
    resend: Vec<String>,
    /// Whether the caller has asked for non-blocking reads.
    ///
    /// Tracked because writes must not inherit it. See [`Surface::send`].
    nonblocking: bool,
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
        Ok(Self {
            stream,
            decoder: Decoder::with_limit(MAX_TREE),
            version: 0,
            resend: Vec::new(),
            nonblocking: false,
        })
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

    /// Keep what the compositor said that was not an event.
    ///
    /// Anything not understood is ignored, so a client does not die because a
    /// later compositor started saying something it has never heard of. That
    /// tolerance is also why anything an application must *act* on travels as
    /// an event instead: a message noted here is only looked at when the
    /// application next comes round its loop, and it comes round its loop when
    /// an event arrives.
    fn note(&mut self, fields: &[String]) {
        if fields.first().map(String::as_str) == Some(MSG_SHEET_RESEND)
            && let Some(source) = fields.get(1)
        {
            self.resend.push(source.clone());
        }
    }

    /// Sheets the compositor has asked for from the beginning, if any.
    ///
    /// It asks when a run named a version it does not hold, which it cannot
    /// apply without guessing. An application answers by publishing the whole
    /// sheet again through [`SheetOut::restart`].
    pub fn resend(&mut self) -> Vec<String> {
        std::mem::take(&mut self.resend)
    }

    /// Publish a run of cells into one of this client's sheets.
    ///
    /// Prefer [`SheetOut`], which owns the version so it cannot be got wrong.
    pub fn sheet(
        &mut self,
        source: &str,
        version: u64,
        base: u64,
        at: &str,
        values: &[&str],
    ) -> io::Result<()> {
        let mut fields = vec![
            MSG_SHEET.to_owned(),
            source.to_owned(),
            version.to_string(),
            base.to_string(),
            at.to_owned(),
        ];
        fields.extend(values.iter().map(|value| (*value).to_owned()));
        let borrowed: Vec<&str> = fields.iter().map(String::as_str).collect();
        self.send(&encode(&borrowed))
    }

    /// Send the whole current interface. Returns the version it was stamped
    /// with.
    pub fn render(&mut self, awml: &str) -> io::Result<u64> {
        self.version += 1;
        let version = self.version.to_string();
        self.send(&encode(&[MSG_RENDER, &version, awml]))?;
        Ok(self.version)
    }

    /// The descriptor, for a client that waits on more than one thing.
    ///
    /// An application blocks in [`Surface::next_event`] and needs nothing else.
    /// An agentdesk cannot: it is also listening to its agent and to a clock, so
    /// it polls this alongside the rest and pulls events with
    /// [`Surface::pump`].
    pub fn set_nonblocking(&mut self, on: bool) -> io::Result<()> {
        self.stream.set_nonblocking(on)?;
        self.nonblocking = on;
        Ok(())
    }

    /// Write a frame whole, whatever the socket is set to for reading.
    ///
    /// A client that polls its descriptor sets it non-blocking to read, and
    /// that setting is the socket's rather than the read's, so a write large
    /// enough to fill the buffer came back `EAGAIN` and looked exactly like a
    /// dead connection. It cost an agentdesk: a conversation of fifty
    /// exchanges made a tree bigger than the socket could take in one go, the
    /// desk logged "could not send a tree" and exited, and PID 1 tore the
    /// whole workspace down around it, agent and applications included. The
    /// bug needed a long session to appear, so nothing shorter had found it.
    ///
    /// Blocking for the write is the same trade `Turn::send_context` already
    /// makes. The compositor reads continuously, so this waits for a buffer
    /// to drain rather than for anybody to decide anything.
    fn send(&mut self, bytes: &[u8]) -> io::Result<()> {
        if !self.nonblocking {
            return self.stream.write_all(bytes);
        }
        self.stream.set_nonblocking(false)?;
        let result = self.stream.write_all(bytes);
        let restored = self.stream.set_nonblocking(true);
        result.and(restored)
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
                    None => {
                        self.note(&fields);
                        continue;
                    }
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
                    None => {
                        self.note(&fields);
                        continue;
                    }
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

/// One sheet an application publishes, and the version it is up to.
///
/// The version is here rather than in the application because getting it
/// wrong is the one way this protocol can go wrong: a run that names a
/// version the compositor does not hold is refused, and a run that names the
/// wrong one silently describes a sheet nobody has. Owning the counter means
/// an application cannot make either mistake.
pub struct SheetOut {
    source: String,
    version: u64,
    /// Whether the next run starts the sheet over. True to begin with,
    /// because the compositor has never heard of this sheet, and again after
    /// a `sheet-resend`.
    restart: bool,
}

impl SheetOut {
    pub fn new(source: &str) -> SheetOut {
        SheetOut { source: source.to_owned(), version: 0, restart: true }
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    /// The version the tree should claim. Put this on the element, and send
    /// the cells before the tree that claims them: one connection, so the
    /// order is the order they arrive in.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Publish a run of values, starting at `at` and running across.
    pub fn put(&mut self, surface: &mut Surface, at: &str, values: &[&str]) -> io::Result<()> {
        let base = if self.restart { 0 } else { self.version };
        self.restart = false;
        self.version += 1;
        surface.sheet(&self.source, self.version, base, at, values)
    }

    /// Start the sheet over: forget everything published so far, and leave it
    /// empty until something is put in it. What a `sheet-resend` asks for, and
    /// what switching a sheet's contents wholesale means.
    ///
    /// This sends a run of no values against `base = 0`, which is exactly
    /// "the sheet is now this, and this is nothing". It has to be sent rather
    /// than remembered, and that is the whole reason this takes a surface: an
    /// earlier version armed a flag that the *next* run carried, so an
    /// application that started a sheet over and then had nothing to publish
    /// said nothing at all, and the compositor went on showing the sheet it
    /// already had. An empty sheet is a thing an application must be able to
    /// say, and a restart with nothing following it is the most likely way of
    /// saying it.
    pub fn restart(&mut self, surface: &mut Surface) -> io::Result<()> {
        self.restart = false;
        self.version += 1;
        surface.sheet(&self.source, self.version, 0, "A1", &[])
    }
}
