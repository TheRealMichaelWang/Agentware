//! Turning a markup tree into pixels, and a point back into a node.
//!
//! Layout runs in two passes. `measure` asks every node how big it wants to be,
//! bottom up. `place` hands out the space that actually exists, top down. Two
//! passes are needed because a stack cannot know its own height until its
//! children have been measured, and a child cannot know its width until the
//! stack has decided how to divide the room it was given.
//!
//! The output is a `Vec<Rect>` indexed the same way as the arena, which is what
//! makes hit testing a scan and makes "where is node `send` on screen" a single
//! lookup. That question is not incidental: it is how the agent's fake cursor
//! knows where to travel, and how an intent naming a node becomes an event at a
//! coordinate.
//!
//! Alongside it is a `Vec<Rect>` of **clips**: the region each node is actually
//! visible within, after every scroll container and framed box above it has had
//! its say. A node whose rectangle lies outside its clip has been scrolled out
//! of sight, and neither a human's click nor an agent's intent may reach it.
//! Without that, hit testing would happily resolve a point to a control the
//! human cannot see, and the promise that an agent can only do what a human
//! could have done would quietly stop holding.
//!
//! ## Styling
//!
//! Applications choose type and colour; the haimanager supplies the defaults,
//! the metrics and the layout. `font`, `size`, `weight`, `italic` and `color`
//! inherit, so a window sets them once and a single element can override.
//!
//! None of it reaches an agent. That is safe rather than merely tidy: an agent
//! is never asked to infer meaning from appearance, because every control
//! carries a description and its actions are derived from its type and state.
//! Styling can therefore be as expressive as an application likes without any
//! of it becoming load-bearing.

use std::collections::HashMap;

use crate::awml::{Node, Tag, Tree};
use crate::document::Document;
use crate::paint::font::{Family, Fonts, Style, Weight};
use crate::paint::{Canvas, Color, Rect, rgb};

// The palette an application names colours out of. An app may also give a hex
// value; these exist so the common cases stay consistent between applications
// and follow the system theme if it ever changes.
pub const BACKGROUND: Color = rgb(0x0e, 0x10, 0x16);
pub const SURFACE: Color = rgb(0x1a, 0x1e, 0x28);
pub const RAISED: Color = rgb(0x25, 0x2b, 0x39);
/// One step above RAISED, for a control under the pointer or being pressed.
pub const PRESSED: Color = rgb(0x32, 0x3a, 0x4c);
pub const BORDER: Color = rgb(0x2e, 0x35, 0x45);
pub const TEXT: Color = rgb(0xe8, 0xeb, 0xf2);
pub const MUTED: Color = rgb(0x94, 0x9d, 0xb2);
pub const ACCENT: Color = rgb(0x4f, 0x9c, 0xf5);
pub const ACCENT_DEEP: Color = rgb(0x2f, 0x77, 0xcc);
pub const DANGER: Color = rgb(0xe0, 0x5a, 0x5a);
pub const DANGER_DEEP: Color = rgb(0xb8, 0x42, 0x42);
pub const OK: Color = rgb(0x5a, 0xc8, 0x8a);
pub const SELECTED: Color = rgb(0x2b, 0x3f, 0x5e);

/// Corner radii. Everything drawn gets one, because a hard corner at these
/// sizes is what makes a surface look like a drawn rectangle rather than a
/// panel, and one square element among rounded ones looks like a bug.
///
/// Kept small. A large radius on a small control is the single loudest thing an
/// interface can do, and it reads as a toy rather than as a tool.
pub const RADIUS_WINDOW: i32 = 8;
pub const RADIUS_SURFACE: i32 = 6;
pub const RADIUS_CONTROL: i32 = 5;
pub const RADIUS_SMALL: i32 = 3;

// Density. These are the numbers that decide whether the result looks like an
// interface or like a toy, and every one of them was too large.
//
// The reference points are the desktops people actually use: a 13px system font,
// a control about 28px tall, and single-digit padding almost everywhere. Chunky
// controls do not read as friendly at this scale, they read as unfinished.
const BODY_SIZE: f32 = 13.0;
const PADDING: i32 = 10;
/// Space above and below the text inside a control.
const CONTROL_PAD: i32 = 6;
/// Space either side of the text inside a button.
const BUTTON_PAD: i32 = 12;
const CHECKBOX_SIZE: i32 = 14;
const DIVIDER: i32 = 1;
/// An editor is this many lines tall.
const EDITOR_LINES: i32 = 4;
/// Width of the indicator drawn beside overflowing scroll content.
const SCROLLBAR: i32 = 5;
/// How far one notch of the wheel moves a scroll container.
pub const WHEEL_STEP: i32 = 48;

