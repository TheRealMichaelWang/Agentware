//! A connected client, and everything the compositor knows about it that the
//! client itself does not.
//!
//! Every process that draws is forked already holding one end of a socketpair to
//! the haimanager. The other end arrives here, pushed by the supervisor over the
//! control socket with a frame saying what it is. That tag is the whole of
//! identity in Agentware: the compositor knows which workspace a connection
//! belongs to because PID 1 said so at handoff. The client is never asked and
//! cannot lie.
//!
//! ## What the client sends, and what it does not
//!
//! It sends its whole tree, every time. It never sends focus, caret position,
//! scroll offset or selection, because those are not its business. They live
//! here, in [`Client`], keyed by node identity, and are carried across every
//! re-render.
//!
//! That split is the reason the model works. If ephemeral state were in the
//! tree, a full-tree resend would reset the human's text cursor on every
//! keystroke, and every application author would reimplement its preservation
//! slightly differently and slightly wrong.
//!
//! ## Typing, and who owns a value
//!
//! A field's `value` is the application's; the caret inside it is the
//! compositor's. So a keystroke cannot simply be applied locally and forgotten,
//! nor sent off and waited on. The compositor does both: it applies the edit to
//! its own copy, paints that immediately, and tells the application what the
//! value now is. Typing therefore never waits for a round trip.
//!
//! The reconciliation is the interesting part. Every value sent is remembered
//! until a tree comes back carrying it. While anything is outstanding, an
//! incoming value that matches one of them is the application agreeing with us
//! and the local copy stands. An incoming value that matches none of them is the
//! application having changed the value itself, which it is entitled to do, and
//! the application wins. Without the queue, a second keystroke arriving before
//! the first round trip completes would see the echo of the first, conclude the
//! application disagreed, and throw the second keystroke away.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::time::{Duration, Instant};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, RawFd};
use std::os::unix::net::UnixStream;

use awproto::Decoder;
use awproto::agent;
use awproto::display::{self, MAX_TREE};

use crate::awml::{self, Tag};
use crate::clipboard::Clipboard;
use crate::document::Document;
use crate::editmenu;
use crate::sheet::{self, Sheets};
use crate::text::{self, Edit, Editing, MultiPress};
use crate::images::Images;
use crate::input::{Button, Event, Key};
use crate::paint::font::Fonts;
use crate::paint::{Canvas, Rect};
use crate::ui::{self, Focus, Frame, Grid, Layout};

/// How much unsent event traffic a client may accumulate before it is treated as
/// gone.
///
/// The compositor must never block on a write, so events queue when a client is
/// slow. A client that has stopped reading entirely is not slow, it is broken,
/// and holding its backlog forever would let one wedged application consume
/// memory in the one process that owns the screen.
const MAX_BACKLOG: usize = 256 * 1024;

/// How many rows one notch of the wheel moves a grid.
const WHEEL_ROWS: i32 = 3;

/// How long a scrollbar stays on screen after its content last moved.
const SCROLLBAR_LINGER: Duration = Duration::from_millis(900);

/// How long a control stays visibly pressed.
///
/// Long enough to be seen in a screenshot and by a human watching an agent
/// work, short enough not to feel like lag when the human is the one clicking.
const PRESS: Duration = Duration::from_millis(240);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The workspace itself. Chrome, and invisible to agents by construction.
    Desk,
    /// An application window inside a workspace.
    App,
    /// A per-turn worker. It reads and acts; it does not draw.
    Agent,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Desk => "desk",
            Kind::App => "app",
            Kind::Agent => "agent",
        }
    }
}

/// The compositor's copy of a text control being edited.
pub struct Client {
    pub kind: Kind,
    /// The workspace this connection belongs to, as the supervisor stated it.
    pub desk: u32,
    pub name: String,
    /// The process behind this connection, as the supervisor stated it.
    ///
    /// Needed to close one window without closing the workspace: lifetime
    /// belongs to PID 1, so the compositor names the process and asks.
    pub pid: i32,
    /// The last thing that happened, for the status strip.
    pub note: String,

    stream: UnixStream,
    decoder: Decoder,
    /// Events written but not yet accepted by the kernel.
    pending: Vec<u8>,
    /// Set when this connection can no longer be written to, which is a reason
    /// to drop it even though it has not hung up.
    broken: bool,

    doc: Option<Document>,
    layout: Layout,
    /// Where this document goes on screen. Decided by the compositor and
    /// changed under the client without telling it: an application is not
    /// informed of its own window, because there is nothing it could correctly
    /// do with the knowledge.
    frame: Frame,

    // Ephemeral state, keyed by node identity rather than by index, because
    // indices do not survive a re-render and identities are meant to.
    focus: Option<String>,
    editing: HashMap<String, Editing>,
    scroll: HashMap<String, i32>,
    /// Offsets across, which only a table has. Kept apart from the offsets
    /// down rather than paired with them, because every scroll container has
    /// one of those and almost nothing has one of these.
    scroll_x: HashMap<String, i32>,
    /// Column widths the human dragged, in pixels, keyed by the column's
    /// identity. Ephemeral like the rest of this: an application says what a
    /// column should start at and is never told it moved, for the same reason
    /// it is never told where its window is.
    columns: HashMap<String, i32>,
    /// A text control whose selection is being dragged out by the pointer.
    selecting: Option<String>,
    /// Presses counted for the double and triple click, which are how a word
    /// and a line are selected everywhere else and now here.
    presses: MultiPress,
    /// A run of static text the human has dragged out: the node's key, where
    /// the drag anchored and where it has reached, in characters.
    ///
    /// Apart from `editing`, which holds text controls, because static text
    /// is not one: it has no value the application tracks, no caret, and
    /// nothing that can be typed into it. The compositor keeps a run over it
    /// for one reason, which is that an agent's answer is worth copying.
    text_run: Option<(String, usize, usize)>,
    /// Whether that run is being dragged out right now.
    running: bool,
    /// A column edge being dragged: which spreadsheet and column, where the
    /// pointer took hold, and how wide the column was when it did.
    column_drag: Option<(String, u32, i32, i32)>,
    /// The sheets behind this client's spreadsheets, by the name each element
    /// points at. Not in any tree: they arrive as their own frames and the
    /// compositor paints from them. See [`crate::sheet`].
    sheets: Sheets,
    /// A run of cells being dragged out: which spreadsheet, the corner it
    /// started from, and the cell the run currently reaches.
    cell_drag: Option<(String, sheet::Ref)>,
    cell_extent: Option<sheet::Ref>,
    /// A tab being carried to another position: its key, and the slot it was
    /// last told to take. The slot is remembered so that crossing the same
    /// midpoint twice in one burst does not tell the application twice.
    tab_drag: Option<(String, usize)>,
    /// Where the last right-press landed, while it still stands. An open menu
    /// hangs from here, which is what makes a context menu appear under the
    /// hand rather than wherever the application happened to put the menu.
    context_at: Option<(i32, i32)>,
    /// Whether this client's caret is drawn lit right now. Written by the
    /// compositor before painting: only the client keystrokes actually go to
    /// gets a caret at all, and its phase comes from the screen's blink clock.
    pub caret_on: bool,
    /// The scroll container whose bar is showing: its key, and when its content
    /// last moved. The bar hides itself once this goes stale.
    scroll_shown: Option<(String, Instant)>,
    /// A scrollbar thumb being dragged: the container's key, and where inside
    /// the thumb it was grabbed, so the thumb tracks the hand rather than
    /// jumping to centre itself under it.
    scroll_drag: Option<(String, i32)>,
    /// Whether the bar being dragged is the one across. A table has two, and
    /// they share a node, so the axis is what tells them apart.
    scroll_across: bool,
    /// The control currently showing a press, and when it started.
    ///
    /// Ephemeral in the strictest sense: it lasts a sixth of a second and never
    /// appears in any tree. It exists because an action with no visible moment
    /// is indistinguishable from one that never happened, which matters most
    /// when the thing acting is not the human.
    press: Option<(String, Instant)>,
}

/// What happened when a client was read from.
pub struct Progress {
    pub gone: bool,
    pub dirty: bool,
    /// A tree was installed and actually differed from the held one. What the
    /// workspace's agent is told about, as distinct from `dirty`, which a
    /// rejected document also sets for the sake of the status strip.
    pub updated: bool,
    /// A first tree was installed where there was none. The moment a window can
    /// be sized to its content, since before this there was nothing to measure.
    pub first: bool,
    /// Lines worth putting in the kernel log.
    pub log: Vec<String>,
    /// Queries and intents from an agent connection, for the screen to answer.
    /// An agent does not render, so nothing it sends is understood here.
    pub requests: Vec<Vec<String>>,
}

impl Client {
    pub fn adopt(
        kind: Kind,
        desk: u32,
        name: String,
        pid: i32,
        stream: UnixStream,
    ) -> io::Result<Self> {
        stream.set_nonblocking(true)?;
        Ok(Client {
            kind,
            desk,
            name,
            pid,
            note: "connected, nothing rendered yet".into(),
            stream,
            decoder: Decoder::with_limit(MAX_TREE),
            pending: Vec::new(),
            broken: false,
            doc: None,
            layout: Layout::empty(),
            frame: Frame::Whole(Rect::new(0, 0, 0, 0)),
            focus: None,
            editing: HashMap::new(),
            scroll: HashMap::new(),
            scroll_x: HashMap::new(),
            columns: HashMap::new(),
            selecting: None,
            presses: MultiPress::default(),
            text_run: None,
            running: false,
            column_drag: None,
            sheets: Sheets::default(),
            cell_drag: None,
            cell_extent: None,
            tab_drag: None,
            context_at: None,
            caret_on: false,
            scroll_shown: None,
            scroll_drag: None,
            scroll_across: false,
            press: None,
        })
    }

