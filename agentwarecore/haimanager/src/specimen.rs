//! A stand-in client, until real applications exist.
//!
//! It holds one AWML document and re-renders it whenever input arrives, which
//! is exactly what an application connection will do in the next milestone. The
//! document is written the way an app would write it: affordances only, no
//! colours, no coordinates.
//!
//! Interaction is wired through hit testing rather than special-cased, so
//! clicking a button here goes through the same path an agent's intent will:
//! resolve a node, check it is enabled, act on it.

use crate::awml::{self, Tag, Tree};
use crate::input::{Button, Event, Key};
use crate::paint::{Canvas, Rect};
use crate::ui::{self, Layout};

/// What an application would send. Everything here is an affordance: an id, a
/// description of what it does, and its state. Nothing describes appearance.
const DOCUMENT: &str = r#"
<window title="Messages">
  <vstack gap="lg">
    <text role="heading">Compose</text>

    <group label="Recipient">
      <field id="to" placeholder="name@example.com"
             description="Address the message will be sent to" value=""/>
      <checkbox id="copy-self" label="Send me a copy"
                description="Also deliver this message to your own inbox"/>
    </group>

    <group label="Message">
      <editor id="body" placeholder="Write something"
              description="Body text of the message being composed" value=""/>
    </group>

    <hstack gap="sm">
      <button id="send" label="Send" emphasis="primary" disabled
              description="Sends the composed message to its recipient"/>
      <button id="discard" label="Discard" emphasis="danger"
              description="Throws away the draft without sending it"/>
      <text grow="true" emphasis="muted">click a control to focus it</text>
      <button id="help" label="Help"
              description="Explains what this window is for"/>
    </hstack>

    <divider/>

    <list id="drafts" label="Saved drafts">
      <item id="draft-1" label="Notes from Tuesday"
            description="Open the draft named Notes from Tuesday"/>
      <item id="draft-2" label="Re: budget" selected="true"
            description="Open the draft named Re: budget"/>
      <item id="draft-3" label="Holiday plans" disabled
            description="Open the draft named Holiday plans"/>
    </list>
  </vstack>
</window>
"#;

pub struct Client {
    tree: Tree,
    layout: Layout,
    focus: Option<usize>,
    /// The most recent thing that happened, drawn so the interaction path can
    /// be checked without a debugger.
    last: String,
}

impl Client {
    pub fn new(area: Rect) -> Result<Self, awml::Error> {
        let tree = awml::parse(DOCUMENT)?;
        let layout = ui::layout(&tree, area);
        Ok(Self { tree, layout, focus: None, last: "nothing yet".into() })
    }

    /// How many nodes the document parsed to, for the boot log.
    pub fn node_count(&self) -> usize {
        self.tree.nodes.len()
    }

    /// The reduced view an agent would be given, for the boot log.
    pub fn agent_view(&self) -> String {
        awml::agent_view(&self.tree)
    }

    /// Where a node named by an agent sits on screen.
    ///
    /// Unused until intents arrive, but this is the whole of what resolving one
    /// requires: a name to a node, a node to a rectangle.
    pub fn locate(&self, id: &str) -> Option<Rect> {
        self.tree.by_id(id).map(|index| self.layout.rect_of(index))
    }

    pub fn handle(&mut self, event: Event) {
        match event {
            Event::ButtonPressed { button: Button::Left, x, y } => self.click(x, y),

            Event::KeyPressed(key) => {
                let Some(index) = self.focus else { return };
                let tag = self.tree.node(index).tag;
                if !matches!(tag, Tag::Field | Tag::Editor) {
                    return;
                }

                // Editing writes back into the tree, which is what an
                // application would do before resending it. The haimanager owns
                // this only because there is no application yet.
                let mut value = self.tree.node(index).attr("value").unwrap_or("").to_owned();
                match key {
                    Key::Char(c) => value.push(c),
                    Key::Backspace => {
                        value.pop();
                    }
                    _ => return,
                }
                self.set(index, "value", &value);
                self.last = format!("typed into {}", self.id_of(index));
            }

            _ => {}
        }
    }