fn gap_of(node: &Node) -> i32 {
    match node.attr("gap") {
        Some("none") => 0,
        Some("sm") => 4,
        Some("lg") => 14,
        _ => 8,
    }
}

/// A node's label as drawn: the `label` attribute, or its text content.
fn label_of(node: &Node) -> &str {
    node.attr("label").unwrap_or(&node.text)
}

/// The value shown in a text control, masked if it is a password.
///
/// Masking here rather than in the application means an app cannot leak a
/// password by forgetting to, and the agent's view is built from the same
/// string, so it cannot see one either.
fn value_of(node: &Node) -> String {
    let value = node.attr("value").unwrap_or("");
    if node.attr("kind") == Some("password") {
        return "*".repeat(value.chars().count());
    }
    value.to_owned()
}

/// Resolve how a node's text should be drawn.
///
/// The element's `role` picks a starting point, then any inherited attribute
/// overrides it.
pub fn style_at(tree: &Tree, index: usize) -> Style {
    let node = tree.node(index);
    let role = node.attr("role");

    let mut style = Style {
        size: match role {
            Some("heading") => BODY_SIZE * 1.45,
            Some("subheading") => BODY_SIZE * 1.15,
            Some("caption") => BODY_SIZE * 0.85,
            _ => BODY_SIZE,
        },
        weight: if matches!(role, Some("heading") | Some("subheading")) {
            Weight::Bold
        } else {
            Weight::Normal
        },
        ..Style::default()
    };

    if let Some(value) = tree.inherited(index, "font")
        && let Some(family) = Family::parse(value)
    {
        style.family = family;
    }
    if let Some(value) = tree.inherited(index, "weight")
        && let Some(weight) = Weight::parse(value)
    {
        style.weight = weight;
    }
    if let Some(value) = tree.inherited(index, "size")
        && let Some(size) = parse_size(value)
    {
        style.size = size;
    }
    if matches!(tree.inherited(index, "italic"), Some(value) if value != "false") {
        style.italic = true;
    }

    style
}

/// A size is either a number of pixels or one of a few names.
fn parse_size(value: &str) -> Option<f32> {
    let scale = match value {
        "xs" => Some(0.75),
        "sm" => Some(0.85),
        "md" => Some(1.0),
        "lg" => Some(1.3),
        "xl" => Some(1.75),
        _ => None,
    };
    if let Some(scale) = scale {
        return Some(BODY_SIZE * scale);
    }

    // Clamped, because a client asking for 4000px would have every glyph
    // rasterize a coverage map the size of the screen.
    value.parse::<f32>().ok().map(|size| size.clamp(6.0, 200.0))
}

/// Resolve a node's text colour, falling back to `default`.
pub fn color_at(tree: &Tree, index: usize, default: Color) -> Color {
    let Some(value) = tree.inherited(index, "color") else {
        return default;
    };

    match value {
        "text" => TEXT,
        "muted" => MUTED,
        "accent" => ACCENT,
        "danger" => DANGER,
        "ok" => OK,
        hex => parse_hex(hex).unwrap_or(default),
    }
}

fn parse_hex(value: &str) -> Option<Color> {
    let digits = value.strip_prefix('#')?;
    match digits.len() {
        6 => u32::from_str_radix(digits, 16).ok(),
        // #rgb, expanded the way every other system expands it.
        3 => {
            let short = u32::from_str_radix(digits, 16).ok()?;
            let expand = |nibble: u32| (nibble << 4) | nibble;
            Some(
                (expand((short >> 8) & 0xf) << 16)
                    | (expand((short >> 4) & 0xf) << 8)
                    | expand(short & 0xf),
            )
        }
        _ => None,
    }
}

/// Where a document's top level is put on screen.
pub enum Frame {
    /// One rectangle for the whole document. Applications get this: an app
    /// describes a window, and where the window goes is not its business.
    Whole(Rect),
    /// The root's children are placed by the `region` each one declares.
    ///
    /// Only a desk connection is laid out this way. That is what makes `region`
    /// a property of the connection rather than an attribute anything may write:
    /// an application can put the word in its markup and it will mean nothing,
    /// because nothing ever reads it on that path.
    Regions(Regions),
}

/// The four parts a workspace is divided into.
///
/// Three belong to the agentdesk and one belongs to application processes. The
/// division is the compositor's, so an agentdesk cannot give itself the whole
/// screen and an application cannot escape the part it was given.
#[derive(Clone, Copy)]
pub struct Regions {
    /// Wallpaper, behind everything. Spans the whole workspace.
    pub background: Rect,
    /// Open apps and the launcher, along the bottom.
    pub taskbar: Rect,
    /// Chat transcript, input box, collapse toggle, down the side.
    pub pane: Rect,
    /// Application windows. Not addressable by the agentdesk.
    pub apps: Rect,
}