    pub fn fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }

    pub fn borrow(&self) -> BorrowedFd<'_> {
        self.stream.as_fd()
    }

    pub fn label(&self) -> String {
        format!("{}:{} desk {}", self.kind.name(), self.name, self.desk)
    }

    pub fn version(&self) -> u64 {
        self.doc.as_ref().map_or(0, |doc| doc.version)
    }

    /// True once this client can no longer be sent events.
    pub fn is_broken(&self) -> bool {
        self.broken
    }

    /// The reduced view an agent connected to this workspace would be given.
    pub fn agent_view(&self) -> Option<String> {
        self.doc
            .as_ref()
            .map(|doc| awml::agent_view(&doc.tree, &self.sheets, &self.name, self.desk))
    }

    /// Drain whatever arrived and apply it.
    pub fn readable(&mut self, fonts: &Fonts) -> Progress {
        let mut progress = Progress {
            gone: false,
            dirty: false,
            updated: false,
            first: false,
            log: Vec::new(),
            requests: Vec::new(),
        };
        let mut buf = [0u8; 8192];

        loop {
            match self.stream.read(&mut buf) {
                Ok(0) => {
                    progress.gone = true;
                    break;
                }
                Ok(n) => self.decoder.feed(&buf[..n]),
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) => {
                    progress.log.push(format!("{}: read failed: {err}", self.label()));
                    progress.gone = true;
                    break;
                }
            }
        }

        loop {
            match self.decoder.next_frame() {
                Ok(Some(fields)) => {
                    if let Some((version, source)) = display::parse_render(&fields) {
                        // The borrow of `fields` has to end before the tree is
                        // installed, so the markup is copied out first.
                        let source = source.to_owned();
                        let had_doc = self.doc.is_some();
                        let was_version = self.version();
                        let (dirty, line) = self.apply(fonts, &source, version);
                        progress.dirty |= dirty;
                        // A tree counts as updated when it was installed and
                        // repainted: the version moved and the screen did too.
                        // An identical resend moves the version silently, and
                        // a rejected document moves neither.
                        progress.updated |= dirty && self.version() != was_version;
                        // Checked against the document rather than taken from
                        // `apply`, so a first tree that failed to parse does not
                        // count as having arrived.
                        progress.first |= !had_doc && self.doc.is_some();
                        progress.log.push(line);
                    } else if let Some((source, version, base, at, values)) =
                        display::parse_sheet(&fields)
                    {
                        // Cells, on their own frames. Nothing is compared with
                        // anything: the application says what to put where and
                        // the compositor writes it. A run that names a version
                        // nobody holds is refused rather than half-applied, and
                        // the answer is to ask for the sheet from the top.
                        let Some(at) = sheet::parse(at) else {
                            progress.log.push(format!(
                                "{}: sheet {source}: {at:?} is not a cell",
                                self.label()
                            ));
                            continue;
                        };
                        let source = source.to_owned();
                        if self.sheets.apply(&source, base, version, at, values) {
                            progress.dirty = true;
                        } else {
                            let have = self.sheets.version(&source).to_string();
                            progress.log.push(format!(
                                "{}: sheet {source}: v{base} does not follow v{have}, asking again",
                                self.label()
                            ));
                            self.send(&[display::MSG_SHEET_RESEND, &source, &have]);
                        }
                    } else if self.kind == Kind::Agent {
                        progress.requests.push(fields);
                    } else {
                        progress.log.push(format!(
                            "{}: ignoring {:?}",
                            self.label(),
                            fields.first().map(String::as_str).unwrap_or("")
                        ));
                    }
                }
                Ok(None) => break,
                Err(err) => {
                    // The stream can no longer be resynchronised, so the
                    // connection goes rather than the compositor guessing.
                    progress.log.push(format!("{}: protocol error: {err}", self.label()));
                    progress.gone = true;
                    break;
                }
            }
        }

        progress
    }

    /// Install a newly arrived tree.
    fn apply(&mut self, fonts: &Fonts, source: &str, version: u64) -> (bool, String) {
        let mut next = match Document::parse(source, version) {
            Ok(doc) => doc,
            Err(err) => {
                // The held tree stays on screen. A client that sent one bad
                // document has not necessarily stopped working, and blanking
                // its window would destroy the state the human was looking at.
                self.note = format!("rejected v{version}: {err}");
                return (true, format!("{}: {}", self.label(), self.note));
            }
        };

        self.reconcile(&mut next);

        let diff = match &self.doc {
            Some(held) => next.diff(held),
            None => Default::default(),
        };
        let first = self.doc.is_none();

        // An identical resend still advances the version, because events must
        // carry the version the application believes it is on. It just does not
        // cost a frame.
        if !first && diff.is_empty() {
            self.doc = Some(next);
            self.note = format!("v{version} identical, not repainted");
            return (false, format!("{}: {}", self.label(), self.note));
        }

        self.doc = Some(next);
        self.relayout(fonts);
        self.note = if first {
            format!("v{version} first tree, {} nodes", self.node_count())
        } else {
            format!("v{version} {}", diff.summary())
        };
        (true, format!("{}: {}", self.label(), self.note))
    }

    fn node_count(&self) -> usize {
        self.doc.as_ref().map_or(0, |doc| doc.tree.nodes.len())
    }

    /// Carry ephemeral state onto the incoming tree, and write back what the
    /// compositor owns.
    ///
    /// Anything whose node no longer exists is dropped here rather than left to
    /// resolve to nothing later: a focus ring on a control the application has
    /// removed is a lie about where the next keystroke goes.
    fn reconcile(&mut self, next: &mut Document) {
        // Focus is remembered by identity even when the node is not in the
        // tree that just arrived. A row outside a table's window has not been
        // removed, it is merely not being described, and forgetting where the
        // cell cursor was would mean scrolling past it and back lost it.
        // While the node is absent the key resolves to nothing, so there is
        // no ring, no caret and nowhere for a keystroke to land; when the
        // application describes it again the cursor is where it was left.
        //
        // The edit in progress is not kept, deliberately. Every keystroke was
        // already reported, so the application holds the value; keeping a
        // local copy across an absence would let a stale one overwrite it.

        // A dialog that has just appeared takes the keyboard: focus moves to
        // its first control, unless focus is already inside it. What a dialog
        // asks for is the next thing to type, and a caret left blinking in a
        // field behind it would be a caret nothing reaches.
        if let Some(front) = next.tree.modal() {
            let inside = self
                .focus
                .as_ref()
                .and_then(|key| next.index_of(key))
                .is_some_and(|index| next.tree.within(index, front));
            if !inside {
                let first = (front..next.tree.nodes.len()).find(|&index| {
                    next.tree.within(index, front)
                        && next.tree.node(index).tag.is_control()
                        && !next.tree.node(index).disabled()
                });
                self.focus = first.map(|index| next.key(index).to_owned());
                // A text control that opens with a value in it opens with the
                // caret at the end: what a dialog offers to be edited is
                // appended to or replaced, not typed into the front of.
                if let Some(index) = first
                    && matches!(next.tree.node(index).tag, Tag::Field | Tag::Editor)
                {
                    let value = next.tree.node(index).attr("value").unwrap_or("").to_owned();
                    let key = next.key(index).to_owned();
                    self.editing.entry(key).or_insert(Editing {
                        caret: value.chars().count(),
                        value,
                        anchor: None,
                        outstanding: Vec::new(),
                    });
                }
            }
        }

        self.scroll.retain(|key, _| next.has_key(key));
        // A cell being typed into is keyed by its element and its name, not by
        // a node, because it is not one. What has to still exist is the
        // element; the cell is a coordinate inside it and cannot go away on
        // its own. Pruning these as if they were node keys threw the edit away
        // on every re-render, which the application does after every
        // keystroke: typing `42` into a cell left `2` in it.
        self.editing.retain(|key, _| match key.split_once('!') {
            Some((element, _)) => next.has_key(element),
            None => next.has_key(key),
        });
        // A run of words whose element the application stopped sending is a
        // run over nothing.
        if let Some((key, ..)) = &self.text_run
            && !next.has_key(key)
        {
            self.text_run = None;
            self.running = false;
        }

        let keys: Vec<String> = self.editing.keys().cloned().collect();
        for key in keys {
            let Some(index) = next.index_of(&key) else { continue };
            let incoming = next.tree.node(index).attr("value").unwrap_or("").to_owned();
            let Some(state) = self.editing.get_mut(&key) else { continue };

            match state.outstanding.iter().position(|sent| *sent == incoming) {
                // The application echoed something we told it. Everything up to
                // and including that value is settled; anything typed since
                // still stands.
                Some(at) => {
                    state.outstanding.drain(..=at);
                }
                // Nothing outstanding and a value we do not have means the
                // application changed it on its own.
                None => {
                    if state.value != incoming {
                        state.value = incoming;
                        state.caret = state.caret.min(state.value.chars().count());
                    }
                    state.outstanding.clear();
                }
            }

            next.tree.nodes[index].set("value", &state.value);
        }
    }

    fn relayout(&mut self, fonts: &Fonts) {
        let Some(doc) = &self.doc else { return };
        let mut state = ui::Ephemeral {
            scroll: &mut self.scroll,
            scroll_x: &mut self.scroll_x,
            columns: &self.columns,
            context_at: self.context_at,
        };
        self.layout = ui::layout(fonts, doc, &self.frame, &mut state);
    }

    /// How tall this client's document wants to be at a given width, or `None`
    /// before its first tree has arrived.
    pub fn natural_height(&self, fonts: &Fonts, width: i32) -> Option<i32> {
        self.doc.as_ref().map(|doc| ui::natural_height(fonts, &doc.tree, width))
    }

    /// How wide the tree wants to be so nothing in it is squeezed.
    pub fn natural_width(&self, fonts: &Fonts) -> Option<i32> {
        self.doc.as_ref().map(|doc| ui::document_width(fonts, &doc.tree))
    }

    /// Whether the tree's top-level content asks to fill its window.
    pub fn wants_room(&self) -> bool {
        self.doc.as_ref().is_some_and(|doc| ui::wants_room(&doc.tree))
    }

    /// Move or resize this client's part of the screen.
    pub fn set_frame(&mut self, fonts: &Fonts, frame: Frame) {
        self.frame = frame;
        self.relayout(fonts);
    }

    /// The window title the document declares, for the chrome the compositor
    /// draws around it. Applications name their windows; they do not draw them.
    pub fn title(&self) -> &str {
        self.doc
            .as_ref()
            .and_then(|doc| doc.tree.node(Document::ROOT).attr("title"))
            .unwrap_or(&self.name)
    }

    /// The top-level node claiming a region, for a desk connection.
    pub fn region(&self, name: &str) -> Option<usize> {
        let doc = self.doc.as_ref()?;
        doc.tree
            .node(Document::ROOT)
            .children
            .iter()
            .copied()
            .find(|&child| doc.tree.node(child).attr("region") == Some(name))
    }

    /// Paint one branch, for a workspace whose regions do not paint
    /// consecutively.
    pub fn draw_region(&self, canvas: &mut Canvas, fonts: &Fonts, images: &Images, name: &str) {
        let (Some(doc), Some(index)) = (&self.doc, self.region(name)) else { return };
        ui::paint_subtree(
            canvas,
            fonts,
            &ui::Content { images, sheets: &self.sheets },
            &doc.tree,
            &self.layout,
            index,
            &self.focus_state(),
        );
    }

    /// Where the compositor believes focus and the caret are, in this tree.
    fn focus_state(&self) -> Focus {
        let Some(doc) = &self.doc else { return Focus::default() };

        // Press and scrollbar do not depend on anything being focused. The
        // first version returned early when no control held focus, which
        // silently kept scrollbars from ever appearing in a window that had
        // not been clicked into yet.
        let pressed = self
            .press
            .as_ref()
            .filter(|(_, since)| since.elapsed() < PRESS)
            .and_then(|(key, _)| doc.index_of(key));
        let scrollbar = self
            .scroll_shown
            .as_ref()
            .filter(|(_, since)| since.elapsed() < SCROLLBAR_LINGER)
            .and_then(|(key, _)| doc.index_of(key));

        let (node, caret) = match &self.focus {
            Some(key) => (
                doc.index_of(key),
                self.editing.get(key).map_or(0, |state| state.caret),
            ),
            None => (None, 0),
        };
        let anchor = self.focus.as_ref().and_then(|key| self.editing.get(key)?.anchor);
        // A cell being typed into: the element holds focus, the tree says
        // which cell the cursor is on, and the compositor's copy of what is
        // in it is what gets painted.
        let cell = self.focus.as_ref().and_then(|key| {
            let index = doc.index_of(key)?;
            if doc.tree.node(index).tag != Tag::Spreadsheet {
                return None;
            }
            let at = doc.tree.node(index).attr("cursor").and_then(sheet::parse)?;
            let state = self.editing.get(&cell_key(key, at))?;
            Some((at, state.value.clone()))
        });
        let caret = cell
            .as_ref()
            .and_then(|(at, _)| {
                let key = self.focus.as_deref()?;
                Some(self.editing.get(&cell_key(key, *at))?.caret)
            })
            .unwrap_or(caret);
        let run = self.text_run.as_ref().and_then(|(key, ..)| {
            let index = doc.index_of(key)?;
            let (from, to) = self.run_of(key)?;
            Some((index, from, to))
        });
        let anchor = cell
            .as_ref()
            .and_then(|(at, _)| {
                let key = self.focus.as_deref()?;
                self.editing.get(&cell_key(key, *at))?.anchor
            })
            .or(anchor);
        Focus { node, caret, anchor, pressed, caret_visible: self.caret_on, scrollbar, run, cell }
    }

    /// Whether keystrokes to this client would land in a text control, which is
    /// what decides if there is a caret to blink at all.
    pub fn focused_text(&self) -> bool {
        let Some(doc) = &self.doc else { return false };
        let Some(key) = &self.focus else { return false };
        doc.index_of(key)
            .is_some_and(|index| matches!(doc.tree.node(index).tag, Tag::Field | Tag::Editor))
    }

    /// True while something on this client is mid-animation, so the loop should
    /// keep producing frames.
    pub fn animating(&self) -> bool {
        self.press
            .as_ref()
            .is_some_and(|(_, since)| since.elapsed() < PRESS)
            // The lingering scrollbar needs one more frame to disappear in;
            // keeping frames coming until it expires is what delivers it.
            || self
                .scroll_shown
                .as_ref()
                .is_some_and(|(_, since)| since.elapsed() < SCROLLBAR_LINGER)
    }

    /// Whether a scrollbar thumb is currently being dragged.
    pub fn scroll_dragging(&self) -> bool {
        self.scroll_drag.is_some()
    }

    /// Take hold of a scrollbar if the point is on one.
    ///
    /// Checked before the ordinary hit test, because the bar overlays content:
    /// a press on the thumb must grab it, not select the list row underneath.
    /// A press on the track jumps the thumb there first, then drags it.
    fn grab_scrollbar(&mut self, fonts: &Fonts, x: i32, y: i32) -> bool {
        let Some(doc) = &self.doc else { return false };
        // Only a visible bar is grabbable. An invisible one that still caught
        // clicks would make the content's right edge mysteriously dead. A
        // grid's bars never fade, so they are always grabbable.
        let lit = self.focus_state().scrollbar;

        for scroller in self.layout.scrollers.iter().rev() {
            if !scroller.permanent && !scroller.horizontal && lit != Some(scroller.node) {
                continue;
            }
            let rect = self.layout.rect_of(scroller.node);
            let Some((track, thumb)) = ui::scrollbar_geometry(rect, scroller) else { continue };
            // The whole track answers, a little widened, because a hairline
            // thumb is a cruel target.
            let target = if scroller.horizontal {
                Rect::new(track.x, track.y - ui::sc(4), track.w, track.h + ui::sc(8))
            } else {
                Rect::new(track.x - ui::sc(4), track.y, track.w + ui::sc(8), track.h)
            };
            if !target.contains(x, y) {
                continue;
            }

            let key = doc.key(scroller.node).to_owned();
            let grab = if scroller.horizontal {
                if thumb.contains(x, y) { x - thumb.x } else { thumb.w / 2 }
            } else if thumb.contains(x, y) {
                y - thumb.y
            } else {
                thumb.h / 2
            };
            self.scroll_drag = Some((key, grab));
            self.scroll_across = scroller.horizontal;
            self.drag_scroll(fonts, x, y);
            return true;
        }
        false
    }

    /// Follow the hand while a thumb is held.
    pub fn drag_scroll(&mut self, fonts: &Fonts, x: i32, y: i32) -> bool {
        let Some((key, grab)) = self.scroll_drag.clone() else { return false };
        let Some(doc) = &self.doc else { return false };
        let Some(node) = doc.index_of(&key) else { return false };
        let Some(scroller) = self
            .layout
            .scrollers
            .iter()
            .find(|scroller| scroller.node == node && scroller.horizontal == self.scroll_across)
        else {
            return false;
        };

        let rect = self.layout.rect_of(node);
        let Some((track, thumb)) = ui::scrollbar_geometry(rect, scroller) else { return false };
        let (along, span, start, size) = if scroller.horizontal {
            (x, track.w, track.x, thumb.w)
        } else {
            (y, track.h, track.y, thumb.h)
        };
        let travel = (span - size).max(1);
        let furthest = scroller.furthest();
        let offset = ((along - grab - start) * furthest / travel).clamp(0, furthest);

        // Every bar moves an offset now, a grid's included. It used to be the
        // one exception: it was drawn against a sheet the compositor did not
        // hold, so dragging it asked the application for a different window.
        // The compositor holds the sheet, so there is nothing to ask, and the
        // flag that marked the exception now marks the only thing still true
        // of it, which is that its bar does not fade.
        if offset != scroller.offset {
            if scroller.horizontal {
                self.scroll_x.insert(key.clone(), offset);
            } else {
                self.scroll.insert(key.clone(), offset);
            }
            self.relayout(fonts);
        }
        self.scroll_shown = Some((key, Instant::now()));
        true
    }

    pub fn end_scroll_drag(&mut self) {
        self.scroll_drag = None;
    }

    pub fn draw(&self, canvas: &mut Canvas, fonts: &Fonts, images: &Images) {
        let Some(doc) = &self.doc else { return };
        ui::paint(
            canvas,
            fonts,
            &ui::Content { images, sheets: &self.sheets },
            &doc.tree,
            &self.layout,
            &self.focus_state(),
        );
    }

    /// A one-line description of the focused node as an agent would see it.
    pub fn focus_summary(&self) -> String {
        let Some(doc) = &self.doc else { return "focus: none".into() };
        let Some(index) = self.focus_state().node else { return "focus: none".into() };

        let node = doc.tree.node(index);
        let rect = self.layout.rect_of(index);
        let actions = awml::actions_of(&doc.tree, index).join(" ");
        format!(
            "focus: <{}> id={} at {},{} {}x{} actions: {}",
            node.tag.name(),
            node.id().unwrap_or("?"),
            rect.x,
            rect.y,
            rect.w,
            rect.h,
            if actions.is_empty() { "none" } else { &actions },
        )
    }

    // ---- input -------------------------------------------------------------

    /// Route one input event into this client. Returns true if the screen needs
    /// repainting.
    pub fn handle(&mut self, fonts: &Fonts, event: Event, clipboard: &mut Clipboard) -> bool {
        match event {
            Event::ButtonPressed { button: Button::Left, x, y } => self.click(fonts, x, y),
            Event::ButtonPressed { button: Button::Right, x, y } => self.context(fonts, x, y),
            Event::Scrolled { delta, x, y } => self.wheel(fonts, delta, x, y),
            Event::KeyPressed(key) => self.key(fonts, key, clipboard),
            _ => false,
        }
    }

    /// Whether the pointer is dragging a selection out of a text control, or
    /// a run out of static text. One question, because the screen arms one
    /// drag for both and the difference is this file's business.
    pub fn selecting(&self) -> bool {
        self.selecting.is_some() || self.running
    }

    /// What the compositor's edit menu would offer over a point, or `None`
    /// when the point is not on text at all and the press belongs to the
    /// application.
    ///
    /// Takes focus on the way, when the press is on a control that did not
    /// have it, so that Paste has somewhere to land. A control that is
    /// already focused is left exactly as it is, selection and all: a
    /// right-press on words you have just dragged out must not throw them
    /// away before offering to copy them.
    pub fn arm_edit(&mut self, fonts: &Fonts, x: i32, y: i32) -> Option<editmenu::Offer> {
        let doc = self.doc.as_ref()?;
        if let Some(index) = self.layout.hit(&doc.tree, x, y) {
            let node = doc.tree.node(index);
            if !node.tag.is_text() || node.disabled() || doc.tree.blocked(index) {
                return None;
            }
            let key = doc.key(index).to_owned();
            // A cell is the exception. Cut, copy and paste over a grid are
            // about cells, not about the characters inside one, and the
            // compositor knows nothing about which cells are chosen: a run
            // across a grid is the application's state, reported to it and
            // painted by it. So the other button on a cell stays the
            // application's, and its menu is the one that can say
            // "paste as values". Once a cell is being typed into it is a
            // field like any other, and then the words in it are the
            // compositor's to offer.
            let editable = true;
            if self.focus.as_deref() != Some(key.as_str()) {
                let caret = self.caret_at(fonts, index, x, y);
                let value = node.attr("value").unwrap_or_default().to_owned();
                self.focus = Some(key.clone());
                self.text_run = None;
                let state = self.editing.entry(key.clone()).or_insert(Editing {
                    value,
                    caret: 0,
                    anchor: None,
                    outstanding: Vec::new(),
                });
                state.caret = caret.min(state.value.chars().count());
                state.anchor = None;
            }
            let selection = self.editing.get(&key).and_then(Editing::selected).is_some();
            return Some(editmenu::Offer { selection, editable });
        }

        // Words on a page: nothing can be pasted into them, and there is
        // something to copy only if a run is being held over this one.
        let words = self.layout.text_at(&doc.tree, x, y)?;
        let key = doc.key(words).to_owned();
        Some(editmenu::Offer { selection: self.run_of(&key).is_some(), editable: false })
    }

    /// The run of static text the human is holding, if any.
    fn run_of(&self, key: &str) -> Option<(usize, usize)> {
        let (held, anchor, caret) = self.text_run.as_ref()?;
        if held != key {
            return None;
        }
        let (from, to) = (*anchor.min(caret), *anchor.max(caret));
        (from != to).then_some((from, to))
    }

    /// Whether a column edge is being dragged.
    pub fn sizing_column(&self) -> bool {
        self.column_drag.is_some()
    }

    /// Whether a run of cells is being dragged out.
    pub fn selecting_cells(&self) -> bool {
        self.cell_drag.is_some()
    }

    /// Whether a tab is being carried along its strip.
    pub fn moving_tab(&self) -> bool {
        self.tab_drag.is_some()
    }

    /// The other mouse button, on whatever is under it.
    ///
    /// It carries no meaning of its own: what a right-press means is the
    /// application's to decide, and all this does is say where it landed and
    /// on what. An application answers by opening one of its menus, and the
    /// compositor hangs that menu from here, which is the whole of what makes
    /// a context menu appear under the hand.
    ///
    /// Not in the agent's vocabulary. An agent opens a menu by naming it,
    /// which reaches the same commands without a pointer.
    fn context(&mut self, fonts: &Fonts, x: i32, y: i32) -> bool {
        self.context_at = Some((x, y));
        let Some(doc) = &self.doc else { return false };
        let target = self
            .layout
            .hit(&doc.tree, x, y)
            .and_then(|index| doc.tree.node(index).id())
            .unwrap_or_default()
            .to_owned();
        self.note = if target.is_empty() {
            format!("the other button at {x},{y}")
        } else {
            format!("the other button on {target}")
        };
        self.emit(&target, display::ACTION_CONTEXT, "");
        // The menu the application may open in answer hangs from the press,
        // so the layout has to be built against it.
        self.relayout(fonts);
        true
    }

    /// The grid an element's key names.
    fn grid_of(&self, key: &str) -> Option<&Grid> {
        let doc = self.doc.as_ref()?;
        let index = doc.index_of(key)?;
        self.layout.grids.iter().find(|grid| grid.node == index)
    }

    /// The id an application knows a spreadsheet by, from its key.
    fn grid_id(&self, key: &str) -> String {
        self.doc
            .as_ref()
            .and_then(|doc| doc.index_of(key))
            .and_then(|index| self.doc.as_ref()?.tree.node(index).id())
            .unwrap_or_default()
            .to_owned()
    }

    /// Carry a run of cells out under the pointer.
    ///
    /// One event per cell the run reaches, in the same spirit as one event
    /// per keystroke: what the application hears is what the hand did, as it
    /// does it, and the highlight it paints in answer is its own.
    pub fn drag_cells(&mut self, x: i32, y: i32) -> bool {
        let Some((key, anchor)) = self.cell_drag.clone() else { return false };
        // Both corners must be in the same grid: a run that started in one
        // sheet and ended in another is not a run of anything.
        let Some(grid) = self.grid_of(&key) else { return false };
        let Some(reached) = grid.cell_at(x, y) else { return false };
        if self.cell_extent == Some(reached) {
            return false;
        }
        self.cell_extent = Some(reached);
        let id = self.grid_id(&key);
        let (from, to) = (sheet::name(anchor), sheet::name(reached));
        self.note = format!("{from} through {to}");
        self.emit_cell(&id, display::ACTION_SELECT_RANGE, &to, &from);
        true
    }

    pub fn end_cell_drag(&mut self) {
        self.cell_drag = None;
        self.cell_extent = None;
    }

    /// Carry a tab along its strip.
    ///
    /// Where the pointer falls among the other tabs' middles is the slot this
    /// one belongs in, which is the navigation bar's rule from the navigation
    /// bar's function. The difference is who does the rearranging: the bar's
    /// order is the compositor's, so it reorders itself, and an application's
    /// is the application's, so it is told and answers with a new tree. One
    /// event per slot crossed, the way a run of cells sends one per cell.
    pub fn drag_tabs(&mut self, fonts: &Fonts, x: i32) -> bool {
        let Some((key, last)) = self.tab_drag.clone() else { return false };
        let Some(doc) = &self.doc else { return false };
        let Some(index) = doc.index_of(&key) else { return false };
        let Some(strip) = doc.tree.node(index).parent else { return false };
        let tabs = doc.tree.tabs(strip);
        let Some(held) = tabs.iter().position(|&tab| tab == index) else { return false };
        let rects: Vec<Rect> = tabs.iter().map(|&tab| self.layout.rect_of(tab)).collect();
        let slot = ui::tab_slot(&rects, held, x);
        if slot == last {
            return false;
        }
        self.tab_drag = Some((key, slot));
        self.act(fonts, index, "move", &slot.to_string()).is_ok()
    }

    pub fn end_tab_drag(&mut self) {
        self.tab_drag = None;
    }

    /// Extend the selection to wherever the pointer has got to.
    pub fn drag_select(&mut self, fonts: &Fonts, x: i32, y: i32) -> bool {
        if self.running {
            return self.drag_text(fonts, x, y);
        }
        let Some(key) = self.selecting.clone() else { return false };
        let Some(doc) = &self.doc else { return false };
        let Some(index) = doc.index_of(&key) else { return false };
        let caret = self.caret_at(fonts, index, x, y);
        let Some(state) = self.editing.get_mut(&key) else { return false };
        state.drag(caret)
    }

    /// Carry a run of static text out under the pointer.
    fn drag_text(&mut self, fonts: &Fonts, x: i32, y: i32) -> bool {
        let Some((key, _, caret)) = self.text_run.clone() else { return false };
        let Some(doc) = &self.doc else { return false };
        let Some(index) = doc.index_of(&key) else { return false };
        let reached = self.offset_at(fonts, index, x, y);
        if reached == caret {
            return false;
        }
        if let Some((_, _, caret)) = &mut self.text_run {
            *caret = reached;
        }
        true
    }

    /// Copy the run of static text the human is holding, if that is what this
    /// keystroke means. `None` when there is no run, or when the key is
    /// something a run has no answer for.
    fn copy_run(&mut self, clipboard: &mut Clipboard, key: Key) -> Option<bool> {
        let (held, ..) = self.text_run.clone()?;
        let (from, to) = self.run_of(&held)?;
        let doc = self.doc.as_ref()?;
        let index = doc.index_of(&held)?;
        let words: String = doc.tree.node(index).text.chars().skip(from).take(to - from).collect();
        match key {
            Key::Copy | Key::Cut => {
                // Cut is a copy here: there is nothing to take words out of.
                clipboard.set_text(&words);
                self.note = format!("copied {} character(s)", words.chars().count());
                Some(true)
            }
            Key::Escape => {
                self.text_run = None;
                self.note = "let go of the words".into();
                Some(true)
            }
            _ => None,
        }
    }

    /// Carry the far end of a run of static text with the keyboard.
    ///
    /// Less than a text box gets, because there is less: no caret to put
    /// down, so a run has to be started by pressing on the words. Once there
    /// is one, shift and an arrow reach further and Ctrl+A takes the whole
    /// paragraph, which is what the two of them mean everywhere.
    fn reach_run(&mut self, key: Key) -> Option<bool> {
        let (held, ..) = self.text_run.clone()?;
        let doc = self.doc.as_ref()?;
        let index = doc.index_of(&held)?;
        let length = doc.tree.node(index).text.chars().count();
        let (_, anchor, caret) = self.text_run.as_mut()?;
        match key {
            Key::ShiftLeft => *caret = caret.saturating_sub(1),
            Key::ShiftRight => *caret = (*caret + 1).min(length),
            Key::ShiftHome | Key::Home => *caret = 0,
            Key::ShiftEnd | Key::End => *caret = length,
            Key::SelectAll => {
                *anchor = 0;
                *caret = length;
            }
            _ => return None,
        }
        Some(true)
    }

    /// Which character of a static text element a point lands on.
    fn offset_at(&self, fonts: &Fonts, index: usize, x: i32, y: i32) -> usize {
        let Some(doc) = &self.doc else { return 0 };
        ui::text_offset_at(
            fonts,
            &doc.tree.node(index).text,
            &ui::style_at(&doc.tree, index),
            self.layout.rect_of(index),
            (x, y),
        )
    }

    pub fn end_select(&mut self) {
        if self.running {
            self.running = false;
            // A run of nothing is kept rather than dropped, unlike a
            // control's anchor. Nothing is painted for it and nothing can be
            // copied from it, and it is what shift and an arrow reach out
            // from: a press on words is the only way to say where a run over
            // them starts, since there is no caret to put down in a
            // paragraph. The reason a control's anchor cannot be kept the
            // same way is that a control can be typed into, and a leftover
            // anchor made the next character replace a run nobody dragged.
            return;
        }
        let Some(key) = self.selecting.take() else { return };
        if let Some(state) = self.editing.get_mut(&key)
            && state.anchor == Some(state.caret)
        {
            state.anchor = None;
        }
    }

    /// Which character of a text control a point lands on.
    fn caret_at(&self, fonts: &Fonts, index: usize, x: i32, y: i32) -> usize {
        let Some(doc) = &self.doc else { return 0 };
        let node = doc.tree.node(index);
        let value = node.attr("value").unwrap_or_default().to_owned();
        let style = ui::style_at(&doc.tree, index);
        let key = doc.key(index);
        let current = self.editing.get(key).map_or(0, |state| state.caret);
        ui::caret_at_point(
            fonts,
            &value,
            &style,
            node.tag,
            self.layout.rect_of(index),
            current,
            (x, y),
        )
    }

    /// Take hold of a column's trailing edge in a header, if the point is on
    /// one.
    fn grab_column(&mut self, x: i32, y: i32) -> bool {
        let Some(doc) = &self.doc else { return false };
        let grip = ui::column_grip();
        for grid in &self.layout.grids {
            if !grid.header.contains(x, y) {
                continue;
            }
            let (first, last) = (grid.visible().0.0, grid.visible().1.0);
            for column in first..=last {
                let edge = grid.body.x + grid.column_x(column) + grid.column_w(column)
                    - grid.offset.0;
                if (x - edge).abs() <= grip {
                    self.column_drag = Some((
                        doc.key(grid.node).to_owned(),
                        column,
                        x,
                        grid.column_w(column),
                    ));
                    return true;
                }
            }
        }
        false
    }

    /// Carry a column's edge with the pointer.
    pub fn drag_column(&mut self, fonts: &Fonts, x: i32) -> bool {
        let Some((key, column, from, width)) = self.column_drag.clone() else { return false };
        let Some(doc) = &self.doc else { return false };
        let Some(index) = doc.index_of(&key) else { return false };
        let floor = ui::column_floor(fonts, &doc.tree, index);
        let next = (width + (x - from)).max(floor);
        let at = format!("{key}#{column}");
        if self.columns.get(&at) == Some(&next) {
            return false;
        }
        self.columns.insert(at, next);
        self.relayout(fonts);
        true
    }

    pub fn end_column_drag(&mut self) {
        self.column_drag = None;
    }

    /// Resolve a point to a node and act on it.
    ///
    /// This is the same sequence an agent's intent will follow: find the node,
    /// check it is visible and enabled, then synthesize exactly one event. The
    /// agent path being the same path is what stops it becoming a second
    /// implementation that can disagree.
    fn click(&mut self, fonts: &Fonts, x: i32, y: i32) -> bool {
        if self.grab_column(x, y) {
            return true;
        }
        if self.grab_scrollbar(fonts, x, y) {
            return true;
        }

        let Some(doc) = &self.doc else { return false };
        let hit = self.layout.hit_with_overlays(&doc.tree, x, y);

        // A click anywhere but on an open dropdown or its options closes it,
        // and does nothing else: the press that dismisses a menu is not also
        // a press on what was behind it.
        if let Some(open) = doc.tree.open_overlays().last().copied()
            && !hit.is_some_and(|index| index == open || doc.tree.node(index).parent == Some(open))
        {
            let _ = self.act(fonts, open, "close", "");
            return true;
        }

        // Any press puts down whatever run of words was being held. A press
        // that lands on text starts a new one below.
        self.text_run = None;

        let Some(index) = hit else {
            // Nothing pressable here, but there may be something to read. A
            // press on words starts a run the human can copy: an agent's
            // answer is the thing in this system most worth copying, and it
            // is a `text` element like any other.
            if let Some(words) = self.layout.text_at(&doc.tree, x, y) {
                let key = doc.key(words).to_owned();
                let at = self.offset_at(fonts, words, x, y);
                let content = doc.tree.node(words).text.clone();
                let count = self.presses.press(x, y);
                // A double click takes the word, a third takes the line, and
                // the boundaries come from the same two functions a text box
                // uses, because a word is a word wherever it is written.
                let (from, to) = match count {
                    1 => (at, at),
                    2 => text::word_at(&content, at),
                    _ => text::line_at(&content, at),
                };
                self.text_run = Some((key.clone(), from, to));
                self.running = true;
                self.focus = None;
                self.note = format!("holding the words in {key} from {from} to {to}");
                return true;
            }
            // Clicking the gap between two controls is a real thing to have
            // done: it takes focus off whatever had it.
            self.focus = None;
            self.note = format!("clicked nothing at {x},{y}");
            return true;
        };

        let node = doc.tree.node(index);
        let tag = node.tag;
        let id = node.id().unwrap_or_default().to_owned();
        let key = doc.key(index).to_owned();
        let value = node.attr("value").unwrap_or_default().to_owned();

        if node.disabled() {
            // Not reported to the application at all. A disabled control has no
            // actions, and inventing one for it is exactly what the derived
            // action list exists to prevent.
            self.note = format!("{id} is disabled");
            return true;
        }

        let rect = self.layout.rect_of(index);
        self.focus = Some(key.clone());
        // A press with the ordinary button ends whatever the other one had
        // standing, so a menu bar's items hang from the menu and not from
        // wherever the last right-press happened to be.
        self.context_at = None;

        // A click on a text control places the caret and reports nothing: where
        // the caret is inside a value is not the application's business.
        // A press on a grid: which cell it landed on is arithmetic, and what
        // it means is choosing that cell. It is also where a run of cells
        // would start; whether it becomes one is decided by whether the
        // pointer moves, exactly as it is for a run of text.
        if tag == Tag::Spreadsheet {
            let at = self.grid_of(&key).and_then(|grid| grid.cell_at(x, y));
            if let Some(at) = at {
                let source = self.grid_of(&key).map(|grid| grid.source.clone()).unwrap_or_default();
                let editing = cell_key(&key, at);
                let name = sheet::name(at);

                // A second press in the same place opens the cell for
                // editing, which is what a double click means everywhere
                // else. It starts from what is in the cell rather than from
                // nothing: the first press already chose it, so this one is
                // the human saying they want to change what is there rather
                // than replace it.
                if self.presses.press(x, y) > 1 {
                    let value = self.published(&source, at);
                    self.editing.insert(editing, Editing::new(value));
                    self.note = format!("editing {name} in {id}");
                    return true;
                }

                self.cell_drag = Some((key.clone(), at));
                self.cell_extent = None;
                self.editing.remove(&editing);
                self.note = format!("{name} chosen in {id}");
                self.emit_cell(&id, display::ACTION_SELECT, "", &name);
                self.press = Some((key, Instant::now()));
                return true;
            }
            self.note = format!("{id}: nothing under {x},{y}");
            return true;
        }

        if tag.is_text() {
            let caret = self.caret_at(fonts, index, x, y);
            let count = self.presses.press(x, y);
            let state = self.editing.entry(key.clone()).or_insert(Editing::new(value));
            // The press is where a selection starts; the drag is what makes
            // it one. A press that never moves leaves anchor and caret in the
            // same place, which is no selection at all. A second press in the
            // same place takes the word under it and a third takes the line,
            // which is what every desktop does.
            if count > 1 {
                state.press_again(caret, count);
            } else {
                state.press(caret);
            }
            self.selecting = Some(key);
            self.note = format!("caret in {id} at {caret}");
            return true;
        }

        // Everything else goes through the same door an agent's intent does. A
        // click on a checkbox is a toggle rather than a click: `check` and
        // `uncheck` exist so an intent can be unconditional, but a human
        // pressing the box means invert it.
        // A dropdown's box opens or closes it, and one of its options is
        // chosen: what a person pressing each of them means.
        // A movable tab is also where a drag along the strip would start.
        // Whether it becomes one is decided by whether the pointer moves,
        // exactly as it is for a run of cells or a run of text. The cross is
        // not part of it: a press there is a close, not a grip.
        let carried = if tag == Tag::Tab
            && node.flag("movable")
            && !(node.flag("closable") && x >= rect.x + rect.w - ui::tab_close_w())
        {
            doc.tree
                .node(index)
                .parent
                .and_then(|strip| doc.tree.tabs(strip).iter().position(|&tab| tab == index))
        } else {
            None
        };

        let action = match tag {
            Tag::Checkbox => display::ACTION_TOGGLE,
            Tag::Select | Tag::Menu if node.flag("open") => "close",
            Tag::Select | Tag::Menu => "open",
            // The cross at a tab's right end closes it; anywhere else on it
            // chooses it. The same division the navigation bar makes, at the
            // same place on the tab, because it is the same cross.
            Tag::Tab if node.flag("closable") && x >= rect.x + rect.w - ui::tab_close_w() => {
                display::ACTION_CLOSE
            }
            // An option or a tab: pressing either means choosing it, so the
            // event is the one a person produced.
            Tag::Option | Tag::Tab => "select",
            _ => display::ACTION_CLICK,
        };
        if let Some(slot) = carried {
            self.tab_drag = Some((key, slot));
        }
        let _ = self.act(fonts, index, action, "");
        true
    }

    /// Perform one action from the closed vocabulary on one node.
    ///
    /// Both paths end here. A human's click resolves a point to a node and calls
    /// this; an agent's intent resolves a name to a node and calls this. One
    /// implementation, so the two cannot disagree about what an action does, and
    /// so an application cannot tell which of them it was.
    ///
    /// The action list is checked against what the element derives from its type
    /// and state, which is the same list the agent was shown. Nothing here
    /// trusts a verb because it arrived.
    pub fn act(
        &mut self,
        fonts: &Fonts,
        index: usize,
        action: &str,
        value: &str,
    ) -> Result<(), &'static str> {
        self.act_cell(fonts, index, action, value, "")
    }

    /// The same, naming a cell of a spreadsheet. Everything an agent does to
    /// a grid comes through here, because a cell is not a node and cannot be
    /// the thing an intent names.
    pub fn act_cell(
        &mut self,
        fonts: &Fonts,
        index: usize,
        action: &str,
        value: &str,
        cell: &str,
    ) -> Result<(), &'static str> {
        let Some(doc) = &self.doc else { return Err(agent::REASON_NO_SUCH_NODE) };
        let node = doc.tree.node(index);
        let tag = node.tag;
        let disabled = node.disabled();

        if disabled {
            return Err(agent::REASON_DISABLED);
        }
        if doc.tree.blocked(index) {
            return Err(agent::REASON_BLOCKED);
        }
        // Asked through `offers`, which knows a cell's `editable`. The
        // tag-only form answers for a cell that cannot be typed into,
        // whatever the application said, so an agent's type-text was refused
        // on a cell the human could type into perfectly well. Typing by hand
        // goes through `key` and consults no list at all, which is why this
        // only ever bit the agent.
        if !self.offers(index, action) {
            return Err(agent::REASON_UNSUPPORTED);
        }
        let Some(doc) = &self.doc else { return Err(agent::REASON_NO_SUCH_NODE) };
        let node = doc.tree.node(index);

        let id = node.id().unwrap_or_default().to_owned();
        let key = doc.key(index).to_owned();
        let checked = node.flag("checked");
        let selected = node.flag("selected");
        let current = node.attr("value").unwrap_or("").to_owned();

        // Focus is the compositor's. An application is never told about it,
        // which is why it is not in the event vocabulary at all.
        self.focus = Some(key.clone());
        // An agent's action is not a right-press, so a menu it opens hangs
        // from the menu rather than from wherever the human last pressed.
        self.context_at = None;

        // Anything that activates a control shows a press. Typing does not: the
        // characters appearing is the feedback, and a field that flashed on
        // every keystroke would be unreadable.
        if !matches!(action, "focus" | "type-text" | "clear" | "move") {
            self.press = Some((key.clone(), Instant::now()));
        }

        match action {
            "focus" => {}

            "click" => self.emit(&id, display::ACTION_CLICK, ""),

            "toggle" => self.emit(&id, display::ACTION_TOGGLE, ""),

            // The unconditional forms. They become the event a human would have
            // produced, which is a toggle, and only when the state has to move.
            // An agent asking for a box to be checked should not depend on a
            // state it read a moment ago.
            "check" | "uncheck" => {
                if checked != (action == "check") {
                    self.emit(&id, display::ACTION_TOGGLE, "");
                }
            }

            // A run of cells, named by its two corners. The value is the
            // other corner's id, which is checked here rather than passed on
            // trust: an application told about a corner that does not exist
            // would be told about a run that is not one.
            "select-range" if tag == Tag::Spreadsheet => {
                // Both corners are cells of this grid, checked here rather
                // than passed on trust: an application told about a corner
                // outside its own sheet has been told about a run that is not
                // one.
                let Some(far) = sheet::parse(value) else {
                    return Err(agent::REASON_NO_SUCH_NODE);
                };
                let Some(near) = sheet::parse(cell) else {
                    return Err(agent::REASON_NO_SUCH_NODE);
                };
                let Some(grid) = self.grid_of(&key) else {
                    return Err(agent::REASON_UNSUPPORTED);
                };
                if far.0 >= grid.columns
                    || far.1 >= grid.rows
                    || near.0 >= grid.columns
                    || near.1 >= grid.rows
                {
                    return Err(agent::REASON_NO_SUCH_NODE);
                }
                self.emit_cell(&id, display::ACTION_SELECT_RANGE, value, cell);
            }

            // Everything else a grid takes. All of it names a cell, because
            // the cell is where it happens and the element is only the frame
            // around it; the checks are the same ones a press goes through,
            // and the events are the same events a press produces.
            "select" | "type-text" | "clear" | "submit" if tag == Tag::Spreadsheet => {
                let Some(at) = sheet::parse(cell) else {
                    return Err(agent::REASON_NO_SUCH_NODE);
                };
                let Some(grid) = self.grid_of(&key) else {
                    return Err(agent::REASON_UNSUPPORTED);
                };
                if at.0 >= grid.columns || at.1 >= grid.rows {
                    return Err(agent::REASON_NO_SUCH_NODE);
                }
                let source = grid.source.clone();
                let editing = cell_key(&key, at);

                match action {
                    "select" => {
                        self.editing.remove(&editing);
                        self.emit_cell(&id, display::ACTION_SELECT, "", cell);
                    }
                    // The compositor's copy of the cell is kept for the same
                    // reason a field's is: an agent types a character at a
                    // time, and each one is the value the cell now has.
                    "type-text" | "clear" => {
                        let next =
                            if action == "clear" { String::new() } else { value.to_owned() };
                        let state = self
                            .editing
                            .entry(editing)
                            .or_insert_with(|| Editing::new(String::new()));
                        state.value = next.clone();
                        state.caret = next.chars().count();
                        self.relayout(fonts);
                        self.emit_cell(&id, display::ACTION_TYPE_TEXT, &next, cell);
                    }
                    _ => {
                        // Submit settles the edit: what the application hears
                        // is the value the cell ended up with, and the
                        // compositor's copy gives way to what it publishes.
                        let value = self
                            .editing
                            .remove(&editing)
                            .map(|state| state.value)
                            .unwrap_or_else(|| self.published(&source, at));
                        self.emit_cell(&id, display::ACTION_SUBMIT, &value, cell);
                    }
                }
            }

            // A dropdown's option is always reported when chosen, even the
            // one already chosen: choosing is also what closes the list, and
            // an application that hears nothing would leave it open.
            "select" if tag == Tag::Option => self.emit(&id, display::ACTION_SELECT, ""),

            "select" | "deselect" => {
                if selected != (action == "select") {
                    let verb = if action == "select" {
                        display::ACTION_SELECT
                    } else {
                        display::ACTION_DESELECT
                    };
                    self.emit(&id, verb, "");
                }
            }

            // A tab's close is not a state to be made true, it is a thing
            // done once: what it means is "take this one away", and the
            // application answers by not sending it again.
            "close" if tag == Tag::Tab => self.emit(&id, display::ACTION_CLOSE, ""),

            // A tab carried to another position. The slot is checked here
            // rather than passed on trust, for the same reason a run of
            // cells checks its other corner: an application told to put a
            // tab in a slot that does not exist has been told nothing. A tab
            // already in that slot sends nothing, like every other
            // unconditional form.
            "move" if tag == Tag::Tab => {
                let Some(doc) = &self.doc else { return Err(agent::REASON_NO_SUCH_NODE) };
                let tabs = doc
                    .tree
                    .node(index)
                    .parent
                    .map(|strip| doc.tree.tabs(strip))
                    .unwrap_or_default();
                let Ok(slot) = value.parse::<usize>() else {
                    return Err(agent::REASON_UNSUPPORTED);
                };
                if slot >= tabs.len() {
                    return Err(agent::REASON_UNSUPPORTED);
                }
                if tabs[slot] != index {
                    self.emit(&id, display::ACTION_MOVE, value);
                }
            }

            // The unconditional forms again: an intent says which way the
            // list should be, and one that already is that way sends nothing.
            "open" | "close" => {
                if node.flag("open") != (action == "open") {
                    let verb = if action == "open" { display::ACTION_OPEN } else { display::ACTION_CLOSE };
                    self.emit(&id, verb, "");
                }
            }

            "type-text" | "clear" => {
                let next = if action == "clear" { String::new() } else { value.to_owned() };
                let state = self.editing.entry(key).or_insert(Editing {
                    value: current,
                    caret: 0,
                    anchor: None,
                    outstanding: Vec::new(),
                });
                state.value = next.clone();
                state.caret = next.chars().count();
                state.outstanding.push(next.clone());

                if let Some(doc) = &mut self.doc {
                    doc.tree.nodes[index].set("value", &next);
                }
                self.relayout(fonts);
                self.emit(&id, display::ACTION_TYPE_TEXT, &next);
            }

            "submit" => {
                let value = self
                    .editing
                    .get(&key)
                    .map(|state| state.value.clone())
                    .unwrap_or(current);
                self.emit(&id, display::ACTION_SUBMIT, &value);
            }

            _ => return Err(agent::REASON_UNSUPPORTED),
        }

        self.note = format!("{action} on {id}");
        Ok(())
    }

    /// Whether a point is over an enabled text control, for the pointer shape.
    ///
    /// The compositor decides the shape from the same hit test that would route
    /// a click, so the beam appears exactly where clicking would place a caret
    /// and nowhere else.
    pub fn text_at(&self, x: i32, y: i32) -> bool {
        let Some(doc) = &self.doc else { return false };
        match self.layout.hit(&doc.tree, x, y) {
            Some(index) => {
                matches!(doc.tree.node(index).tag, Tag::Field | Tag::Editor)
                    && !doc.tree.node(index).disabled()
            }
            None => false,
        }
    }

    /// Resolve an id the way an agent names one.
    ///
    /// Ids are unique within a window, not globally, which is why this is scoped
    /// to one client and an agent has to name the application first.
    pub fn node_by_id(&self, id: &str) -> Option<usize> {
        let doc = self.doc.as_ref()?;
        (0..doc.tree.nodes.len()).find(|&index| doc.tree.node(index).id() == Some(id))
    }

    /// Where one cell of a spreadsheet is on screen, for the fake cursor.
    pub fn cell_rect(&self, index: usize, at: sheet::Ref) -> Option<Rect> {
        let grid = self.layout.grids.iter().find(|grid| grid.node == index)?;
        Some(grid.cell_rect(at))
    }

    pub fn rect_of(&self, index: usize) -> Rect {
        self.layout.rect_of(index)
    }

    pub fn is_disabled(&self, index: usize) -> bool {
        self.doc
            .as_ref()
            .is_some_and(|doc| doc.tree.node(index).disabled())
    }

    /// Whether a node is behind an open dialog.
    pub fn is_blocked(&self, index: usize) -> bool {
        self.doc.as_ref().is_some_and(|doc| doc.tree.blocked(index))
    }

    /// Whether an element offers an action in its current state.
    ///
    /// The same derived list the agent was shown, asked in one place so that
    /// the check before an action and the list in the view cannot disagree.
    /// An option of a closed dropdown offers nothing, and hearing that is
    /// more use to an agent than any remark about where it is on screen.
    pub fn offers(&self, index: usize, action: &str) -> bool {
        let Some(doc) = &self.doc else { return false };
        awml::actions_of(&doc.tree, index).contains(&action)
    }

    /// Whether the application declared that a human must approve this control
    /// before an agent may act on it.
    ///
    /// An app declaring its own permissions is a starting point rather than a
    /// security model: it can mark a destructive action false through
    /// carelessness or design. Nothing here should assume the declaration is the
    /// final word, which is why the check is a lookup rather than a cached flag.
    pub fn needs_approval(&self, index: usize) -> bool {
        self.doc
            .as_ref()
            .is_some_and(|doc| doc.tree.node(index).flag("must-ask-perms"))
    }

    /// Scroll whatever has to move so a node can be acted on.
    ///
    /// This runs before every intent, so an agent never expresses a scroll
    /// and never hears that something was out of view. It is the same idea as
    /// arranging an application's window: reachability is something the
    /// compositor makes true rather than a question it answers.
    ///
    /// Every scroll container above the node is moved, outermost first,
    /// because moving an outer one changes where the inner one sits and doing
    /// it the other way round undoes the work. A table is moved across, over
    /// the columns it holds. What it cannot do is move a table down: the rows
    /// on screen are the ones the application sent, so a row below the body
    /// is one only the application can bring up, and [`Client::ask_for_row`]
    /// is how it is asked.
    ///
    /// Returns whether the node is reachable now.
    pub fn reveal(&mut self, fonts: &Fonts, index: usize) -> bool {
        let Some(doc) = &self.doc else { return false };

        // Containers above the node, outermost first.
        let mut containers: Vec<usize> = Vec::new();
        let mut at = doc.tree.node(index).parent;
        while let Some(node) = at {
            if doc.tree.node(node).tag == Tag::Scroll {
                containers.push(node);
            }
            at = doc.tree.node(node).parent;
        }
        containers.reverse();

        for container in containers {
            self.reveal_within(fonts, index, container);
        }

        self.layout.is_visible(index)
    }

    /// Move a grid so that one of its cells is on screen.
    ///
    /// The compositor can do this itself now, which it could not when the
    /// rows on screen were the ones the application chose to describe: it
    /// holds the sheet, so bringing a cell into view is two offsets and a
    /// relayout rather than a question and an answer.
    pub fn reveal_cell(&mut self, fonts: &Fonts, index: usize, at: sheet::Ref) -> bool {
        let Some(doc) = &self.doc else { return false };
        let key = doc.key(index).to_owned();
        let Some(grid) = self.grid_of(&key) else { return false };
        if at.0 >= grid.columns || at.1 >= grid.rows {
            return false;
        }

        let (content_w, content_h) = grid.content();
        let (mut across, mut down) = grid.offset;
        let left = grid.column_x(at.0);
        let right = left + grid.column_w(at.0);
        if left < across {
            across = left;
        } else if right > across + grid.body.w {
            across = right - grid.body.w;
        }
        let top = at.1 as i32 * grid.row_h;
        if top < down {
            down = top;
        } else if top + grid.row_h > down + grid.body.h {
            down = top + grid.row_h - grid.body.h;
        }
        let across = across.clamp(0, (content_w - grid.body.w).max(0));
        let down = down.clamp(0, (content_h - grid.body.h).max(0));

        if (across, down) == grid.offset {
            return true;
        }
        self.scroll_x.insert(key.clone(), across);
        self.scroll.insert(key.clone(), down);
        self.scroll_shown = Some((key, Instant::now()));
        self.relayout(fonts);
        true
    }

    /// Move one container so that a node inside it comes into view.
    fn reveal_within(&mut self, fonts: &Fonts, index: usize, container: usize) {
        let Some(doc) = &self.doc else { return };
        let key = doc.key(container).to_owned();
        let horizontal = false;

        let rect = self.layout.rect_of(index);
        let view = self.layout.rect_of(container);
        let held = self
            .layout
            .scrollers
            .iter()
            .find(|scroller| scroller.node == container && scroller.horizontal == horizontal);
        let Some(scroller) = held else { return };
        let (offset, furthest) = (scroller.offset, scroller.furthest());

        // Everything scrolls down, over content laid out in full. A grid is
        // the one thing that also moves across, and it does it in
        // `reveal_cell`, which knows about cells; this one knows about nodes.
        let shift = if rect.y < view.y {
            rect.y - view.y
        } else if rect.y + rect.h > view.y + view.h {
            rect.y + rect.h - (view.y + view.h)
        } else {
            return;
        };

        let next = (offset + shift).clamp(0, furthest);
        if next == offset {
            return;
        }
        if horizontal {
            self.scroll_x.insert(key.clone(), next);
        } else {
            self.scroll.insert(key.clone(), next);
        }
        // The bar lights up for the compositor's scrolling exactly as it does
        // for the human's wheel, so the human watching sees where the view
        // moved and why.
        self.scroll_shown = Some((key, Instant::now()));
        self.relayout(fonts);
        self.note = format!("scrolled to reveal {}", self.label());
    }

    /// A rectangle of one of this client's sheets, as rows of values.
    ///
    /// Tab-separated, one line per row, and nothing else: no elements, no
    /// descriptions, no action lists. All three of those are the same for
    /// every cell in a sheet, so they are said once on the element and this
    /// carries what is actually in there.
    ///
    /// A range that runs past the sheet is cut to it rather than refused: an
    /// agent asking for A1:Z100 of a small sheet is asking to see the sheet.
    pub fn cells(&self, id: &str, range: &str) -> String {
        let Some(doc) = &self.doc else { return String::new() };
        let Some(index) = self.node_by_id(id) else { return String::new() };
        if doc.tree.node(index).tag != Tag::Spreadsheet {
            return String::new();
        }
        let source = doc.tree.node(index).attr("source").unwrap_or_default();
        let Some(sheet) = self.sheets.get(source) else { return String::new() };
        let Some(((x0, y0), (x1, y1))) = sheet::parse_range(range) else {
            return String::new();
        };
        let grid = self.layout.grids.iter().find(|grid| grid.node == index);
        let (columns, rows) = grid.map_or((x1 + 1, y1 + 1), |grid| (grid.columns, grid.rows));
        let (x1, y1) = (x1.min(columns.saturating_sub(1)), y1.min(rows.saturating_sub(1)));

        let mut out = String::new();
        for row in y0..=y1 {
            for column in x0..=x1 {
                if column > x0 {
                    out.push('\t');
                }
                out.push_str(sheet.get((column, row)).unwrap_or_default());
            }
            out.push('\n');
        }
        out
    }

    /// Queue one frame for this client.
    pub fn send(&mut self, fields: &[&str]) {
        self.pending.extend_from_slice(&awproto::encode(fields));
        self.flush();
    }

    fn wheel(&mut self, fonts: &Fonts, delta: i32, x: i32, y: i32) -> bool {
        let Some(doc) = &self.doc else { return false };

        // A grid scrolls like anything else now: the compositor holds the
        // sheet, so a notch is an offset rather than a question. Its two
        // scrollers share a node, so the one down is the one taken here and
        // the one across belongs to the bar.
        //
        // Innermost first, and the first that can still move takes the
        // notch: a container at its end, or one that never overflowed, hands
        // it outward rather than swallowing it. Positive delta is a push away
        // from the human, which moves the content down and the viewport up.
        //
        // A grid moves by whole rows, because half a row of a spreadsheet
        // showing is not a thing anybody wants; everything else moves by
        // pixels, because its content has no unit.
        let Some((key, next, furthest)) = self
            .layout
            .scrollers_at(x, y)
            .filter(|scroller| !scroller.horizontal)
            .find_map(|scroller| {
                let furthest = scroller.furthest();
                let step = if scroller.step > 1 {
                    scroller.step * WHEEL_ROWS
                } else {
                    ui::wheel_step()
                };
                let next = (scroller.offset - delta * step).clamp(0, furthest);
                (next != scroller.offset).then(|| (doc.key(scroller.node).to_owned(), next, furthest))
            })
        else {
            return false;
        };

        self.scroll.insert(key.clone(), next);
        self.scroll_shown = Some((key, Instant::now()));
        self.relayout(fonts);
        self.note = format!("scrolled to {next} of {furthest}");
        true
    }

    fn key(&mut self, fonts: &Fonts, key: Key, clipboard: &mut Clipboard) -> bool {
        if key == Key::Tab {
            return self.focus_next();
        }

        // A run of words held over static text answers to copy and to nothing
        // else. There is no caret in it to move, nothing to cut out of it and
        // nowhere in it to paste, so this is the whole of what it does.
        if let Some(words) = self.copy_run(clipboard, key) {
            return words;
        }
        if let Some(reached) = self.reach_run(key) {
            return reached;
        }

        let Some(doc) = &self.doc else { return false };
        let Some(focus_key) = self.focus.clone() else { return false };
        let Some(index) = doc.index_of(&focus_key) else { return false };
        // A control behind a dialog keeps its focus ring but not the keyboard.
        if doc.tree.blocked(index) {
            return false;
        }

        let node = doc.tree.node(index);
        let tag = node.tag;
        if tag == Tag::Spreadsheet && !node.disabled() {
            return self.grid_key(fonts, index, key, clipboard);
        }
        if !tag.is_text() || node.disabled() {
            return false;
        }
        let editable = true;
        let id = node.id().unwrap_or_default().to_owned();
        let seed = node.attr("value").unwrap_or_default().to_owned();
        // A compositor-internal attribute for chat-shaped editors: Enter
        // submits and Shift+Enter breaks the line, the convention every
        // messenger keeps. Without it an editor keeps the catalogue's rule,
        // Enter breaks the line, because a general editor has no submit.
        let enter_submits = node.flag("enter-submits");
        let typing = self.editing.contains_key(&focus_key);

        let _ = (editable, typing);
        let state = self.editing.entry(focus_key.clone()).or_insert_with(|| {
            let mut state = Editing::new(seed.clone());
            // Focus arrived without a press, from Tab or from an agent, so
            // there is no point to put the caret at.
            state.caret = 0;
            state
        });

        // Everything a text box does with a key is one implementation, in
        // `text.rs`. This was the third place that needed it and the only one
        // that had it: the start menu's prompt and the navigation bar's
        // rename field had `push` and `pop` and nothing else, so neither
        // could be selected in, copied out of or pasted into. None of it
        // reaches the application, which hears only the value the control
        // ended up with.
        let outcome = state.key(key, clipboard, tag == Tag::Editor);
        let mut changed = outcome == Edit::Changed;

        if outcome == Edit::Ignored {
            match key {
                // Enter confirms a field and inserts a newline in an editor.
                // That is the whole reason the two elements are separate: an
                // editor offers no `submit` because Enter already means
                // something else in it. An editor marked `enter-submits`
                // swaps the two: Enter confirms and Shift+Enter breaks the
                // line. In a field the shift is simply not load-bearing:
                // there is no line to break, so both confirm.
                //
                // Not in `text.rs` because what Enter means is exactly what
                // differs between the boxes that share it: here it submits,
                // in the start menu it makes a workspace, in the navigation
                // bar it commits a name.
                Key::Enter | Key::ShiftEnter => {
                    let newline = match tag {
                        Tag::Editor if enter_submits => key == Key::ShiftEnter,
                        Tag::Editor => true,
                        _ => false,
                    };
                    if newline {
                        state.insert("\n");
                        changed = true;
                    } else {
                        let value = state.value.clone();
                        self.note = format!("submitted {id}");
                        self.emit(&id, display::ACTION_SUBMIT, &value);
                        return true;
                    }
                }
                _ => return false,
            }
        }

        if !changed {
            let caret = state.caret;
            self.note = format!("caret in {id} at {caret}");
            return true;
        }

        state.anchor = None;
        let value = state.value.clone();
        state.outstanding.push(value.clone());
        self.note = format!("typed into {id}");

        // The local copy is what is painted, so the screen has to be laid out
        // against it: a growing editor changes how much room it needs.
        if let Some(doc) = &mut self.doc {
            doc.tree.nodes[index].set("value", &value);
        }
        self.relayout(fonts);
        self.emit(&id, display::ACTION_TYPE_TEXT, &value);
        true
    }

    /// Everything the keyboard does over a grid.
    ///
    /// The cursor is the *application's*: it arrives on the element as
    /// `cursor` and moves because the application was told to move it, which
    /// is the same event a press on a cell produces. So an arrow key here is
    /// a `select` on the cell beside the one the tree named, not a change the
    /// compositor makes and hopes the application agrees with.
    ///
    /// Typing is the other way round. The characters are the compositor's
    /// until the application echoes them, exactly as they are in a field: the
    /// local copy is painted so a keystroke shows at once, and what the
    /// application hears is the value the cell ended up with.
    fn grid_key(&mut self, fonts: &Fonts, index: usize, key: Key, clipboard: &mut Clipboard) -> bool {
        let Some(doc) = &self.doc else { return false };
        let node = doc.tree.node(index);
        let id = node.id().unwrap_or_default().to_owned();
        let element = doc.key(index).to_owned();
        let Some(cursor) = node.attr("cursor").and_then(sheet::parse) else { return false };
        let Some(grid) = self.grid_of(&element) else { return false };
        let (rows, columns) = (grid.rows, grid.columns);
        let source = grid.source.clone();
        let editing = cell_key(&element, cursor);
        let typing = self.editing.contains_key(&editing);

        let step = |at: sheet::Ref, across: i32, down: i32| -> sheet::Ref {
            (
                (at.0 as i32 + across).clamp(0, columns as i32 - 1) as u32,
                (at.1 as i32 + down).clamp(0, rows as i32 - 1) as u32,
            )
        };

        // Enter on a cell nobody is typing into opens it for editing, and
        // opens it on what is already in it: a human who wanted to replace
        // the contents would have started typing, which is the other way in
        // and starts from nothing. Enter *while* editing is the commit, and
        // it moves down, which is the arm below.
        if key == Key::Enter && !typing {
            let was = self.published(&source, cursor);
            // The caret goes to the end, not over the whole value: opening a
            // cell to edit it is not the same as opening it to replace it,
            // and replacing is what typing straight into a chosen cell does.
            self.editing.insert(editing, Editing::new(was));
            self.note = format!("editing {} in {id}", sheet::name(cursor));
            return true;
        }

        // Where the arrows take the cursor, and where shift and an arrow
        // reach instead. Up and down always leave a cell, exactly as they do
        // in every spreadsheet; left and right mean the caret once there is
        // one to move.
        let moved = match key {
            Key::Up => Some((step(cursor, 0, -1), false)),
            Key::Down | Key::Enter => Some((step(cursor, 0, 1), false)),
            Key::Left if !typing => Some((step(cursor, -1, 0), false)),
            Key::Right if !typing => Some((step(cursor, 1, 0), false)),
            Key::ShiftUp => Some((step(self.reached(cursor), 0, -1), true)),
            Key::ShiftDown => Some((step(self.reached(cursor), 0, 1), true)),
            Key::ShiftLeft if !typing => Some((step(self.reached(cursor), -1, 0), true)),
            Key::ShiftRight if !typing => Some((step(self.reached(cursor), 1, 0), true)),
            _ => None,
        };
        if let Some((at, extend)) = moved {
            // Whatever was being typed is already with the application,
            // keystroke by keystroke, so leaving the cell is not a commit:
            // there is nothing left to say.
            self.editing.remove(&editing);
            if extend {
                if self.cell_extent == Some(at) {
                    return false;
                }
                self.cell_extent = Some(at);
                self.note = format!("{} through {}", sheet::name(cursor), sheet::name(at));
                self.emit_cell(&id, display::ACTION_SELECT_RANGE, &sheet::name(at), &sheet::name(cursor));
            } else {
                if at == cursor {
                    return false;
                }
                self.cell_extent = None;
                self.note = format!("{} chosen in {id}", sheet::name(at));
                self.emit_cell(&id, display::ACTION_SELECT, "", &sheet::name(at));
            }
            self.reveal_cell(fonts, index, at);
            return true;
        }

        if key == Key::Escape && typing {
            // Give the cell back what the application last published. Every
            // keystroke was already reported, so undoing has to be reported
            // too, as the value it ends on.
            self.editing.remove(&editing);
            let was = self.published(&source, cursor);
            self.note = format!("cancelled the edit in {}", sheet::name(cursor));
            self.emit_cell(&id, display::ACTION_TYPE_TEXT, &was, &sheet::name(cursor));
            return true;
        }

        // A cell nobody is typing into has no caret and no selection, so
        // there is nothing in it for a key to move or copy. Only a key that
        // puts characters in begins an edit, and when one does the cell
        // starts empty: the first character replaces what was there, which is
        // what every spreadsheet does. Ctrl+C over a grid means the chosen
        // *cells*, which is the application's to answer and not this.
        if !typing && !matches!(key, Key::Char(_) | Key::Paste) {
            return false;
        }
        let state = self.editing.entry(editing).or_insert_with(|| Editing::new(String::new()));
        if state.key(key, clipboard, false) != Edit::Changed {
            return true;
        }
        let value = state.value.clone();
        self.note = format!("typed into {}", sheet::name(cursor));
        self.emit_cell(&id, display::ACTION_TYPE_TEXT, &value, &sheet::name(cursor));
        true
    }

    /// The far corner of a run being reached out with the keyboard, or the
    /// cursor when there is not one yet.
    fn reached(&self, cursor: sheet::Ref) -> sheet::Ref {
        self.cell_extent.unwrap_or(cursor)
    }

    /// What the application last published for a cell.
    fn published(&self, source: &str, at: sheet::Ref) -> String {
        self.sheets
            .get(source)
            .and_then(|sheet| sheet.get(at))
            .unwrap_or_default()
            .to_owned()
    }

    /// Move focus to the next enabled control, wrapping.
    fn focus_next(&mut self) -> bool {
        let Some(doc) = &self.doc else { return false };

        let order: Vec<usize> = (0..doc.tree.nodes.len())
            .filter(|&index| {
                let node = doc.tree.node(index);
                node.tag.is_control() && !doc.tree.inert(index) && self.layout.is_visible(index)
            })
            .collect();
        if order.is_empty() {
            return false;
        }

        let current = self.focus.as_ref().and_then(|key| doc.index_of(key));
        let next = match current.and_then(|index| order.iter().position(|&at| at == index)) {
            Some(position) => order[(position + 1) % order.len()],
            None => order[0],
        };

        self.focus = Some(doc.key(next).to_owned());
        self.note = format!("focus moved to {}", doc.tree.node(next).id().unwrap_or("?"));
        true
    }

    // ---- output ------------------------------------------------------------

    /// Send one event, stamped with the version of the tree it was generated
    /// against.
    fn emit(&mut self, target: &str, action: &str, value: &str) {
        self.emit_cell(target, action, value, "");
    }

    /// The same, naming a cell of a spreadsheet as well.
    fn emit_cell(&mut self, target: &str, action: &str, value: &str, cell: &str) {
        let event = display::Event {
            version: self.version(),
            target: target.to_owned(),
            action: action.to_owned(),
            value: value.to_owned(),
            cell: cell.to_owned(),
        };
        self.pending.extend_from_slice(&event.encode());
        self.flush();
    }

    /// Push as much of the backlog as the kernel will take.
    ///
    /// Never blocks. Events are tens of bytes against a socket buffer measured
    /// in hundreds of kilobytes, so in practice this always completes in one
    /// write; the backlog exists so that a client which has stopped reading
    /// cannot stall the process that owns the screen.
    pub fn flush(&mut self) {
        while !self.pending.is_empty() {
            match self.stream.write(&self.pending) {
                Ok(0) => break,
                Ok(n) => {
                    self.pending.drain(..n);
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    self.broken = true;
                    return;
                }
            }
        }

        // A client that is not slow but stopped is not worth holding memory for.
        if self.pending.len() >= MAX_BACKLOG {
            self.broken = true;
        }
    }
}

