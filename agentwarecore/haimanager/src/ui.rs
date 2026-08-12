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
pub const BACKGROUND: Color = rgb(0x10, 0x12, 0x18);
pub const SURFACE: Color = rgb(0x1c, 0x20, 0x2a);
pub const RAISED: Color = rgb(0x26, 0x2c, 0x3a);
pub const BORDER: Color = rgb(0x3a, 0x42, 0x54);
pub const TEXT: Color = rgb(0xe6, 0xe9, 0xef);
pub const MUTED: Color = rgb(0x8a, 0x93, 0xa6);
pub const ACCENT: Color = rgb(0x4f, 0x9c, 0xf5);
pub const DANGER: Color = rgb(0xe0, 0x5a, 0x5a);
pub const OK: Color = rgb(0x5a, 0xc8, 0x8a);
pub const SELECTED: Color = rgb(0x2b, 0x3f, 0x5e);

const BODY_SIZE: f32 = 15.0;
const PADDING: i32 = 10;
/// Space above and below the text inside a control.
const CONTROL_PAD: i32 = 8;
const CHECKBOX_SIZE: i32 = 16;
const DIVIDER: i32 = 1;
/// An editor is this many lines tall.
const EDITOR_LINES: i32 = 4;
/// Width of the indicator drawn beside overflowing scroll content.
const SCROLLBAR: i32 = 4;
/// How far one notch of the wheel moves a scroll container.
pub const WHEEL_STEP: i32 = 48;

fn gap_of(node: &Node) -> i32 {
    match node.attr("gap") {
        Some("none") => 0,
        Some("sm") => 4,
        Some("lg") => 16,
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
            Some("heading") => BODY_SIZE * 1.75,
            Some("subheading") => BODY_SIZE * 1.3,
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
    area: Rect,
    scroll: &mut HashMap<String, i32>,
) -> Layout {
    let count = doc.tree.nodes.len();
    let mut placer = Placer {
        fonts,
        doc,
        scroll,
        rects: vec![Rect::new(0, 0, 0, 0); count],
        clips: vec![area; count],
        scrollers: Vec::new(),
    };
    placer.place(Tree::ROOT, area, area);

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

fn control_height(fonts: &Fonts, tree: &Tree, index: usize) -> i32 {
    line_height(fonts, tree, index) + CONTROL_PAD * 2
}

/// Elements that draw a surface behind their children.
fn framed(tag: Tag) -> bool {
    matches!(tag, Tag::Group | Tag::Dialog | Tag::List)
}

/// Elements that keep their children away from their own edges.
///
/// A window is padded but not framed: content should not sit flush against the
/// side of the screen, but the window itself draws nothing but background.
fn padded(tag: Tag) -> bool {
    framed(tag) || tag == Tag::Window
}

/// Elements that confine their children to their own rectangle.
fn clipping(tag: Tag) -> bool {
    framed(tag) || tag == Tag::Scroll
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
            let padding = if padded(node.tag) { PADDING * 2 } else { 0 };
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
                line_height(fonts, tree, index) + gap
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
    fn place(&mut self, index: usize, area: Rect, clip: Rect) {
        let tree = &self.doc.tree;
        self.rects[index] = area;
        self.clips[index] = clip;

        let node = tree.node(index);
        let tag = node.tag;
        let padding = if padded(tag) { PADDING } else { 0 };
        let mut inner = area.inset(padding);

        // A container that labels itself takes the top of the space before the
        // children divide what is left.
        if titled(tag) && node.attr("label").is_some() {
            let used = line_height(self.fonts, tree, index) + gap_of(node);
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
        Tag::Button => fonts.measure(label_of(node), &style) + PADDING * 2,
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
    paint_node(canvas, fonts, tree, layout, Tree::ROOT, focus);
    paint_scrollbars(canvas, layout);
}

/// A thin indicator beside content that overflows.
///
/// Drawn last so it sits above whatever it is describing, and only when there is
/// something out of sight: a bar on content that fits would say something untrue.
fn paint_scrollbars(canvas: &mut Canvas, layout: &Layout) {
    for scroller in &layout.scrollers {
        if scroller.content <= scroller.viewport || scroller.viewport <= 0 {
            continue;
        }

        let rect = layout.rects[scroller.node];
        let track = Rect::new(rect.x + rect.w - SCROLLBAR - 2, rect.y, SCROLLBAR, rect.h);
        canvas.fill_rect(track, SURFACE);

        let span = (track.h * scroller.viewport / scroller.content).max(16);
        let travel = track.h - span;
        let furthest = (scroller.content - scroller.viewport).max(1);
        let top = track.y + travel * scroller.offset / furthest;
        canvas.fill_rect(Rect::new(track.x, top, track.w, span), BORDER);
    }
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

        Tag::Group | Tag::Dialog | Tag::List => {
            canvas.fill_rect(rect, if node.tag == Tag::Dialog { RAISED } else { SURFACE });
            canvas.stroke_rect(rect, 1, BORDER);
            if let Some(label) = node.attr("label") {
                canvas.draw_text(fonts, label, rect.x + PADDING, rect.y + PADDING, &style, MUTED);
            }
        }

        Tag::Button => {
            let fill = match (disabled, node.attr("emphasis")) {
                (true, _) => SURFACE,
                (_, Some("primary")) => ACCENT,
                (_, Some("danger")) => DANGER,
                _ => RAISED,
            };
            canvas.fill_rect(rect, fill);
            canvas.stroke_rect(rect, 1, if focused { TEXT } else { BORDER });

            let label = label_of(node);
            let x = rect.x + (rect.w - fonts.measure(label, &style)) / 2;
            canvas.draw_text(fonts, label, x, centred(rect.h), &style, ink);
        }

        Tag::Field | Tag::Editor => {
            canvas.fill_rect(rect, BACKGROUND);
            let edge = match (focused, node.flag("invalid")) {
                (_, true) => DANGER,
                (true, _) => ACCENT,
                _ => BORDER,
            };
            canvas.stroke_rect(rect, 1, edge);

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
            canvas.fill_rect(box_rect, BACKGROUND);
            canvas.stroke_rect(box_rect, 1, if focused { ACCENT } else { BORDER });
            if node.flag("checked") {
                canvas.fill_rect(box_rect.inset(4), if disabled { MUTED } else { ACCENT });
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