impl Regions {
    /// Carve a workspace up. Order matters: the taskbar takes the full width
    /// along the bottom, then the pane takes the side of what is left.
    pub fn carve(area: Rect, taskbar: i32, pane: i32) -> Regions {
        let body = Rect::new(area.x, area.y, area.w, area.h - taskbar);
        Regions {
            background: area,
            taskbar: Rect::new(area.x, area.y + area.h - taskbar, area.w, taskbar),
            pane: Rect::new(body.x + body.w - pane, body.y, pane, body.h),
            apps: Rect::new(body.x, body.y, body.w - pane, body.h),
        }
    }

    /// The rectangle a top-level node claims, or `None` if it claims nothing it
    /// is allowed to have.
    ///
    /// `apps` is deliberately absent. It belongs to application processes, and a
    /// desk asking for it is asking to draw over its own applications.
    fn claim(&self, name: Option<&str>) -> Option<Rect> {
        match name? {
            "background" => Some(self.background),
            "taskbar" => Some(self.taskbar),
            "pane" => Some(self.pane),
            _ => None,
        }
    }
}

/// A scroll container that was placed, and how much of it did not fit.
pub struct Scroller {
    pub node: usize,
    /// Total height the children wanted.
    pub content: i32,
    /// How much of them is visible.
    pub viewport: i32,
    pub offset: i32,
}

/// Where the compositor believes the caret is, and in which node.
///
/// Both halves are ephemeral state the application never sees. Keeping them out
/// of the tree is what stops a full-tree resend from moving the human's cursor
/// on every keystroke.
#[derive(Default, Clone, Copy)]
pub struct Focus {
    pub node: Option<usize>,
    /// Position in characters within the focused control's value.
    pub caret: usize,
    /// A control being pressed right now.
    ///
    /// Kept apart from focus because they answer different questions: focus is
    /// where the next keystroke goes and lasts until it moves, a press is what
    /// is happening at this instant and lasts a fraction of a second. An agent's
    /// click has to produce the second one or its actions are invisible except
    /// for their consequences.
    pub pressed: Option<usize>,
}

pub struct Layout {
    /// One rectangle per node, indexed as the arena is.
    pub rects: Vec<Rect>,
    /// The region each node is visible within, after clipping by every
    /// container above it.
    pub clips: Vec<Rect>,
    pub scrollers: Vec<Scroller>,
}

impl Layout {
    /// A layout for a client that has not sent anything yet.
    pub fn empty() -> Layout {
        Layout { rects: Vec::new(), clips: Vec::new(), scrollers: Vec::new() }
    }

    /// The innermost node at a point that an agent or a human could act on.
    ///
    /// Walked in reverse so later siblings, which paint on top, win. Layout
    /// elements are skipped: clicking the gap between two buttons should hit
    /// nothing, not the stack that arranged them. A node scrolled outside its
    /// container is skipped too, because nothing is there to click.
    pub fn hit(&self, tree: &Tree, x: i32, y: i32) -> Option<usize> {
        (0..tree.nodes.len())
            .rev()
            .find(|&index| tree.node(index).tag.is_control() && self.visible_at(index, x, y))
    }

    /// Where a node is on screen, for driving the fake cursor to it.
    pub fn rect_of(&self, index: usize) -> Rect {
        self.rects[index]
    }

    /// Whether any part of a node is on screen.
    ///
    /// This is what an intent naming a node is checked against, so that an agent
    /// cannot act on something scrolled out of sight that no human could have
    /// clicked.
    pub fn is_visible(&self, index: usize) -> bool {
        self.clips[index].intersect(&self.rects[index]).is_some()
    }

    fn visible_at(&self, index: usize, x: i32, y: i32) -> bool {
        self.rects[index].contains(x, y) && self.clips[index].contains(x, y)
    }

    /// The innermost scroll container under a point, for routing the wheel.
    ///
    /// The agent never addresses one of these, which is why `scroll` needs no
    /// id: `scroll-into-view` names the node it wants seen and the compositor
    /// works out what to move.
    pub fn scroller_at(&self, x: i32, y: i32) -> Option<&Scroller> {
        self.scrollers
            .iter()
            .rev()
            .find(|scroller| self.visible_at(scroller.node, x, y))
    }
}