/// Where the compositor's copy of one cell being typed into is kept.
///
/// Cells are not nodes, so there is no document key to use. This is the
/// element's key and the cell's name, which is stable in exactly the same way:
/// it survives a re-render, and it names a different thing the moment the
/// element does.
fn cell_key(element: &str, at: sheet::Ref) -> String {
    format!("{element}!{}", sheet::name(at))
}





#[cfg(test)]
mod tests {
    use super::*;

    use crate::paint::Rect;
    use crate::ui::Frame;
    use awproto::Decoder;
    use std::os::unix::net::UnixStream;

    /// A client holding one tree, framed into a rectangle.
    ///
    /// The peer end of the socketpair comes back with it: dropping it would
    /// break the connection and every event this client tried to send would
    /// mark it broken instead of arriving.
    fn framed(source: &str, frame: Rect) -> (Fonts, Client, UnixStream) {
        ui::set_scale(1.0);
        let fonts = Fonts::load().expect("the faces are compiled in");
        let (ours, peer) = UnixStream::pair().expect("a socketpair");
        let mut client =
            Client::adopt(Kind::App, 1, "test".to_owned(), 0, ours).expect("adopted");
        client.apply(&fonts, source, 1);
        client.set_frame(&fonts, Frame::Whole(frame));
        (fonts, client, peer)
    }

