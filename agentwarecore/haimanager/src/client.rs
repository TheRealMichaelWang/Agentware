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
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, RawFd};
use std::os::unix::net::UnixStream;

use awproto::Decoder;
use awproto::agent;
use awproto::display::{self, MAX_TREE};

use crate::awml::{self, Tag};
use crate::document::Document;
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
    /// Values sent to the application and not yet seen echoed back in a tree.
    outstanding: Vec<String>,
}

pub struct Client {
    pub kind: Kind,
    /// The workspace this connection belongs to, as the supervisor stated it.
    pub desk: u32,
    pub name: String,
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
}

/// What happened when a client was read from.
pub struct Progress {
    pub gone: bool,
    pub dirty: bool,
    /// Lines worth putting in the kernel log.
    pub log: Vec<String>,
    /// Queries and intents from an agent connection, for the screen to answer.
    /// An agent does not render, so nothing it sends is understood here.
    pub requests: Vec<Vec<String>>,
}

impl Client {
    pub fn adopt(kind: Kind, desk: u32, name: String, stream: UnixStream) -> io::Result<Self> {
        stream.set_nonblocking(true)?;
        Ok(Client {
            kind,
            desk,
            name,
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
        let mut progress =
            Progress { gone: false, dirty: false, log: Vec::new(), requests: Vec::new() };
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
                        let (dirty, line) = self.apply(fonts, &source, version);
                        progress.dirty |= dirty;
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
        if let Some(key) = &self.focus
            && !next.has_key(key)
        {
            self.focus = None;
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
        self.layout = ui::layout(fonts, doc, &self.frame, &mut self.scroll);
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
    pub fn draw_region(&self, canvas: &mut Canvas, fonts: &Fonts, name: &str) {
        let (Some(doc), Some(index)) = (&self.doc, self.region(name)) else { return };
        ui::paint_subtree(canvas, fonts, &doc.tree, &self.layout, index, &self.focus_state());
    }

    /// Where the compositor believes focus and the caret are, in this tree.
    fn focus_state(&self) -> Focus {
        let Some(doc) = &self.doc else { return Focus::default() };
        let Some(key) = &self.focus else { return Focus::default() };
        let node = doc.index_of(key);
        let caret = self.editing.get(key).map_or(0, |state| state.caret);
        Focus { node, caret }
    }

    pub fn draw(&self, canvas: &mut Canvas, fonts: &Fonts) {
        let Some(doc) = &self.doc else { return };
        ui::paint(canvas, fonts, &doc.tree, &self.layout, &self.focus_state());
    }

    /// A one-line description of the focused node as an agent would see it.
    pub fn focus_summary(&self) -> String {
        let Some(doc) = &self.doc else { return "focus: none".into() };
        let Some(index) = self.focus_state().node else { return "focus: none".into() };

        let node = doc.tree.node(index);
        let rect = self.layout.rect_of(index);
        let actions = node.tag.actions(node.disabled()).join(" ");
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
    pub fn handle(&mut self, fonts: &Fonts, event: Event) -> bool {
        match event {
            Event::ButtonPressed { button: Button::Left, x, y } => self.click(fonts, x, y),
            Event::Scrolled { delta, x, y } => self.wheel(fonts, delta, x, y),
            Event::KeyPressed(key) => self.key(fonts, key),
            _ => false,
        }
    }

    /// Resolve a point to a node and act on it.
    ///
    /// This is the same sequence an agent's intent will follow: find the node,
    /// check it is visible and enabled, then synthesize exactly one event. The
    /// agent path being the same path is what stops it becoming a second
    /// implementation that can disagree.
    fn click(&mut self, fonts: &Fonts, x: i32, y: i32) -> bool {
        let Some(doc) = &self.doc else { return false };
        let Some(index) = self.layout.hit(&doc.tree, x, y) else {
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
        let value = node.attr("value").unwrap_or("").to_owned();
        let rect = self.layout.rect_of(index);

        if node.disabled() {
            // Not reported to the application at all. A disabled control has no
            // actions, and inventing one for it is exactly what the derived
            // action list exists to prevent.
            self.note = format!("{id} is disabled");
            return true;
        }

        self.focus = Some(key.clone());

        // A click on a text control places the caret and reports nothing: where
        // the caret is inside a value is not the application's business.
        if matches!(tag, Tag::Field | Tag::Editor) {
            let style = ui::style_at(&doc.tree, index);
            let caret = ui::caret_from_x(fonts, &value, &style, ui::text_origin(rect), x);
            let state = self.editing.entry(key).or_insert(Editing {
                value,
                caret: 0,
                outstanding: Vec::new(),
            });
            state.caret = caret.min(state.value.chars().count());
            self.note = format!("caret in {id} at {}", state.caret);
            return true;
        }

        // Everything else goes through the same door an agent's intent does. A
        // click on a checkbox is a toggle rather than a click: `check` and
        // `uncheck` exist so an intent can be unconditional, but a human
        // pressing the box means invert it.
        let action = if tag == Tag::Checkbox {
            display::ACTION_TOGGLE
        } else {
            display::ACTION_CLICK
        };
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
        if !tag.actions(disabled).contains(&action) {
            return Err(agent::REASON_UNSUPPORTED);
        }

        let id = node.id().unwrap_or_default().to_owned();
        let key = doc.key(index).to_owned();
        let checked = node.flag("checked");
        let selected = node.flag("selected");
        let current = node.attr("value").unwrap_or("").to_owned();

        // Focus is the compositor's. An application is never told about it,
        // which is why it is not in the event vocabulary at all.
        self.focus = Some(key.clone());

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

            "type-text" | "clear" => {
                let next = if action == "clear" { String::new() } else { value.to_owned() };
                let state = self.editing.entry(key).or_insert(Editing {
                    value: current,
                    caret: 0,
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

    pub fn is_visible(&self, index: usize) -> bool {
        self.layout.is_visible(index)
    }

    pub fn is_disabled(&self, index: usize) -> bool {
        self.doc
            .as_ref()
            .is_some_and(|doc| doc.tree.node(index).disabled())
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

    /// Scroll whatever has to move so a node is inside its container.
    ///
    /// This is the one action handled entirely by the compositor. The agent says
    /// which node it wants visible and the compositor works out which container
    /// to scroll and by how much, which is why `scroll` needs no id and an agent
    /// never addresses one. It is the intent principle applied to a mechanism:
    /// express what should be true, not the steps to make it so.
    pub fn reveal(&mut self, fonts: &Fonts, index: usize) -> bool {
        let Some(doc) = &self.doc else { return false };

        // Innermost scroll container above the node. Only one is moved: a nested
        // scroll is rare and moving the outer one first would undo the inner.
        let mut at = doc.tree.node(index).parent;
        let mut container = None;
        while let Some(node) = at {
            if doc.tree.node(node).tag == Tag::Scroll {
                container = Some(node);
                break;
            }
            at = doc.tree.node(node).parent;
        }

        let Some(container) = container else { return false };
        let rect = self.layout.rect_of(index);
        let view = self.layout.rect_of(container);

        let shift = if rect.y < view.y {
            rect.y - view.y
        } else if rect.y + rect.h > view.y + view.h {
            rect.y + rect.h - (view.y + view.h)
        } else {
            return false;
        };

        let key = doc.key(container).to_owned();
        let was = self.scroll.get(&key).copied().unwrap_or(0);
        self.scroll.insert(key, was + shift);
        self.relayout(fonts);
        self.note = format!("scrolled to reveal {}", self.label());
        true
    }

    /// Queue one frame for this client.
    pub fn send(&mut self, fields: &[&str]) {
        self.pending.extend_from_slice(&awproto::encode(fields));
        self.flush();
    }

    fn wheel(&mut self, fonts: &Fonts, delta: i32, x: i32, y: i32) -> bool {
        let Some(doc) = &self.doc else { return false };
        let Some(scroller) = self.layout.scroller_at(x, y) else { return false };

        let key = doc.key(scroller.node).to_owned();
        let furthest = (scroller.content - scroller.viewport).max(0);
        // Positive delta is a push away from the human, which moves the content
        // down and the viewport up.
        let next = (scroller.offset - delta * ui::WHEEL_STEP).clamp(0, furthest);
        if next == scroller.offset {
            return false;
        }

        self.scroll.insert(key, next);
        self.relayout(fonts);
        self.note = format!("scrolled to {next} of {furthest}");
        true
    }

    fn key(&mut self, fonts: &Fonts, key: Key) -> bool {
        if key == Key::Tab {
            return self.focus_next();
        }

        let Some(doc) = &self.doc else { return false };
        let Some(focus_key) = self.focus.clone() else { return false };
        let Some(index) = doc.index_of(&focus_key) else { return false };

        let node = doc.tree.node(index);
        let tag = node.tag;
        if !matches!(tag, Tag::Field | Tag::Editor) || node.disabled() {
            return false;
        }
        let id = node.id().unwrap_or_default().to_owned();
        let seed = node.attr("value").unwrap_or("").to_owned();

        let state = self.editing.entry(focus_key).or_insert(Editing {
            value: seed,
            caret: 0,
            outstanding: Vec::new(),
        });
        state.caret = state.caret.min(state.value.chars().count());

        let mut changed = false;
        match key {
            Key::Char(character) => {
                state.value.insert(byte_at(&state.value, state.caret), character);
                state.caret += 1;
                changed = true;
            }
            Key::Backspace => {
                if state.caret == 0 {
                    return false;
                }
                state.caret -= 1;
                state.value.remove(byte_at(&state.value, state.caret));
                changed = true;
            }
            Key::Left => state.caret = state.caret.saturating_sub(1),
            Key::Right => state.caret = (state.caret + 1).min(state.value.chars().count()),
            Key::Up | Key::Down if tag == Tag::Editor => {
                state.caret = move_line(&state.value, state.caret, key == Key::Down);
            }
            // Enter confirms a field and inserts a newline in an editor. That is
            // the whole reason the two elements are separate: an editor offers no
            // `submit` because Enter already means something else in it.
            Key::Enter => {
                if tag == Tag::Editor {
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

        if !changed {
            self.note = format!("caret in {id} at {}", state.caret);
            return true;
        }

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

    /// Move focus to the next enabled control, wrapping.
    fn focus_next(&mut self) -> bool {
        let Some(doc) = &self.doc else { return false };

        let order: Vec<usize> = (0..doc.tree.nodes.len())
            .filter(|&index| {
                let node = doc.tree.node(index);
                node.tag.is_control() && !node.disabled() && self.layout.is_visible(index)
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