/// Lay out a document, resolving scroll offsets against `scroll` and clamping
/// them back into it.
///
/// The clamp belongs here because only layout knows how tall the content turned
/// out to be. A container whose content shrank should not stay scrolled past its
/// own end.
pub fn layout(
    fonts: &Fonts,
    doc: &Document,
    frame: &Frame,
    scroll: &mut HashMap<String, i32>,
) -> Layout {
    let count = doc.tree.nodes.len();
    let bounds = match frame {
        Frame::Whole(rect) => *rect,
        Frame::Regions(regions) => regions.background,
    };

    let mut placer = Placer {
        fonts,
        doc,
        scroll,
        rects: vec![Rect::new(0, 0, 0, 0); count],
        clips: vec![bounds; count],
        scrollers: Vec::new(),
    };

    match frame {
        Frame::Whole(rect) => placer.place(Tree::ROOT, *rect, *rect),
        Frame::Regions(regions) => placer.place_regions(*regions),
    }

    Layout {
        rects: placer.rects,
        clips: placer.clips,
        scrollers: placer.scrollers,
    }
}

/// The height of one line of a node's own text.
fn line_height(fonts: &Fonts, tree: &Tree, index: usize) -> i32 {
    fonts.line_height(&style_at(tree, index))
}

/// The height of the small label a group or list draws above its children.
fn label_height(fonts: &Fonts, tree: &Tree, index: usize) -> i32 {
    let style = style_at(tree, index);
    fonts.line_height(&Style { size: style.size * 0.85, ..style }) + 6
}

fn control_height(fonts: &Fonts, tree: &Tree, index: usize) -> i32 {
    line_height(fonts, tree, index) + CONTROL_PAD * 2
}

/// Elements that confine their children and so need their own clip.
fn bounded(tag: Tag) -> bool {
    matches!(tag, Tag::Group | Tag::Dialog | Tag::List)
}

/// Elements that keep their children away from their own edges.
///
/// A window is padded but not framed: content should not sit flush against the
/// side of the screen, but the window itself draws nothing but background.
///
/// `pad="none"` opts out. It exists for documents whose frame is already exact,
/// which today means the compositor's own chrome: the navigation bar is sized to
/// its content and double-padding it pushed the buttons out of the bar.
fn padded(node: &Node) -> bool {
    matches!(node.tag, Tag::Dialog | Tag::Window) && node.attr("pad") != Some("none")
}

/// Elements that confine their children to their own rectangle.
fn clipping(tag: Tag) -> bool {
    bounded(tag) || tag == Tag::Scroll
}

/// Elements that draw their own label above their children, and so must reserve
/// room for it. Getting this list wrong does not fail loudly: the label simply
/// draws on top of the first child.
fn titled(tag: Tag) -> bool {
    matches!(tag, Tag::Group | Tag::Dialog | Tag::List)
}

/// How big a node wants to be, given the width it will get.
fn measure(fonts: &Fonts, tree: &Tree, index: usize, width: i32) -> i32 {
    let node = tree.node(index);

    match node.tag {
        Tag::Text | Tag::Icon => line_height(fonts, tree, index),
        Tag::Divider => DIVIDER,
        Tag::Button | Tag::Field | Tag::Item => control_height(fonts, tree, index),
        Tag::Checkbox => control_height(fonts, tree, index).max(CHECKBOX_SIZE),
        Tag::Editor => line_height(fonts, tree, index) * EDITOR_LINES + CONTROL_PAD * 2,

        Tag::HStack => node
            .children
            .iter()
            .map(|&child| measure(fonts, tree, child, width))
            .max()
            .unwrap_or(0),

        // Everything else stacks vertically: the children's heights plus the
        // gaps between them, plus padding for anything that insets.
        //
        // A scroll container measures as its content, so one whose content fits
        // takes exactly the room it needs and never scrolls. It only scrolls
        // once something gives it less than that, which is what `grow` does.
        _ => {
            let padding = if padded(node) { PADDING * 2 } else { 0 };
            let inner = width - padding;
            let gap = gap_of(node);

            let mut height = 0;
            for (position, &child) in node.children.iter().enumerate() {
                if position > 0 {
                    height += gap;
                }
                height += measure(fonts, tree, child, inner);
            }

            let title = if titled(node.tag) && node.attr("label").is_some() {
                label_height(fonts, tree, index) + gap
            } else {
                0
            };
            height + padding + title
        }
    }
}

struct Placer<'a> {
    fonts: &'a Fonts,
    doc: &'a Document,
    scroll: &'a mut HashMap<String, i32>,
    rects: Vec<Rect>,
    clips: Vec<Rect>,
    scrollers: Vec<Scroller>,
}