    /// A column of buttons taller than any window will give it.
    fn tall_list() -> String {
        let mut out = String::from("<window pad=\"none\"><scroll grow=\"true\"><vstack>");
        for at in 0..40 {
            out.push_str(&format!(
                "<button id=\"b{at}\" label=\"row {at}\" description=\"Row {at}\"/>"
            ));
        }
        out.push_str("</vstack></scroll></window>");
        out
    }

    /// Acting on a node that is scrolled away brings it into view rather than
    /// refusing. This is the whole of why `scroll-into-view` no longer exists:
    /// reachability is made true instead of being asked about.
    #[test]
    fn a_scrolled_away_node_is_revealed_rather_than_refused() {
        let (fonts, mut client, _peer) = framed(&tall_list(), Rect::new(0, 0, 300, 200));
        let index = client.node_by_id("b39").expect("the last row exists in the tree");
        assert!(!client.layout.is_visible(index), "the last of forty rows fitted in 200 pixels");

        assert!(client.reveal(&fonts, index), "reveal did not report success");
        assert!(client.layout.is_visible(index), "the node is still out of view");
    }

    /// A node inside a scroll container inside another one. The outer has to
    /// move first: moving the inner one first puts the node where the outer
    /// then carries it away from again.
    #[test]
    fn nested_scroll_containers_are_moved_outermost_first() {
        let mut inner = String::from(
            "<window pad=\"none\"><scroll grow=\"true\"><vstack>             <button id=\"top\" label=\"top\" description=\"Top\"/>",
        );
        for at in 0..30 {
            inner.push_str(&format!(
                "<button id=\"pad{at}\" label=\"pad\" description=\"Padding\"/>"
            ));
        }
        inner.push_str("<scroll><vstack>");
        for at in 0..30 {
            inner.push_str(&format!(
                "<button id=\"deep{at}\" label=\"deep {at}\" description=\"Deep {at}\"/>"
            ));
        }
        inner.push_str("</vstack></scroll></vstack></scroll></window>");

        let (fonts, mut client, _peer) = framed(&inner, Rect::new(0, 0, 300, 200));
        let index = client.node_by_id("deep29").expect("the deep row exists");
        assert!(!client.layout.is_visible(index), "it was somehow already in view");
        assert!(client.reveal(&fonts, index), "reveal did not report success");
        assert!(client.layout.is_visible(index), "a nested container was not moved");
    }

