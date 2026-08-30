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
use crate::images::Images;
use crate::input::{Button, Event, Key};
use crate::paint::font::Fonts;
use crate::paint::{Canvas, Rect};
use crate::ui::{self, Focus, Frame, Layout};

/// How much unsent event traffic a client may accumulate before it is treated as
/// gone.
///
/// The compositor must never block on a write, so events queue when a client is
/// slow. A client that has stopped reading entirely is not slow, it is broken,
/// and holding its backlog forever would let one wedged application consume
/// memory in the one process that owns the screen.
const MAX_BACKLOG: usize = 256 * 1024;

/// How many rows one notch of the wheel moves a table.
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
struct Editing {
    /// What is on screen, which may be ahead of what the application holds.
    value: String,
    /// Caret position, in characters.
    caret: usize,
    /// Where a selection began, when there is one. The selected run is
    /// everything between this and the caret, in either order, and it is the
    /// compositor's alone: an application is told what the value became, not
    /// which part of it was highlighted on the way.
    anchor: Option<usize>,
    /// Values sent to the application and not yet seen echoed back in a tree.
    outstanding: Vec<String>,
}

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
    /// A column edge being dragged: which column, where the pointer took
    /// hold, and how wide it was when it did.
    column_drag: Option<(String, i32, i32)>,
    /// A run of cells being dragged out: the corner it started from, by key
    /// and by id, and the cell the run currently reaches. The id is kept
    /// because it is what the application is told; the key because it is what
    /// survives a re-render.
    cell_drag: Option<(String, String)>,
    cell_extent: Option<String>,
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
            column_drag: None,
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
            .map(|doc| awml::agent_view(&doc.tree, &self.name, self.desk))
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
        self.editing.retain(|key, _| next.has_key(key));

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
        ui::paint_subtree(canvas, fonts, images, &doc.tree, &self.layout, index, &self.focus_state());
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
        Focus { node, caret, anchor, pressed, caret_visible: self.caret_on, scrollbar }
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
        // table's bars are always visible, so they are always grabbable.
        let lit = self.focus_state().scrollbar;

        for scroller in self.layout.scrollers.iter().rev() {
            if !scroller.asks && !scroller.horizontal && lit != Some(scroller.node) {
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

        // A table's bar is drawn against the whole sheet, so dragging it is a
        // question rather than a move: what comes back is a tree holding a
        // different window.
        if scroller.asks {
            let step = scroller.step.max(1);
            let row = offset / step;
            if row == scroller.offset / step {
                return false;
            }
            let id = doc.tree.node(node).id().unwrap_or_default().to_owned();
            if id.is_empty() {
                return false;
            }
            self.note = format!("asked {id} for row {row}");
            self.emit(&id, display::ACTION_SCROLL, &row.to_string());
            return true;
        }

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
        ui::paint(canvas, fonts, images, &doc.tree, &self.layout, &self.focus_state());
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

    /// Whether the pointer is dragging a selection out of a text control.
    pub fn selecting(&self) -> bool {
        self.selecting.is_some()
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

    /// Carry a run of cells out under the pointer.
    ///
    /// One event per cell the run reaches, in the same spirit as one event
    /// per keystroke: what the application hears is what the hand did, as it
    /// does it, and the highlight it paints in answer is its own.
    pub fn drag_cells(&mut self, x: i32, y: i32) -> bool {
        let Some((anchor_key, anchor_id)) = self.cell_drag.clone() else { return false };
        let Some(doc) = &self.doc else { return false };
        let Some(index) = self.layout.hit(&doc.tree, x, y) else { return false };
        if doc.tree.node(index).tag != Tag::Cell {
            return false;
        }
        // Both corners must be in the same grid: a run that started in one
        // table and ended in another is not a run of anything.
        let same = doc
            .index_of(&anchor_key)
            .and_then(|anchor| Some((doc.tree.table_of(anchor)?, doc.tree.table_of(index)?)))
            .is_some_and(|(from, to)| from == to);
        if !same {
            return false;
        }

        let reached = doc.tree.node(index).id().unwrap_or_default().to_owned();
        if reached.is_empty() || self.cell_extent.as_deref() == Some(reached.as_str()) {
            return false;
        }
        self.cell_extent = Some(reached.clone());
        self.note = format!("{anchor_id} through {reached}");
        self.emit(&anchor_id, display::ACTION_SELECT_RANGE, &reached);
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
        let Some(key) = self.selecting.clone() else { return false };
        let Some(doc) = &self.doc else { return false };
        let Some(index) = doc.index_of(&key) else { return false };
        let caret = self.caret_at(fonts, index, x, y);
        let Some(state) = self.editing.get_mut(&key) else { return false };
        if state.caret == caret {
            return false;
        }
        state.caret = caret;
        true
    }

    pub fn end_select(&mut self) {
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
        let value = match node.attr("value") {
            Some(value) => value.to_owned(),
            None if node.tag == Tag::Cell => node.text.clone(),
            None => String::new(),
        };
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

    /// Take hold of a column's trailing edge, if the point is on one.
    fn grab_column(&mut self, x: i32, y: i32) -> bool {
        let Some(doc) = &self.doc else { return false };
        for table in (0..doc.tree.nodes.len()).filter(|&i| doc.tree.node(i).tag == Tag::Table) {
            for column in doc.tree.columns(table) {
                let rect = self.layout.rect_of(column);
                if rect.w == 0 || !self.layout.is_visible(column) {
                    continue;
                }
                let edge = Rect::new(rect.x + rect.w - ui::column_grip(), rect.y, ui::column_grip() * 2, rect.h);
                if edge.contains(x, y) {
                    self.column_drag = Some((doc.key(column).to_owned(), x, rect.w));
                    return true;
                }
            }
        }
        false
    }

    /// Carry a column's edge with the pointer.
    pub fn drag_column(&mut self, fonts: &Fonts, x: i32) -> bool {
        let Some((key, from, width)) = self.column_drag.clone() else { return false };
        let next = (width + (x - from)).max(ui::sc(24));
        if self.columns.get(&key) == Some(&next) {
            return false;
        }
        self.columns.insert(key, next);
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

        let Some(index) = hit else {
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
        let editable = node.flag("editable");
        let value = match node.attr("value") {
            Some(value) => value.to_owned(),
            None if tag == Tag::Cell => node.text.clone(),
            None => String::new(),
        };

        if node.disabled() {
            // Not reported to the application at all. A disabled control has no
            // actions, and inventing one for it is exactly what the derived
            // action list exists to prevent.
            self.note = format!("{id} is disabled");
            return true;
        }

        let rect = self.layout.rect_of(index);
        let already = self.focus.as_deref() == Some(key.as_str());
        self.focus = Some(key.clone());
        // A press with the ordinary button ends whatever the other one had
        // standing, so a menu bar's items hang from the menu and not from
        // wherever the last right-press happened to be.
        self.context_at = None;

        // A click on a text control places the caret and reports nothing: where
        // the caret is inside a value is not the application's business.
        if tag.is_text() {
            // A cell is chosen before it is edited. The first press selects
            // it, which is the event a person pressing it produces; a second,
            // once it already carries the ring, puts the caret in. That is
            // what a double click means elsewhere, spread over two presses,
            // because there is no double click in the event vocabulary and
            // adding one would be a second way to produce an event.
            if tag == Tag::Cell && !(already && editable) {
                // The press is also where a run of cells would start. Whether
                // it becomes one is decided by whether the pointer moves,
                // exactly as it is for a run of text.
                let id = doc.tree.node(index).id().unwrap_or_default().to_owned();
                self.cell_drag = Some((key.clone(), id));
                self.cell_extent = None;
                let _ = self.act(fonts, index, "select", "");
                return true;
            }

            let caret = self.caret_at(fonts, index, x, y);
            let state = self.editing.entry(key.clone()).or_insert(Editing {
                value,
                caret: 0,
                anchor: None,
                outstanding: Vec::new(),
            });
            state.caret = caret.min(state.value.chars().count());
            // The press is where a selection starts; the drag is what makes
            // it one. A press that never moves leaves anchor and caret in the
            // same place, which is no selection at all.
            state.anchor = Some(state.caret);
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
            // A header, a row's gutter, a tab: pressing any of them means
            // choosing it, so the event is the one a person produced.
            Tag::Option | Tag::Row | Tag::Column | Tag::Tab => "select",
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
            "select-range" => {
                let Some(other) = self.node_by_id(value) else {
                    return Err(agent::REASON_NO_SUCH_NODE);
                };
                let Some(doc) = &self.doc else { return Err(agent::REASON_NO_SUCH_NODE) };
                if doc.tree.node(other).tag != Tag::Cell {
                    return Err(agent::REASON_UNSUPPORTED);
                }
                if doc.tree.table_of(index) != doc.tree.table_of(other) {
                    return Err(agent::REASON_UNSUPPORTED);
                }
                self.emit(&id, display::ACTION_SELECT_RANGE, value);
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

    /// One numeric attribute, for the compositor's own arithmetic about a
    /// table's window.
    pub fn number(&self, index: usize, name: &str) -> Option<i32> {
        self.doc
            .as_ref()?
            .tree
            .node(index)
            .attr(name)
            .and_then(|value| value.parse().ok())
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
            if matches!(doc.tree.node(node).tag, Tag::Scroll | Tag::Table) {
                containers.push(node);
            }
            at = doc.tree.node(node).parent;
        }
        containers.reverse();

        for container in containers {
            self.reveal_within(fonts, index, container);
        }

        if self.layout.is_visible(index) {
            return true;
        }

        // Still out of sight, which for a cell means the application placed
        // it below the body of its own table. Ask for a window that starts
        // there, so the next attempt finds it on screen.
        if let Some(doc) = &self.doc
            && let Some(table) = doc.tree.table_of(index)
            && let Some((down, _)) = doc.tree.cell_position(index)
        {
            let first = doc
                .tree
                .node(table)
                .attr("first-row")
                .and_then(|value| value.parse::<i32>().ok())
                .unwrap_or(0);
            self.ask_for_row(table, first + down as i32);
        }
        false
    }

    /// Move one container so that a node inside it comes into view.
    fn reveal_within(&mut self, fonts: &Fonts, index: usize, container: usize) {
        let Some(doc) = &self.doc else { return };
        let key = doc.key(container).to_owned();
        let horizontal = doc.tree.node(container).tag == Tag::Table;

        let rect = self.layout.rect_of(index);
        let view = self.layout.rect_of(container);
        let held = self
            .layout
            .scrollers
            .iter()
            .find(|scroller| scroller.node == container && scroller.horizontal == horizontal);
        let Some(scroller) = held else { return };
        let (offset, furthest) = (scroller.offset, scroller.furthest());

        // A table is moved across, over the columns it holds; everything else
        // is moved down, over content laid out in full.
        let shift = if horizontal {
            // The gutter is frozen, so the room a cell has to be inside of
            // starts after it rather than at the table's edge.
            let left = view.x + ui::gutter_width(fonts, &doc.tree, container);
            if rect.x < left {
                rect.x - left
            } else if rect.x + rect.w > view.x + view.w {
                rect.x + rect.w - (view.x + view.w)
            } else {
                return;
            }
        } else if rect.y < view.y {
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

    /// Ask an application to bring a row of a table into its window.
    /// Ask an application to bring a row of a table into its window.
    ///
    /// The compositor cannot scroll a table itself: the rows on screen are
    /// the ones the application chose to describe, and no offset here can
    /// conjure the ones it did not send. So an agent that wants row five
    /// hundred asks for it, and the application answers with a tree. This is
    /// the same promise kept at a larger scale: the agent
    /// says what should be true and never how to bring it about.
    pub fn ask_for_row(&mut self, index: usize, row: i32) -> bool {
        let Some(doc) = &self.doc else { return false };
        if doc.tree.node(index).tag != Tag::Table {
            return false;
        }
        let id = doc.tree.node(index).id().unwrap_or_default().to_owned();
        if id.is_empty() {
            return false;
        }
        self.note = format!("asked {id} for row {row}");
        self.emit(&id, display::ACTION_SCROLL, &row.max(0).to_string());
        true
    }

    /// Queue one frame for this client.
    pub fn send(&mut self, fields: &[&str]) {
        self.pending.extend_from_slice(&awproto::encode(fields));
        self.flush();
    }

    fn wheel(&mut self, fonts: &Fonts, delta: i32, x: i32, y: i32) -> bool {
        let Some(doc) = &self.doc else { return false };

        // A table does not scroll: it asks. Its rows are the window the
        // application chose, so moving is a question for the application and
        // the answer is a new tree. Checked before the ordinary containers so
        // that a table inside a scrolling page takes the notch itself.
        // Read out of the iterator before anything is emitted: the borrow it
        // holds on the layout cannot outlive the write to the socket.
        let asking = self
            .layout
            .scrollers_at(x, y)
            .find(|scroller| scroller.asks)
            .map(|scroller| {
                let step = scroller.step.max(1);
                (
                    scroller.offset / step,
                    scroller.content / step,
                    (scroller.viewport / step).max(1),
                    doc.tree.node(scroller.node).id().unwrap_or_default().to_owned(),
                )
            });
        if let Some((first, total, visible, id)) = asking {
            let next = (first - delta * WHEEL_ROWS).clamp(0, (total - visible).max(0));
            if next == first || id.is_empty() {
                return false;
            }
            self.note = format!("asked {id} for row {next}");
            self.emit(&id, display::ACTION_SCROLL, &next.to_string());
            return true;
        }

        // Innermost first, and the first that can still move takes the
        // notch: a container at its end, or one that never overflowed, hands
        // it outward rather than swallowing it. Positive delta is a push away
        // from the human, which moves the content down and the viewport up.
        let Some((key, next, furthest)) = self
            .layout
            .scrollers_at(x, y)
            .filter(|scroller| !scroller.horizontal)
            .find_map(|scroller| {
                let furthest = scroller.furthest();
                let next = (scroller.offset - delta * ui::wheel_step()).clamp(0, furthest);
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

        let Some(doc) = &self.doc else { return false };
        let Some(focus_key) = self.focus.clone() else { return false };
        let Some(index) = doc.index_of(&focus_key) else { return false };
        // A control behind a dialog keeps its focus ring but not the keyboard.
        if doc.tree.blocked(index) {
            return false;
        }

        let node = doc.tree.node(index);
        let tag = node.tag;
        if !tag.is_text() || node.disabled() {
            return false;
        }
        // A cell takes text only when the application says it does; a field
        // and an editor always do.
        let editable = tag != Tag::Cell || node.flag("editable");
        let id = node.id().unwrap_or_default().to_owned();
        let seed = match node.attr("value") {
            Some(value) => value.to_owned(),
            None if tag == Tag::Cell => node.text.clone(),
            None => String::new(),
        };
        // A compositor-internal attribute for chat-shaped editors: Enter
        // submits and Shift+Enter breaks the line, the convention every
        // messenger keeps. Without it an editor keeps the catalogue's rule,
        // Enter breaks the line, because a general editor has no submit.
        let enter_submits = node.flag("enter-submits");
        let typing = self.editing.contains_key(&focus_key);

        // Moving about the grid comes first, because in a cell the arrows
        // mean the grid until something is being typed into it, and then
        // they mean the caret. Up and down always leave: a spreadsheet that
        // trapped the selection in a half-typed cell would be unusable.
        if tag == Tag::Cell {
            match key {
                Key::Up => return self.move_cell(fonts, 0, -1),
                Key::Down | Key::Enter => return self.move_cell(fonts, 0, 1),
                Key::Left if !typing => return self.move_cell(fonts, -1, 0),
                Key::Right if !typing => return self.move_cell(fonts, 1, 0),
                Key::Escape if typing => {
                    // Give the cell back the value the application last sent.
                    // Every keystroke was already reported, so undoing has to
                    // be reported too, as the value it ends on.
                    self.editing.remove(&focus_key);
                    self.emit(&id, display::ACTION_TYPE_TEXT, &seed);
                    if let Some(doc) = &mut self.doc {
                        doc.tree.nodes[index].set("value", &seed);
                    }
                    self.relayout(fonts);
                    self.note = format!("cancelled the edit in {id}");
                    return true;
                }
                _ => {}
            }
            if !editable {
                return false;
            }
        }

        // A cell that is not being typed into yet starts empty, so the first
        // character replaces what was there rather than appending to it,
        // which is what every spreadsheet does and what the human expects
        // when they select a cell and start typing.
        let fresh = tag == Tag::Cell && !typing;
        let state = self.editing.entry(focus_key.clone()).or_insert(Editing {
            value: if fresh { String::new() } else { seed.clone() },
            caret: 0,
            anchor: None,
            outstanding: Vec::new(),
        });
        state.caret = state.caret.min(state.value.chars().count());

        // The clipboard. None of this is in the display protocol: what the
        // application hears is the value the control ended up with, exactly
        // as if the human had typed it out.
        match key {
            Key::SelectAll => {
                state.anchor = Some(0);
                state.caret = state.value.chars().count();
                self.note = format!("selected all of {id}");
                return true;
            }
            Key::Copy | Key::Cut => {
                let Some(selected) = state.selected_text() else { return false };
                clipboard.set_text(&selected);
                if key == Key::Copy {
                    self.note = format!("copied {} character(s)", selected.chars().count());
                    return true;
                }
                state.delete_selection();
            }
            Key::Paste => {
                let Some(words) = clipboard.text().map(str::to_owned) else {
                    self.note = "nothing on the clipboard this can take".into();
                    return true;
                };
                state.delete_selection();
                let at = byte_at(&state.value, state.caret);
                state.value.insert_str(at, &words);
                state.caret += words.chars().count();
                state.anchor = None;
            }
            _ => {}
        }

        let mut changed = matches!(key, Key::Paste | Key::Cut);
        if !changed {
            match key {
                Key::Char(character) => {
                    state.delete_selection();
                    state.value.insert(byte_at(&state.value, state.caret), character);
                    state.caret += 1;
                    changed = true;
                }
                Key::Backspace => {
                    if state.delete_selection() {
                        changed = true;
                    } else if state.caret == 0 {
                        return false;
                    } else {
                        state.caret -= 1;
                        state.value.remove(byte_at(&state.value, state.caret));
                        changed = true;
                    }
                }
                // Moving the caret drops the selection, the way it does
                // everywhere: the arrow keys are how you stop having one.
                Key::Left => {
                    state.anchor = None;
                    state.caret = state.caret.saturating_sub(1);
                }
                Key::Right => {
                    state.anchor = None;
                    state.caret = (state.caret + 1).min(state.value.chars().count());
                }
                Key::Up | Key::Down if tag == Tag::Editor => {
                    state.anchor = None;
                    state.caret = move_line(&state.value, state.caret, key == Key::Down);
                }
                // Enter confirms a field and inserts a newline in an editor. That is
                // the whole reason the two elements are separate: an editor offers no
                // `submit` because Enter already means something else in it.
                // An editor marked `enter-submits` swaps the two: Enter
                // confirms and Shift+Enter breaks the line. In a field the
                // shift is simply not load-bearing: there is no line to
                // break, so both confirm.
                Key::Enter | Key::ShiftEnter => {
                    let newline = match tag {
                        Tag::Editor if enter_submits => key == Key::ShiftEnter,
                        Tag::Editor => true,
                        _ => false,
                    };
                    if newline {
                        state.delete_selection();
                        state.value.insert(byte_at(&state.value, state.caret), '\n');
                        state.caret += 1;
                        changed = true;
                    } else {
                        self.note = format!("submitted {id}");
                        let value = state.value.clone();
                        self.emit(&id, display::ACTION_SUBMIT, &value);
                        return true;
                    }
                }
                _ => return false,
            }
        }

        if !changed {
            self.note = format!("caret in {id} at {}", state.caret);
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

    /// Move the grid's cursor, committing whatever was being typed.
    ///
    /// The destination is told to the application as a `select`, which is the
    /// same event a press on it would have produced. That matters more than
    /// it looks: the application, not the compositor, decides which rows it
    /// has sent, so it can only keep the cursor in view if it is told the
    /// cursor moved.
    fn move_cell(&mut self, fonts: &Fonts, across: i32, down: i32) -> bool {
        let Some(doc) = &self.doc else { return false };
        let Some(key) = self.focus.clone() else { return false };
        let Some(index) = doc.index_of(&key) else { return false };
        let Some((row_at, column_at)) = doc.tree.cell_position(index) else { return false };
        let Some(table) = doc.tree.table_of(index) else { return false };

        let rows = doc.tree.rows(table);
        if rows.is_empty() {
            return false;
        }
        let row = (row_at as i32 + down).clamp(0, rows.len() as i32 - 1) as usize;
        let cells = doc.tree.cells(rows[row]);
        if cells.is_empty() {
            return false;
        }
        let column = (column_at as i32 + across).clamp(0, cells.len() as i32 - 1) as usize;
        let target = cells[column];
        let target_key = doc.key(target).to_owned();
        let id = doc.tree.node(index).id().unwrap_or_default().to_owned();

        // Whatever was typed is already with the application, keystroke by
        // keystroke. Leaving the cell is what says it is finished.
        if self.editing.remove(&key).is_some() {
            self.emit(&id, display::ACTION_SUBMIT, "");
        }
        if target == index {
            return true;
        }

        self.focus = Some(target_key);
        let _ = self.act(fonts, target, "select", "");
        true
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
        let event = display::Event {
            version: self.version(),
            target: target.to_owned(),
            action: action.to_owned(),
            value: value.to_owned(),
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

impl Editing {
    /// The selected run, low to high, or `None` when the caret is a point.
    fn selected(&self) -> Option<(usize, usize)> {
        let anchor = self.anchor?;
        let (from, to) = (anchor.min(self.caret), anchor.max(self.caret));
        (from != to).then_some((from, to))
    }

    fn selected_text(&self) -> Option<String> {
        let (from, to) = self.selected()?;
        Some(self.value.chars().skip(from).take(to - from).collect())
    }

    /// Remove the selected run, leaving the caret where it was. Returns
    /// whether there was anything to remove, which is what tells a backspace
    /// whether it has already done its work.
    fn delete_selection(&mut self) -> bool {
        let Some((from, to)) = self.selected() else { return false };
        let start = byte_at(&self.value, from);
        let end = byte_at(&self.value, to);
        self.value.replace_range(start..end, "");
        self.caret = from;
        self.anchor = None;
        true
    }
}

/// The byte offset of a character position.
fn byte_at(text: &str, caret: usize) -> usize {
    text.char_indices()
        .nth(caret)
        .map_or(text.len(), |(at, _)| at)
}

/// Move a caret one line up or down, keeping the column where it can.
fn move_line(value: &str, caret: usize, down: bool) -> usize {
    let (row, column) = ui::caret_position(value, caret, Tag::Editor);
    let target = if down { row + 1 } else { row.checked_sub(1).unwrap_or(row) };

    let mut at = 0;
    for (index, line) in value.split('\n').enumerate() {
        let length = line.chars().count();
        if index == target {
            return at + column.min(length);
        }
        at += length + 1;
    }
    caret
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

    /// A cell off the right-hand edge of a table. The table is not a scroll
    /// container and the old reveal walked straight past it, so this is the
    /// second of the two gaps that made `not-visible` look unavoidable.
    #[test]
    fn a_cell_off_the_side_of_a_table_is_revealed_across() {
        let mut source = String::from(
            "<window pad=\"none\"><table id=\"sheet\" grow=\"true\" rows=\"100\"              first-row=\"0\" description=\"The grid\">",
        );
        for name in ["A", "B", "C", "D", "E", "F", "G", "H"] {
            source.push_str(&format!("<column label=\"{name}\" chars=\"12\"/>"));
        }
        source.push_str("<row label=\"1\">");
        for name in ["A", "B", "C", "D", "E", "F", "G", "H"] {
            source.push_str(&format!("<cell id=\"{name}1\" value=\"x\"/>"));
        }
        source.push_str("</row></table></window>");

        // Narrow on purpose: the last columns are off the side.
        let (fonts, mut client, _peer) = framed(&source, Rect::new(0, 0, 300, 200));
        let index = client.node_by_id("H1").expect("the last cell exists");
        assert!(!client.layout.is_visible(index), "eight columns fitted in 300 pixels");
        assert!(client.reveal(&fonts, index), "reveal did not report success");
        assert!(client.layout.is_visible(index), "the table was not moved across");
    }

    fn window(first: i32, rows: &[i32]) -> String {
        let mut out = format!(
            "<window pad=\"none\"><table id=\"sheet\" grow=\"true\" rows=\"1000\"              first-row=\"{first}\" description=\"The grid\">             <column label=\"A\" chars=\"8\"/><column label=\"B\" chars=\"8\"/>"
        );
        for &row in rows {
            out.push_str(&format!(
                "<row label=\"{row}\"><cell id=\"A{row}\" value=\"v\" editable=\"true\"/>                 <cell id=\"B{row}\" value=\"w\"/></row>"
            ));
        }
        out.push_str("</table></window>");
        out
    }

    /// An agent typing into an editable cell.
    ///
    /// `Tag::actions` cannot know whether a cell is editable, so it answers
    /// for one that is not, and this went through it: every keystroke an
    /// agent sent to a perfectly ordinary cell came back
    /// `unsupported-action`. It never showed up by hand because typing goes
    /// through the keyboard path, which consults no action list at all.
    #[test]
    fn an_agent_may_type_into_an_editable_cell() {
        let (fonts, mut client, _peer) = framed(&window(0, &[1, 2, 3]), Rect::new(0, 0, 400, 300));
        let editable = client.node_by_id("A1").expect("the cell exists");
        let plain = client.node_by_id("B1").expect("the cell exists");

        assert_eq!(client.act(&fonts, editable, "type-text", "42"), Ok(()));
        assert_eq!(client.act(&fonts, editable, "submit", ""), Ok(()));
        assert_eq!(client.act(&fonts, editable, "select", ""), Ok(()));

        // A cell the application did not mark editable still takes neither,
        // which is the other half of the same question being asked properly.
        assert_eq!(
            client.act(&fonts, plain, "type-text", "42"),
            Err(awproto::agent::REASON_UNSUPPORTED)
        );
    }

    /// The cell cursor survives the window moving past it and back.
    ///
    /// A row outside a table's window has not been removed; it is not being
    /// described. Forgetting focus for anything absent from the incoming tree
    /// meant scrolling away from a selected cell lost it for good.
    #[test]
    fn the_cell_cursor_survives_a_window_that_moved_past_it() {
        let (fonts, mut client, _peer) = framed(&window(0, &[1, 2, 3]), Rect::new(0, 0, 400, 300));
        let cell = client.node_by_id("A2").expect("the cell exists");
        client.act(&fonts, cell, "select", "").expect("selected");
        assert!(client.focus_state().node.is_some(), "nothing was selected to begin with");

        // The application describes a window far below. The cell is nowhere
        // in this tree.
        client.apply(&fonts, &window(500, &[501, 502, 503]), 2);
        assert!(client.node_by_id("A2").is_none(), "the row is somehow still here");
        assert!(client.focus_state().node.is_none(), "a ring on a row nobody sent");

        // And back. The cursor is where it was left.
        client.apply(&fonts, &window(0, &[1, 2, 3]), 3);
        let back = client.node_by_id("A2").expect("the row is described again");
        assert_eq!(client.focus_state().node, Some(back), "the cell cursor did not come back");
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

    /// Dragging across cells is one gesture, so it is one kind of event, sent
    /// as the run grows. The application paints the highlight; the compositor
    /// only says what the hand did.
    #[test]
    fn dragging_across_cells_reports_a_run() {
        let (fonts, mut client, mut peer) =
            framed(&window(0, &[1, 2, 3]), Rect::new(0, 0, 400, 300));
        let mut clipboard = Clipboard::default();

        // Press on A1, which selects it and arms the drag.
        let a1 = client.layout.rect_of(client.node_by_id("A1").unwrap());
        client.handle(
            &fonts,
            Event::ButtonPressed { button: Button::Left, x: a1.x + 2, y: a1.y + 2 },
            &mut clipboard,
        );
        assert!(client.selecting_cells(), "the press did not arm a run");
        let opening = said(&mut peer);
        assert!(
            opening.iter().any(|fields| fields[3] == display::ACTION_SELECT && fields[2] == "A1"),
            "the press did not choose the cell it landed on: {opening:?}"
        );

        // Carry it to B3.
        let b3 = client.layout.rect_of(client.node_by_id("B3").unwrap());
        client.drag_cells(b3.x + 2, b3.y + 2);
        let dragged = said(&mut peer);
        let run = dragged
            .iter()
            .find(|fields| fields[3] == display::ACTION_SELECT_RANGE)
            .unwrap_or_else(|| panic!("no run was reported: {dragged:?}"));
        assert_eq!(run[2], "A1", "the run does not start where the press did");
        assert_eq!(run[4], "B3", "the run does not reach where the pointer is");

        // The same cell again says nothing: one event per cell reached, not
        // one per pixel of pointer motion.
        client.drag_cells(b3.x + 3, b3.y + 3);
        assert!(said(&mut peer).is_empty(), "a run was reported twice for one cell");
    }

    /// An agent naming both corners, and the two ways that can be wrong.
    #[test]
    fn an_agent_names_both_corners_of_a_run() {
        let (fonts, mut client, mut peer) =
            framed(&window(0, &[1, 2, 3]), Rect::new(0, 0, 400, 300));
        let a1 = client.node_by_id("A1").unwrap();

        assert_eq!(client.act(&fonts, a1, "select-range", "B3"), Ok(()));
        let said = said(&mut peer);
        let run = said.iter().find(|f| f[3] == display::ACTION_SELECT_RANGE).expect("a run");
        assert_eq!((run[2].as_str(), run[4].as_str()), ("A1", "B3"));

        // A corner that does not exist is not a corner.
        assert_eq!(
            client.act(&fonts, a1, "select-range", "Z99"),
            Err(awproto::agent::REASON_NO_SUCH_NODE)
        );
    }

    /// The other button says where it landed and on what, and nothing else.
    /// What it means is the application's to decide.
    #[test]
    fn the_other_button_reports_what_was_under_it() {
        let (fonts, mut client, mut peer) =
            framed(&window(0, &[1, 2, 3]), Rect::new(0, 0, 400, 300));
        let mut clipboard = Clipboard::default();
        let b2 = client.layout.rect_of(client.node_by_id("B2").unwrap());

        client.handle(
            &fonts,
            Event::ButtonPressed { button: Button::Right, x: b2.x + 2, y: b2.y + 2 },
            &mut clipboard,
        );
        let said = said(&mut peer);
        let context = said
            .iter()
            .find(|fields| fields[3] == display::ACTION_CONTEXT)
            .unwrap_or_else(|| panic!("nothing was reported: {said:?}"));
        assert_eq!(context[2], "B2", "reported on the wrong node");
        // And it is remembered, because a menu opened in answer hangs from it.
        assert_eq!(client.context_at, Some((b2.x + 2, b2.y + 2)));
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