impl Placer<'_> {
    /// Place each top-level child into the region it declares.
    ///
    /// A child that declares nothing, or declares something it may not have, is
    /// given an empty rectangle. It is then invisible and unreachable rather
    /// than being silently promoted to somewhere it does not belong, which is
    /// the failure that would be hardest to notice.
    fn place_regions(&mut self, regions: Regions) {
        self.rects[Tree::ROOT] = regions.background;
        self.clips[Tree::ROOT] = regions.background;

        let children = self.doc.tree.node(Tree::ROOT).children.clone();
        for child in children {
            let claim = regions.claim(self.doc.tree.node(child).attr("region"));
            match claim {
                // Inset, so a region's content does not sit flush against the
                // edge of the region. The compositor owns the division, so it
                // owns the breathing room too; there is no attribute an
                // agentdesk could set to take it back.
                Some(rect) => self.place(child, rect.inset(PADDING), rect),
                None => {
                    self.rects[child] = Rect::new(0, 0, 0, 0);
                    self.clips[child] = Rect::new(0, 0, 0, 0);
                }
            }
        }
    }

    fn place(&mut self, index: usize, area: Rect, clip: Rect) {
        let tree = &self.doc.tree;
        self.rects[index] = area;
        self.clips[index] = clip;

        let node = tree.node(index);
        let tag = node.tag;
        let padding = if padded(node) { PADDING } else { 0 };
        let mut inner = area.inset(padding);

        // A container that labels itself takes the top of the space before the
        // children divide what is left.
        if titled(tag) && node.attr("label").is_some() {
            let used = label_height(self.fonts, tree, index) + gap_of(node);
            inner = Rect::new(inner.x, inner.y + used, inner.w, inner.h - used);
        }

        let gap = gap_of(node);
        let children = node.children.clone();
        let inside = if clipping(tag) {
            clip.intersect(&area).unwrap_or(Rect::new(area.x, area.y, 0, 0))
        } else {
            clip
        };

        if tag == Tag::HStack {
            self.place_row(&children, inner, gap, inside);
            return;
        }

        if tag == Tag::Scroll {
            self.place_scroll(index, &children, inner, gap, inside);
            return;
        }

        self.place_column(&children, inner, gap, inside);
    }

    /// Children keep their natural width and share the height. A child marked
    /// `grow` takes what is left over, which is how a field sits beside a
    /// fixed-width button.
    fn place_row(&mut self, children: &[usize], inner: Rect, gap: i32, clip: Rect) {
        let tree = &self.doc.tree;
        let mut fixed = 0;
        let mut growers = 0;
        for &child in children {
            if tree.node(child).flag("grow") {
                growers += 1;
            } else {
                fixed += natural_width(self.fonts, tree, child);
            }
        }

        let gaps = gap * (children.len().saturating_sub(1)) as i32;
        let spare = (inner.w - fixed - gaps).max(0);
        let each = if growers > 0 { spare / growers } else { 0 };

        let mut x = inner.x;
        for &child in children {
            let width = if self.doc.tree.node(child).flag("grow") {
                each
            } else {
                natural_width(self.fonts, &self.doc.tree, child)
            };
            self.place(child, Rect::new(x, inner.y, width, inner.h), clip);
            x += width + gap;
        }
    }

    /// Children take their measured height, top to bottom. A child marked `grow`
    /// takes what is left instead, which may be *less* than it asked for: that is
    /// what gives a scroll container a bounded viewport and something to scroll.
    fn place_column(&mut self, children: &[usize], inner: Rect, gap: i32, clip: Rect) {
        let heights = self.column_heights(children, inner, gap);

        let mut y = inner.y;
        for (&child, height) in children.iter().zip(heights) {
            self.place(child, Rect::new(inner.x, y, inner.w, height), clip);
            y += height + gap;
        }
    }

    fn column_heights(&self, children: &[usize], inner: Rect, gap: i32) -> Vec<i32> {
        let tree = &self.doc.tree;
        let natural: Vec<i32> = children
            .iter()
            .map(|&child| measure(self.fonts, tree, child, inner.w))
            .collect();

        let growers = children.iter().filter(|&&c| tree.node(c).flag("grow")).count();
        if growers == 0 {
            return natural;
        }

        let gaps = gap * (children.len().saturating_sub(1)) as i32;
        let fixed: i32 = children
            .iter()
            .zip(&natural)
            .filter(|&(&child, _)| !tree.node(child).flag("grow"))
            .map(|(_, &height)| height)
            .sum();
        let each = (inner.h - fixed - gaps).max(0) / growers as i32;

        children
            .iter()
            .zip(&natural)
            .map(|(&child, &height)| if tree.node(child).flag("grow") { each } else { height })
            .collect()
    }

    /// Content is laid out at its natural height and translated up by the offset,
    /// so every rectangle in the arena is already in screen coordinates. Hit
    /// testing then needs no special case for scrolled content, and neither does
    /// the fake cursor: a node's rectangle is where it is, or it is outside its
    /// clip and cannot be reached at all.
    fn place_scroll(&mut self, index: usize, children: &[usize], inner: Rect, gap: i32, clip: Rect) {
        let tree = &self.doc.tree;
        let heights: Vec<i32> = children
            .iter()
            .map(|&child| measure(self.fonts, tree, child, inner.w))
            .collect();

        let gaps = gap * (children.len().saturating_sub(1)) as i32;
        let content = heights.iter().sum::<i32>() + gaps;
        let furthest = (content - inner.h).max(0);

        let key = self.doc.key(index).to_owned();
        let offset = self.scroll.get(&key).copied().unwrap_or(0).clamp(0, furthest);
        self.scroll.insert(key, offset);
        self.scrollers.push(Scroller {
            node: index,
            content,
            viewport: inner.h,
            offset,
        });

        let mut y = inner.y - offset;
        for (&child, height) in children.iter().zip(heights) {
            self.place(child, Rect::new(inner.x, y, inner.w, height), clip);
            y += height + gap;
        }
    }
}