    /// Whatever the client has said to its application, without waiting for
    /// more. Non-blocking, so a test that expected an event and got none
    /// fails rather than hanging.
    fn said(peer: &mut UnixStream) -> Vec<Vec<String>> {
        peer.set_nonblocking(true).unwrap();
        let mut decoder = Decoder::with_limit(1024 * 1024);
        let mut buf = [0u8; 8192];
        loop {
            match peer.read(&mut buf) {
                Ok(0) => break,
                Ok(read) => decoder.feed(&buf[..read]),
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => panic!("reading the peer: {err}"),
            }
        }
        let mut out = Vec::new();
        while let Some(fields) = decoder.next_frame().unwrap() {
            out.push(fields);
        }
        out
    }

    /// The other button says where it landed and on what, and nothing else.
    /// What it means is the application's to decide.
    #[test]
    fn the_other_button_reports_what_was_under_it() {
        let source = r#"<window pad="none">
             <button id="go" label="Go" description="Does the thing"/>
           </window>"#;
        let (fonts, mut client, mut peer) = framed(source, Rect::new(0, 0, 400, 300));
        let mut clipboard = Clipboard::default();
        let go = client.layout.rect_of(client.node_by_id("go").unwrap());

        client.handle(
            &fonts,
            Event::ButtonPressed { button: Button::Right, x: go.x + 2, y: go.y + 2 },
            &mut clipboard,
        );
        let said = said(&mut peer);
        let context = said
            .iter()
            .find(|fields| fields[3] == display::ACTION_CONTEXT)
            .unwrap_or_else(|| panic!("nothing was reported: {said:?}"));
        assert_eq!(context[2], "go", "reported on the wrong node");
        // And it is remembered, because a menu opened in answer hangs from it.
        assert_eq!(client.context_at, Some((go.x + 2, go.y + 2)));
    }