    /// Resolve a point to a node and act on it.
    ///
    /// This is the same sequence an agent's intent will follow: find the node,
    /// refuse if it is disabled, then act. Doing it this way now means the
    /// agent path is not a second implementation that can disagree.
    fn click(&mut self, x: i32, y: i32) {
        let Some(index) = self.layout.hit(&self.tree, x, y) else {
            self.focus = None;
            self.last = format!("clicked nothing at {x}, {y}");
            return;
        };

        if self.tree.node(index).disabled() {
            self.last = format!("{} is disabled", self.id_of(index));
            return;
        }

        self.focus = Some(index);
        let tag = self.tree.node(index).tag;

        match tag {
            Tag::Checkbox => {
                let now = !self.tree.node(index).flag("checked");
                self.set(index, "checked", if now { "true" } else { "false" });
                self.last = format!("{} {}", self.id_of(index), if now { "checked" } else { "unchecked" });
            }

            Tag::Item => {
                // Single selection: clearing the siblings is the list's
                // behaviour, not the item's.
                let siblings: Vec<usize> = (0..self.tree.nodes.len())
                    .filter(|&i| self.tree.node(i).tag == Tag::Item)
                    .collect();
                for sibling in siblings {
                    self.set(sibling, "selected", "false");
                }
                self.set(index, "selected", "true");
                self.last = format!("selected {}", self.id_of(index));
            }

            _ => self.last = format!("clicked {}", self.id_of(index)),
        }

        // A control changing state can change how much room it needs, so the
        // tree is laid out again rather than patched.
        self.relayout();
    }

    fn id_of(&self, index: usize) -> String {
        self.tree.node(index).id().unwrap_or("?").to_owned()
    }

    fn set(&mut self, index: usize, key: &str, value: &str) {
        let node = &mut self.tree.nodes[index];
        match node.attrs.iter_mut().find(|(name, _)| name == key) {
            Some((_, existing)) => *existing = value.to_owned(),
            None => node.attrs.push((key.to_owned(), value.to_owned())),
        }
    }

    fn relayout(&mut self) {
        let area = self.layout.rect_of(Tree::ROOT);
        self.layout = ui::layout(&self.tree, area);
    }

    pub fn draw(&self, canvas: &mut Canvas) {
        canvas.clear(ui::BACKGROUND);
        ui::paint(canvas, &self.tree, &self.layout, self.focus);
        self.draw_status(canvas);
    }

    /// A readout of the agent-facing view of whatever has focus.
    ///
    /// Drawn here because it is the thing worth checking: that layout produced
    /// a usable rectangle, that hit testing found the node under the pointer,
    /// and that the actions offered match the element's state.
    fn draw_status(&self, canvas: &mut Canvas) {
        let height = 96;
        let panel = Rect::new(0, canvas.height() - height, canvas.width(), height);
        canvas.fill_rect(panel, ui::SURFACE);
        canvas.stroke_rect(panel, 1, ui::BORDER);

        let x = 16;
        let mut y = panel.y + 12;

        canvas.draw_text(&format!("last: {}", self.last), x, y, 2, ui::TEXT);
        y += 22;

        let Some(index) = self.focus else {
            canvas.draw_text("focus: none", x, y, 2, ui::MUTED);
            return;
        };

        let node = self.tree.node(index);
        let rect = self.layout.rect_of(index);
        canvas.draw_text(
            &format!(
                "focus: <{}> id={} at {},{} {}x{}",
                node.tag.name(),
                node.id().unwrap_or("?"),
                rect.x,
                rect.y,
                rect.w,
                rect.h
            ),
            x,
            y,
            2,
            ui::TEXT,
        );
        y += 22;

        let actions = node.tag.actions(node.disabled()).join(" ");
        canvas.draw_text(
            &format!("actions: {}", if actions.is_empty() { "none" } else { &actions }),
            x,
            y,
            2,
            ui::ACCENT,
        );
    }
}