/// How wide a node is when it is not being stretched.
fn natural_width(fonts: &Fonts, tree: &Tree, index: usize) -> i32 {
    let node = tree.node(index);
    let style = style_at(tree, index);

    match node.tag {
        Tag::Text | Tag::Icon => fonts.measure(label_of(node), &style),
        Tag::Button => fonts.measure(label_of(node), &style) + BUTTON_PAD * 2,
        Tag::Checkbox => CHECKBOX_SIZE + 8 + fonts.measure(label_of(node), &style),
        _ => 160,
    }
}

/// The first `caret` characters of a string, as a slice.
fn prefix(text: &str, caret: usize) -> &str {
    match text.char_indices().nth(caret) {
        Some((at, _)) => &text[..at],
        None => text,
    }
}

pub fn paint(canvas: &mut Canvas, fonts: &Fonts, tree: &Tree, layout: &Layout, focus: &Focus) {
    paint_subtree(canvas, fonts, tree, layout, Tree::ROOT, focus);
}

/// Paint one branch of a document.
///
/// A workspace's regions are separate branches of one tree that do not paint
/// consecutively: the wallpaper goes down, then the application windows on top
/// of it, then the side pane and the taskbar above those. Painting has to be
/// interruptible at the top level for that to be possible.
pub fn paint_subtree(
    canvas: &mut Canvas,
    fonts: &Fonts,
    tree: &Tree,
    layout: &Layout,
    index: usize,
    focus: &Focus,
) {
    paint_node(canvas, fonts, tree, layout, index, focus);
}

/// A thin indicator beside content that overflows.
///
/// Drawn after a scroll container's children so it sits above them, and only
/// when there is something out of sight: a bar on content that fits would say
/// something untrue.
fn paint_scrollbar(canvas: &mut Canvas, layout: &Layout, index: usize) {
    let Some(scroller) = layout.scrollers.iter().find(|s| s.node == index) else {
        return;
    };
    if scroller.content <= scroller.viewport || scroller.viewport <= 0 {
        return;
    }

    let rect = layout.rects[index];
    let track = Rect::new(rect.x + rect.w - SCROLLBAR - 2, rect.y, SCROLLBAR, rect.h);
    canvas.fill_rect(track, SURFACE);

    let span = (track.h * scroller.viewport / scroller.content).max(16);
    let travel = track.h - span;
    let furthest = (scroller.content - scroller.viewport).max(1);
    let top = track.y + travel * scroller.offset / furthest;
    canvas.fill_rect(Rect::new(track.x, top, track.w, span), BORDER);
}