    /// A grid, with the cells published the way an application publishes
    /// them: on their own frames, before the tree that claims their version.
    fn grid(client: &mut Client, fonts: &Fonts, cursor: &str) {
        let markup = format!(
            r#"<window pad="none"><spreadsheet id="sheet" grow="true" source="book"
                 version="3" rows="1000" columns="26" cursor="{cursor}"
                 description="The grid"/></window>"#
        );
        client.sheets.apply("book", 0, 1, (0, 0), &["Region".into(), "Q1".into()]);
        client.sheets.apply("book", 1, 2, (0, 1), &["North".into(), "1240".into()]);
        client.sheets.apply("book", 2, 3, (0, 499), &["far down".into()]);
        client.apply(fonts, &markup, client.version() + 1);
        client.set_frame(fonts, Frame::Whole(Rect::new(0, 0, 400, 300)));
    }

    /// A run of cells arrives as a run, applies to the version it names, and
    /// is refused when it names one nobody holds. The compositor never works
    /// out what changed; it is told, and what it is told is checked.
    #[test]
    fn cells_arrive_as_runs_against_a_version() {
        let (fonts, mut client, _peer) =
            framed(r#"<window pad="none"><text>x</text></window>"#, Rect::new(0, 0, 400, 300));
        grid(&mut client, &fonts, "A1");

        assert_eq!(client.cells("sheet", "A1:B2"), "Region	Q1
North	1240
");
        // Past what was published is empty rather than missing: an agent
        // asking for a rectangle is asking about the rectangle.
        assert_eq!(client.cells("sheet", "A3:B3"), "	
");
        // A range that runs past the sheet is cut to it.
        assert!(!client.cells("sheet", "A1:ZZ2000").is_empty());
    }

    /// A press on a grid chooses the cell it landed on, a drag reports a run,
    /// and both name the cell beside the action, because a cell is a
    /// coordinate rather than a node.
    #[test]
    fn a_press_on_a_grid_names_the_cell_it_landed_on() {
        let (fonts, mut client, mut peer) =
            framed(r#"<window pad="none"><text>x</text></window>"#, Rect::new(0, 0, 400, 300));
        grid(&mut client, &fonts, "A1");
        let mut clipboard = Clipboard::default();
        let _ = said(&mut peer);

        let index = client.node_by_id("sheet").unwrap();
        let b2 = client.cell_rect(index, (1, 1)).unwrap();
        client.handle(
            &fonts,
            Event::ButtonPressed { button: Button::Left, x: b2.x + 2, y: b2.y + 2 },
            &mut clipboard,
        );
        let heard = said(&mut peer);
        let chosen = heard
            .iter()
            .find(|fields| fields[3] == display::ACTION_SELECT)
            .unwrap_or_else(|| panic!("nothing was chosen: {heard:?}"));
        assert_eq!(chosen[2], "sheet", "the event does not name the element");
        assert_eq!(chosen[5], "B2", "the event does not name the cell");

        // Carried to another cell: one run, two corners.
        let d3 = client.cell_rect(index, (3, 2)).unwrap();
        assert!(client.drag_cells(d3.x + 2, d3.y + 2), "the drag reported nothing");
        let heard = said(&mut peer);
        let run = heard
            .iter()
            .find(|fields| fields[3] == display::ACTION_SELECT_RANGE)
            .unwrap_or_else(|| panic!("no run: {heard:?}"));
        assert_eq!(run[5], "B2", "the run does not start where the press did");
        assert_eq!(run[4], "D3", "the run does not reach where the pointer is");
    }

    /// Typing into a cell is the compositor's until the application echoes
    /// it: the keystroke shows at once and what the application hears is the
    /// value the cell ended up with, exactly as in a field.
    #[test]
    fn typing_into_a_cell_reports_the_value_it_ended_up_with() {
        let (fonts, mut client, mut peer) =
            framed(r#"<window pad="none"><text>x</text></window>"#, Rect::new(0, 0, 400, 300));
        grid(&mut client, &fonts, "B2");
        let mut clipboard = Clipboard::default();
        client.focus = Some(client.doc.as_ref().unwrap().key(client.node_by_id("sheet").unwrap()).to_owned());
        let _ = said(&mut peer);

        for character in ['4', '2'] {
            client.handle(&fonts, Event::KeyPressed(Key::Char(character)), &mut clipboard);
        }
        let heard = said(&mut peer);
        let typed: Vec<&Vec<String>> =
            heard.iter().filter(|fields| fields[3] == display::ACTION_TYPE_TEXT).collect();
        assert_eq!(typed.len(), 2, "one event per keystroke: {heard:?}");
        assert_eq!(typed[1][4], "42", "the value is not what the cell ended up with");
        assert_eq!(typed[1][5], "B2", "the event does not name the cell");

        // An arrow leaves the cell and chooses the next one, which is the
        // same event a press on it produces.
        client.handle(&fonts, Event::KeyPressed(Key::Down), &mut clipboard);
        let heard = said(&mut peer);
        let moved = heard
            .iter()
            .find(|fields| fields[3] == display::ACTION_SELECT)
            .unwrap_or_else(|| panic!("the cursor did not move: {heard:?}"));
        assert_eq!(moved[5], "B3");

        // Shift and an arrow reach further instead of moving.
        client.handle(&fonts, Event::KeyPressed(Key::ShiftRight), &mut clipboard);
        let heard = said(&mut peer);
        let run = heard
            .iter()
            .find(|fields| fields[3] == display::ACTION_SELECT_RANGE)
            .unwrap_or_else(|| panic!("shift and right reached nothing: {heard:?}"));
        assert_eq!(run[5], "B2", "the run does not start at the chosen cell");
        assert_eq!(run[4], "C2", "the run does not reach one cell across");
    }

    /// A cell far down the sheet is reached by scrolling to it, which the
    /// compositor can do itself: it holds the sheet, so there is nothing to
    /// ask the application for.
    #[test]
    fn a_cell_below_the_screen_is_scrolled_to_rather_than_asked_for() {
        let (fonts, mut client, mut peer) =
            framed(r#"<window pad="none"><text>x</text></window>"#, Rect::new(0, 0, 400, 300));
        grid(&mut client, &fonts, "A1");
        let index = client.node_by_id("sheet").unwrap();
        let _ = said(&mut peer);

        assert!(client.reveal_cell(&fonts, index, (0, 499)), "A500 could not be reached");
        let grid = client.layout.grids.iter().find(|grid| grid.node == index).unwrap();
        let ((_, first), (_, last)) = grid.visible();
        assert!(
            (first..=last).contains(&499),
            "A500 is still off screen: rows {first} to {last}"
        );
        // Nothing was asked of the application. There is nothing to ask.
        assert!(said(&mut peer).is_empty(), "the application was asked for a row");
    }

    /// Opening a cell to *edit* it is a different thing from choosing it and
    /// typing over it, and both ways in have to exist: a second press, or
    /// Enter on the cell already chosen. Either starts from what is in the
    /// cell, because a human who wanted to replace it would have just typed.
    #[test]
    fn a_second_press_or_enter_opens_a_cell_on_what_is_in_it() {
        let (fonts, mut client, mut peer) =
            framed(r#"<window pad="none"><text>x</text></window>"#, Rect::new(0, 0, 400, 300));
        grid(&mut client, &fonts, "A1");
        let mut clipboard = Clipboard::default();
        let index = client.node_by_id("sheet").unwrap();
        let element = client.doc.as_ref().unwrap().key(index).to_owned();
        let a1 = client.cell_rect(index, (0, 0)).unwrap();
        let _ = said(&mut peer);

        // One press chooses it and opens nothing.
        client.handle(
            &fonts,
            Event::ButtonPressed { button: Button::Left, x: a1.x + 2, y: a1.y + 2 },
            &mut clipboard,
        );
        assert!(!client.editing.contains_key(&cell_key(&element, (0, 0))));

        // A second in the same place opens it, on what was published.
        client.handle(
            &fonts,
            Event::ButtonPressed { button: Button::Left, x: a1.x + 2, y: a1.y + 2 },
            &mut clipboard,
        );
        let state = client
            .editing
            .get(&cell_key(&element, (0, 0)))
            .unwrap_or_else(|| panic!("a double press did not open the cell"));
        assert_eq!(state.value, "Region", "the cell did not open on what is in it");
        assert_eq!(state.caret, 6, "the caret is not at the end of it");

        // Enter does the same from the keyboard, and Enter again commits and
        // moves down, which is the other half of what Enter means here.
        client.focus = Some(element.clone());
        client.editing.remove(&cell_key(&element, (0, 0)));
        let _ = said(&mut peer);
        client.handle(&fonts, Event::KeyPressed(Key::Enter), &mut clipboard);
        assert_eq!(
            client.editing.get(&cell_key(&element, (0, 0))).map(|state| state.value.as_str()),
            Some("Region"),
            "Enter did not open the cell"
        );
        assert!(said(&mut peer).is_empty(), "opening a cell told the application something");

        client.handle(&fonts, Event::KeyPressed(Key::Enter), &mut clipboard);
        let heard = said(&mut peer);
        assert!(
            heard.iter().any(|fields| fields[3] == display::ACTION_SELECT && fields[5] == "A2"),
            "Enter while editing did not commit and move down: {heard:?}"
        );
    }

    /// A grid's bar moves the compositor's own offset. It used to be the one
    /// bar that did not: it was drawn against a sheet the compositor did not
    /// hold, so dragging it asked the application for a different window. The
    /// compositor holds the sheet now, and the flag that marked the exception
    /// kept sending an event nothing answered, so the bar could be seen and
    /// not dragged.
    #[test]
    fn dragging_a_grids_bar_moves_it_rather_than_asking() {
        let (fonts, mut client, mut peer) =
            framed(r#"<window pad="none"><text>x</text></window>"#, Rect::new(0, 0, 400, 300));
        grid(&mut client, &fonts, "A1");
        let mut clipboard = Clipboard::default();
        let index = client.node_by_id("sheet").unwrap();
        let key = client.doc.as_ref().unwrap().key(index).to_owned();
        let _ = said(&mut peer);

        let scroller = client
            .layout
            .scrollers
            .iter()
            .find(|scroller| scroller.node == index && !scroller.horizontal)
            .expect("a bar down the side");
        assert!(scroller.permanent, "a grid's bar fades");
        let rect = client.layout.rect_of(index);
        let (track, thumb) = ui::scrollbar_geometry(rect, scroller).expect("a thumb");

        client.handle(
            &fonts,
            Event::ButtonPressed {
                button: Button::Left,
                x: thumb.x + thumb.w / 2,
                y: thumb.y + thumb.h / 2,
            },
            &mut clipboard,
        );
        assert!(client.scroll_dragging(), "the press did not take hold of the thumb");

        client.drag_scroll(&fonts, thumb.x, track.y + track.h);
        assert!(
            client.scroll.get(&key).copied().unwrap_or(0) > 0,
            "the drag moved nothing"
        );
        assert!(said(&mut peer).is_empty(), "the drag asked the application for rows");
    }

    fn editing(value: &str, anchor: Option<usize>, caret: usize) -> Editing {
        Editing { value: value.to_owned(), caret, anchor, outstanding: Vec::new() }
    }

    /// A selection is a run between an anchor and the caret, in either
    /// direction, and dragging backwards selects the same characters as
    /// dragging forwards.
    #[test]
    fn a_selection_reads_the_same_in_both_directions() {
        let forwards = editing("hello world", Some(6), 11);
        let backwards = editing("hello world", Some(11), 6);
        assert_eq!(forwards.selected_text().as_deref(), Some("world"));
        assert_eq!(backwards.selected_text().as_deref(), Some("world"));
        // A caret that has not moved from its anchor is not a selection.
        assert_eq!(editing("hello", Some(2), 2).selected_text(), None);
        assert_eq!(editing("hello", None, 2).selected_text(), None);
    }

    /// Cutting and pasting are edits to the compositor's own copy. What an
    /// application hears is the value the control ended up with, which is
    /// exactly what it would have heard had the human typed it.
    #[test]
    fn cut_removes_the_run_and_leaves_the_caret_where_it_was() {
        let mut state = editing("hello world", Some(5), 11);
        assert_eq!(state.selected_text().as_deref(), Some(" world"));
        assert!(state.delete_selection());
        assert_eq!(state.value, "hello");
        assert_eq!(state.caret, 5);
        assert_eq!(state.anchor, None, "a deleted selection is not still selected");
        // Nothing selected, nothing to delete, and the value is untouched.
        assert!(!state.delete_selection());
        assert_eq!(state.value, "hello");
    }

    /// Selections are counted in characters, not bytes, or a cut across
    /// anything but ASCII would slice a character in half and panic.
    #[test]
    fn a_selection_counts_characters_rather_than_bytes() {
        let mut state = editing("héllo wörld", Some(0), 6);
        assert_eq!(state.selected_text().as_deref(), Some("héllo "));
        assert!(state.delete_selection());
        assert_eq!(state.value, "wörld");
    }

    /// A tab does not flash when it is pressed, and the rule is not in
    /// either click path.
    ///
    /// The navigation bar's tabs go through `click_nav` and an application's
    /// through `Client::act`, so anything that made them look alike by
    /// matching conditions in both would be one edit away from not. The
    /// press is still recorded here, uniformly, for whatever is pressed;
    /// what a tab does with it is the paint's business, and the paint is
    /// shared.
    #[test]
    fn a_tab_records_a_press_and_renders_none() {
        let source = r#"<window pad="none"><tabs>
             <tab id="one" label="One" selected="true" description="The first"/>
             <tab id="two" label="Two" description="The second"/>
           </tabs></window>"#;
        let (fonts, mut client, _peer) = framed(source, Rect::new(0, 0, 400, 100));
        let two = client.node_by_id("two").unwrap();

        client.act(&fonts, two, "select", "").expect("chosen");
        // Recorded, exactly as it is for a button: the click path knows
        // nothing about tabs.
        assert!(client.press.is_some(), "the press was special-cased away in the click path");
        // And not rendered, which is the half that keeps the two bars alike.
        assert_eq!(
            client.focus_state().pressed,
            Some(two),
            "the paint is not being handed the press to ignore"
        );
    }

    /// A tab carried along its strip tells the application where the hand put
    /// it, and nothing else.
    ///
    /// This is the half that cannot be shared with the navigation bar. The
    /// arithmetic is: `ui::tab_slot` answers for both. What differs is who
    /// owns the order. The bar's is the compositor's, so it rearranges
    /// itself; an application's is the application's, so it is told and
    /// answers with a new tree, exactly as it answers a table's `scroll`.
    #[test]
    fn a_movable_tab_reports_where_it_was_carried() {
        let source = r#"<window pad="none"><tabs>
             <tab id="one" label="One" selected="true" movable="true" description="The first"/>
             <tab id="two" label="Two" movable="true" description="The second"/>
             <tab id="three" label="Three" movable="true" description="The third"/>
           </tabs></window>"#;
        let (fonts, mut client, mut peer) = framed(source, Rect::new(0, 0, 400, 100));
        let mut clipboard = Clipboard::default();

        // Press the last one, which chooses it and takes hold of it.
        let three = client.layout.rect_of(client.node_by_id("three").unwrap());
        client.handle(
            &fonts,
            Event::ButtonPressed { button: Button::Left, x: three.x + 2, y: three.y + 2 },
            &mut clipboard,
        );
        assert!(client.moving_tab(), "the press did not take hold of the tab");
        let _ = said(&mut peer);

        // Carry it past the first tab's middle.
        let one = client.layout.rect_of(client.node_by_id("one").unwrap());
        assert!(client.drag_tabs(&fonts, one.x + 2), "the drag reported nothing");
        let moved = said(&mut peer);
        let event = moved
            .iter()
            .find(|fields| fields[3] == display::ACTION_MOVE)
            .unwrap_or_else(|| panic!("no move was reported: {moved:?}"));
        assert_eq!(event[2], "three", "the wrong tab was carried");
        assert_eq!(event[4], "0", "the slot is not where the pointer is");

        // The same slot again says nothing: one event per slot crossed.
        assert!(!client.drag_tabs(&fonts, one.x + 3), "the same slot was sent twice");

        // A tab nobody marked movable is not one, and an agent asking is
        // told so rather than quietly reordering somebody's sheets.
        let source = r#"<window pad="none"><tabs>
             <tab id="one" label="One" selected="true" description="The first"/>
             <tab id="two" label="Two" description="The second"/>
           </tabs></window>"#;
        let (fonts, mut client, _peer) = framed(source, Rect::new(0, 0, 400, 100));
        let two = client.node_by_id("two").unwrap();
        assert_eq!(
            client.act(&fonts, two, "move", "0"),
            Err(agent::REASON_UNSUPPORTED),
            "a tab that cannot be moved was moved"
        );
    }

    /// Words on a page can be dragged out and copied, and the application is
    /// never told any of it happened.
    ///
    /// This is the half of the clipboard that was missing. A field and an
    /// editor could always be selected in; an agent's answer, which is the
    /// thing in this system most worth copying, is a `text` element, and
    /// `Layout::hit` only ever answered with controls, so a press on one
    /// landed on nothing at all.
    #[test]
    fn words_on_a_page_can_be_dragged_out_and_copied() {
        let source = r#"<window pad="none"><vstack>
             <text>the agent said something worth keeping</text>
           </vstack></window>"#;
        let (fonts, mut client, mut peer) = framed(source, Rect::new(0, 0, 400, 200));
        let mut clipboard = Clipboard::default();
        let words = client.doc.as_ref().unwrap().tree.nodes.len() - 1;
        let rect = client.layout.rect_of(words);

        client.handle(
            &fonts,
            Event::ButtonPressed { button: Button::Left, x: rect.x + 1, y: rect.y + 2 },
            &mut clipboard,
        );
        assert!(client.selecting(), "a press on words did not take hold of them");
        assert!(client.drag_select(&fonts, rect.x + 60, rect.y + 2), "the drag reported nothing");
        client.end_select();

        client.handle(&fonts, Event::KeyPressed(Key::Copy), &mut clipboard);
        let copied = clipboard.text().unwrap_or_default().to_owned();
        assert!(!copied.is_empty(), "copy put nothing on the clipboard");
        assert!(
            "the agent said something worth keeping".starts_with(&copied),
            "copied something that is not the start of the words: {copied:?}"
        );

        // None of it reached the application. A selection is the compositor's
        // and static text has no events at all.
        assert!(said(&mut peer).is_empty(), "the application was told about a selection");

        // A press that never moves selects nothing, the way it does in a
        // control: the anchor is set on every press because a press is where
        // a drag would start. Somewhere else on the line, so it is a fresh
        // press rather than the second half of a double click.
        client.handle(
            &fonts,
            Event::ButtonPressed { button: Button::Left, x: rect.x + 120, y: rect.y + 2 },
            &mut clipboard,
        );
        client.end_select();
        clipboard.set_text("what was there before");
        client.handle(&fonts, Event::KeyPressed(Key::Copy), &mut clipboard);
        assert_eq!(
            clipboard.text(),
            Some("what was there before"),
            "a press that selected nothing overwrote the clipboard"
        );

        // Two presses in the same place take the word under them, on a page
        // exactly as in a text box.
        for _ in 0..2 {
            client.handle(
                &fonts,
                Event::ButtonPressed { button: Button::Left, x: rect.x + 20, y: rect.y + 2 },
                &mut clipboard,
            );
            client.end_select();
        }
        client.handle(&fonts, Event::KeyPressed(Key::Copy), &mut clipboard);
        let word = clipboard.text().unwrap_or_default().to_owned();
        assert_ne!(word, "what was there before", "a double click selected nothing");
        assert!(
            "the agent said something worth keeping".contains(&word),
            "a double click took {word:?}, which is not part of the words"
        );
    }

    /// A strip is a cap, the tabs, a foot and a hairline.
    ///
    /// Read out of the navigation bar a pixel at a time: down a column
    /// between two of its tabs there are three rows of `raised`, then
    /// twenty-six of `background`, then two of `raised` and one of `border`.
    /// What matters is the middle: its tabs stand on the same colour as the
    /// desk behind the bar, not on a raised band. Painting the band and
    /// putting tabs on it is what made them look like buttons lying on a
    /// bar, and it took three attempts because it cannot be seen by looking,
    /// only by reading the pixels.
    #[test]
    fn a_tab_strip_leaves_band_showing_above_and_below() {
        let source = r#"<window pad="none"><tabs>
             <tab id="one" label="One" selected="true" description="The first"/>
             <tab id="two" label="Two" description="The second"/>
           </tabs></window>"#;
        let (_fonts, client, _peer) = framed(source, Rect::new(0, 0, 400, 100));
        let doc = client.doc.as_ref().unwrap();
        let strip = (0..doc.tree.nodes.len())
            .find(|&at| doc.tree.node(at).tag == Tag::Tabs)
            .expect("the strip");
        let tab = client.node_by_id("one").unwrap();

        let band = client.layout.rect_of(strip);
        let sits = client.layout.rect_of(tab);
        assert!(sits.y > band.y, "no cap above the tab");
        assert!(
            sits.y + sits.h < band.y + band.h - 1,
            "no foot below the tab for the hairline to sit under"
        );
    }

    /// A press that never moved is a caret, not a selection.
    ///
    /// The anchor is set on every press, because a press is where a drag
    /// would start. Leaving it set once the button came up made the second
    /// character typed after a click delete the first: the caret had walked
    /// away from an anchor nobody had dragged, and the run between them
    /// looked exactly like a selection to replace. Found by typing into the
    /// pane and reading back "bc" for "abc"; no test would have caught it,
    /// because it needs a click and a keystroke in that order.
    #[test]
    fn a_click_that_did_not_drag_leaves_no_selection() {
        let mut state = editing("", None, 0);
        // The press: anchor where the caret landed.
        state.anchor = Some(state.caret);
        // A character goes in and the caret moves on.
        state.value.push('a');
        state.caret = 1;
        // With the anchor still standing, the next character would replace
        // everything since the click.
        assert_eq!(state.selected_text().as_deref(), Some("a"), "the phantom run this guards against");
        // Which is why the release drops it, and why an edit does too.
        state.anchor = None;
        assert_eq!(state.selected_text(), None);
    }

    /// The clipboard holds a kind, so the day something copies a picture the
    /// readers that only understand words say so rather than pasting base64
    /// into a spreadsheet.
    #[test]
    fn the_clipboard_says_what_kind_of_thing_it_holds() {
        let mut clipboard = Clipboard::default();
        assert_eq!(clipboard.kind(), awproto::agent::CLIPBOARD_NONE);
        assert_eq!(clipboard.text(), None);

        clipboard.set_text("copied");
        assert_eq!(clipboard.kind(), awproto::agent::CLIPBOARD_TEXT);
        assert_eq!(clipboard.text(), Some("copied"));
        assert_eq!(clipboard.content(), "copied");

        // Copying nothing empties it rather than holding an empty string,
        // so "is there anything to paste" has one answer.
        clipboard.set_text("");
        assert_eq!(clipboard.kind(), awproto::agent::CLIPBOARD_NONE);
    }
}