fn paint_node(
    canvas: &mut Canvas,
    fonts: &Fonts,
    tree: &Tree,
    layout: &Layout,
    index: usize,
    focus: &Focus,
) {
    let node = tree.node(index);
    let rect = layout.rects[index];
    let disabled = node.disabled();
    let style = style_at(tree, index);
    let focused = focus.node == Some(index);
    let pressed = focus.pressed == Some(index);

    // A pressed control sinks by a pixel. Small enough not to reflow anything,
    // large enough that a still frame shows which control was just acted on.
    let rect = if pressed { Rect::new(rect.x, rect.y + 1, rect.w, rect.h) } else { rect };

    // Disabled always wins over a colour the application chose: a control that
    // cannot be used must not look like one that can.
    let ink = if disabled { MUTED } else { color_at(tree, index, TEXT) };

    // Text sits vertically centred in whatever box it was given.
    let centred = |height: i32| rect.y + (height - fonts.line_height(&style)) / 2;

    // Nothing draws outside the region its containers left it. The clip is
    // computed once during layout and reused here, so what is painted and what
    // is hit-testable cannot disagree.
    let clip = layout.clips[index];

    canvas.clipped(clip, |canvas| match node.tag {
        Tag::Window => canvas.fill_rect(rect, BACKGROUND),

        Tag::Text => {
            canvas.draw_text(fonts, label_of(node), rect.x, rect.y, &style, ink);
        }

        Tag::Divider => canvas.fill_rect(rect, BORDER),

        Tag::Icon => {
            canvas.draw_text(
                fonts,
                node.attr("alt").unwrap_or("?"),
                rect.x,
                rect.y,
                &style,
                MUTED,
            );
        }

        Tag::Dialog => {
            canvas.shadow(rect, RADIUS_SURFACE, 18, 120);
            canvas.fill_round_rect(rect, RADIUS_SURFACE, RAISED);
            canvas.stroke_round_rect(rect, RADIUS_SURFACE, 1, BORDER);
            if let Some(label) = node.attr("label") {
                canvas.draw_text(fonts, label, rect.x + PADDING, rect.y + PADDING, &style, MUTED);
            }
        }

        // A group draws a hairline and a small label, and nothing else. It is a
        // heading with a rule under it, not a container: a filled box inside a
        // filled window inside a filled region is three surfaces deep and none
        // of them carries information.
        Tag::Group | Tag::List => {
            let heading = style_at(tree, index);
            let heading = Style { size: heading.size * 0.85, ..heading };
            if let Some(label) = node.attr("label") {
                canvas.draw_text(fonts, label, rect.x, rect.y, &heading, MUTED);
                let rule = rect.y + fonts.line_height(&heading) + 3;
                canvas.fill_rect(Rect::new(rect.x, rule, rect.w, 1), BORDER);
            }
        }

        Tag::Button => {
            let emphasis = node.attr("emphasis");
            let fill = match (disabled, pressed, emphasis) {
                (true, _, _) => SURFACE,
                (_, true, Some("primary")) => ACCENT_DEEP,
                (_, true, Some("danger")) => DANGER_DEEP,
                (_, true, _) => PRESSED,
                (_, _, Some("primary")) => ACCENT,
                (_, _, Some("danger")) => DANGER,
                _ => RAISED,
            };
            canvas.fill_round_rect(rect, RADIUS_CONTROL, fill);
            if focused && !disabled {
                canvas.stroke_round_rect(rect, RADIUS_CONTROL, 2, ACCENT);
            } else if emphasis.is_none() {
                canvas.stroke_round_rect(rect, RADIUS_CONTROL, 1, BORDER);
            }

            let label = label_of(node);
            let x = rect.x + (rect.w - fonts.measure(label, &style)) / 2;
            canvas.draw_text(fonts, label, x, centred(rect.h), &style, ink);
        }

        Tag::Field | Tag::Editor => {
            canvas.fill_round_rect(rect, RADIUS_CONTROL, BACKGROUND);
            let edge = match (focused, node.flag("invalid")) {
                (_, true) => DANGER,
                (true, _) => ACCENT,
                _ => BORDER,
            };
            canvas.stroke_round_rect(rect, RADIUS_CONTROL, if focused { 2 } else { 1 }, edge);

            let value = value_of(node);
            let empty = value.is_empty();
            let content = if empty {
                node.attr("placeholder").unwrap_or("").to_owned()
            } else {
                value.clone()
            };
            let color = if empty { MUTED } else { ink };

            let x = rect.x + CONTROL_PAD;
            let top = if node.tag == Tag::Editor { rect.y + CONTROL_PAD } else { centred(rect.h) };
            let step = fonts.line_height(&style);

            canvas.clipped(rect.inset(1), |inner| {
                // An editor is multi-line, so its value is drawn a line at a
                // time. A field is one line and any newline in it would be a
                // client bug, so it is drawn as it stands.
                if node.tag == Tag::Editor {
                    for (row, line) in content.lines().enumerate() {
                        inner.draw_text(fonts, line, x, top + row as i32 * step, &style, color);
                    }
                    if content.is_empty() {
                        inner.draw_text(fonts, &content, x, top, &style, color);
                    }
                } else {
                    inner.draw_text(fonts, &content, x, top, &style, color);
                }

                if !focused {
                    return;
                }

                // The caret is the compositor's, not the application's. It is
                // placed against the value rather than against whatever
                // placeholder is standing in for it.
                let (row, column) = caret_position(&value, focus.caret, node.tag);
                let line = value.lines().nth(row).unwrap_or("");
                let caret_x = x + fonts.measure(prefix(line, column), &style);
                inner.fill_rect(Rect::new(caret_x, top + row as i32 * step, 2, step), ACCENT);
            });
        }

        Tag::Checkbox => {
            let box_rect = Rect::new(
                rect.x,
                rect.y + (rect.h - CHECKBOX_SIZE) / 2,
                CHECKBOX_SIZE,
                CHECKBOX_SIZE,
            );
            let checked = node.flag("checked");
            canvas.fill_round_rect(
                box_rect,
                RADIUS_SMALL,
                if checked && !disabled { ACCENT } else { BACKGROUND },
            );
            if !checked || disabled {
                canvas.stroke_round_rect(box_rect, RADIUS_SMALL, 1, if focused { ACCENT } else { BORDER });
            }
            if checked && disabled {
                canvas.fill_round_rect(box_rect.inset(4), 2, MUTED);
            } else if checked {
                // A tick rather than a filled square: a square inside a square
                // reads as a loading state. Drawn as two strokes stepped along
                // the box's own geometry, because the previous version was a
                // hand-tuned cluster of rectangles that stopped lining up the
                // first time the box changed size.
                let t = box_rect.inset(3);
                let (w, h) = (t.w as f32, t.h as f32);
                let low = (t.x as f32 + w * 0.36, t.y as f32 + h * 0.72);
                let strokes = [
                    ((t.x as f32 + w * 0.08, t.y as f32 + h * 0.46), low),
                    (low, (t.x as f32 + w * 0.90, t.y as f32 + h * 0.12)),
                ];
                for ((ax, ay), (bx, by)) in strokes {
                    let steps = t.w.max(4);
                    for step in 0..=steps {
                        let f = step as f32 / steps as f32;
                        let px = (ax + (bx - ax) * f).round() as i32;
                        let py = (ay + (by - ay) * f).round() as i32;
                        canvas.fill_rect(Rect::new(px, py - 1, 2, 2), BACKGROUND);
                    }
                }
            }
            canvas.draw_text(
                fonts,
                label_of(node),
                rect.x + CHECKBOX_SIZE + 8,
                centred(rect.h),
                &style,
                ink,
            );
        }

        Tag::Item => {
            if node.flag("selected") {
                canvas.fill_rect(rect, SELECTED);
            }
            if focused {
                canvas.stroke_rect(rect, 1, ACCENT);
            }
            canvas.draw_text(
                fonts,
                label_of(node),
                rect.x + CONTROL_PAD,
                centred(rect.h),
                &style,
                ink,
            );
        }

        // Pure arrangement draws nothing at all.
        Tag::VStack | Tag::HStack | Tag::Scroll => {}
    });

    for &child in &tree.node(index).children {
        paint_node(canvas, fonts, tree, layout, child, focus);
    }

    if node.tag == Tag::Scroll {
        canvas.clipped(clip, |canvas| paint_scrollbar(canvas, layout, index));
    }
}

/// Turn a caret offset in characters into a line and a column.
///
/// A field has one line by construction, so it is column-only. An editor may
/// have many, and the caret has to survive a newline in the middle of the value.
pub fn caret_position(value: &str, caret: usize, tag: Tag) -> (usize, usize) {
    if tag != Tag::Editor {
        return (0, caret.min(value.chars().count()));
    }

    let mut row = 0;
    let mut column = 0;
    for (at, character) in value.chars().enumerate() {
        if at >= caret {
            break;
        }
        if character == '\n' {
            row += 1;
            column = 0;
        } else {
            column += 1;
        }
    }
    (row, column)
}

/// Which character of a text control's value a click at `x` landed on.
///
/// Measuring one prefix at a time rather than doing anything cleverer: a field
/// holds a line of text, this runs once per click, and a binary search over a
/// proportional font is not obviously correct at the ends.
pub fn caret_from_x(fonts: &Fonts, value: &str, style: &Style, left: i32, x: i32) -> usize {
    let mut best = 0;
    let mut closest = i32::MAX;
    for caret in 0..=value.chars().count() {
        let at = left + fonts.measure(prefix(value, caret), style);
        let distance = (at - x).abs();
        if distance < closest {
            closest = distance;
            best = caret;
        }
    }
    best
}

/// The left edge text starts at inside a text control.
pub fn text_origin(rect: Rect) -> i32 {
    rect.x + CONTROL_PAD
}
