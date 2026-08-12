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

use crate::awml::{Node, Tag, Tree};
use crate::paint::{Canvas, Color, Rect, rgb};

// The whole of the system's appearance. Applications do not get to choose any
// of it, which is what keeps every app legible to an agent that has learned one
// visual language, and keeps the renderer small.
pub const BACKGROUND: Color = rgb(0x10, 0x12, 0x18);
pub const SURFACE: Color = rgb(0x1c, 0x20, 0x2a);
pub const RAISED: Color = rgb(0x26, 0x2c, 0x3a);
pub const BORDER: Color = rgb(0x3a, 0x42, 0x54);
pub const TEXT: Color = rgb(0xe6, 0xe9, 0xef);
pub const MUTED: Color = rgb(0x8a, 0x93, 0xa6);
pub const ACCENT: Color = rgb(0x4f, 0x9c, 0xf5);
pub const DANGER: Color = rgb(0xe0, 0x5a, 0x5a);
pub const SELECTED: Color = rgb(0x2b, 0x3f, 0x5e);

const TEXT_SCALE: i32 = 2;
const HEADING_SCALE: i32 = 3;
const PADDING: i32 = 10;
const CONTROL_HEIGHT: i32 = 32;
const EDITOR_HEIGHT: i32 = 96;
const CHECKBOX_SIZE: i32 = 16;
const DIVIDER: i32 = 1;

fn gap_of(node: &Node) -> i32 {
    match node.attr("gap") {
        Some("none") => 0,
        Some("sm") => 4,
        Some("lg") => 16,
        _ => 8,
    }
}

fn scale_of(node: &Node) -> i32 {
    match node.attr("role") {
        Some("heading") => HEADING_SCALE,
        _ => TEXT_SCALE,
    }
}

/// A node's label as drawn: the `label` attribute, or its text content.
fn label_of(node: &Node) -> &str {
    node.attr("label").unwrap_or(&node.text)
}

/// The value shown in a text control, masked if it is a password.
///
/// Masking here rather than at the app means an application cannot leak a
/// password by forgetting, and the agent's view is built from the same string,
/// so it cannot see one either.
fn value_of(node: &Node) -> String {
    let value = node.attr("value").unwrap_or("");
    if node.attr("kind") == Some("password") {
        return "*".repeat(value.chars().count());
    }
    value.to_owned()
}

pub struct Layout {
    /// One rectangle per node, indexed as the arena is.
    pub rects: Vec<Rect>,
}

impl Layout {
    /// The innermost node at a point that an agent or a human could act on.
    ///
    /// Walked in reverse so later siblings, which paint on top, win. Layout
    /// elements are skipped: clicking the gap between two buttons should hit
    /// nothing, not the stack that arranged them.
    pub fn hit(&self, tree: &Tree, x: i32, y: i32) -> Option<usize> {
        (0..tree.nodes.len()).rev().find(|&index| {
            tree.node(index).tag.is_control() && self.rects[index].contains(x, y)
        })
    }

    /// Where a node is on screen, for driving the fake cursor to it.
    pub fn rect_of(&self, index: usize) -> Rect {
        self.rects[index]
    }
}

/// Lay a tree out inside `area`.
pub fn layout(tree: &Tree, area: Rect) -> Layout {
    let mut rects = vec![Rect::new(0, 0, 0, 0); tree.nodes.len()];
    place(tree, Tree::ROOT, area, &mut rects);
    Layout { rects }
}

/// How big a node wants to be, given the width it will get.
///
/// Width is an input because text-shaped things are as tall as the room they
/// have is wide. Nothing wraps yet, but the signature is the one wrapping
/// needs, so adding it later does not mean rewriting every caller.
fn measure(tree: &Tree, index: usize, width: i32) -> i32 {
    let node = tree.node(index);

    match node.tag {
        Tag::Text => Canvas::text_height(scale_of(node)),
        Tag::Divider => DIVIDER,
        Tag::Icon => Canvas::text_height(TEXT_SCALE),
        Tag::Button | Tag::Field | Tag::Item => CONTROL_HEIGHT,
        Tag::Checkbox => CONTROL_HEIGHT.max(CHECKBOX_SIZE),
        Tag::Editor => EDITOR_HEIGHT,

        Tag::HStack => {
            let inner = width;
            node.children
                .iter()
                .map(|&child| measure(tree, child, inner))
                .max()
                .unwrap_or(0)
        }

        // Everything else stacks vertically: the children's heights plus the
        // gaps between them, plus padding for anything that draws a frame.
        _ => {
            let padding = if padded(node.tag) { PADDING * 2 } else { 0 };
            let inner = width - padding;
            let gap = gap_of(node);

            let mut height = 0;
            for (position, &child) in node.children.iter().enumerate() {
                if position > 0 {
                    height += gap;
                }
                height += measure(tree, child, inner);
            }

            let title = if titled(node.tag) && node.attr("label").is_some() {
                Canvas::text_height(TEXT_SCALE) + gap
            } else {
                0
            };
            height + padding + title
        }
    }
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

/// Elements that draw their own label above their children, and so must reserve
/// room for it. Getting this list wrong does not fail loudly: the label simply
/// draws on top of the first child.
fn titled(tag: Tag) -> bool {
    matches!(tag, Tag::Group | Tag::Dialog | Tag::List)
}

fn place(tree: &Tree, index: usize, area: Rect, rects: &mut [Rect]) {
    rects[index] = area;
    let node = tree.node(index);

    let padding = if padded(node.tag) { PADDING } else { 0 };
    let mut inner = area.inset(padding);

    // A container that labels itself takes the top of the space before the
    // children divide what is left.
    if titled(node.tag) && node.attr("label").is_some() {
        let used = Canvas::text_height(TEXT_SCALE) + gap_of(node);
        inner = Rect::new(inner.x, inner.y + used, inner.w, inner.h - used);
    }

    let gap = gap_of(node);

    if node.tag == Tag::HStack {
        // Children keep their measured width and share the height. A child
        // marked `grow` takes whatever is left over, which is how a field sits
        // beside a fixed-width button.
        let mut fixed = 0;
        let mut growers = 0;
        for &child in &node.children {
            if tree.node(child).flag("grow") {
                growers += 1;
            } else {
                fixed += natural_width(tree, child);
            }
        }

        let gaps = gap * (node.children.len().saturating_sub(1)) as i32;
        let spare = (inner.w - fixed - gaps).max(0);
        let each = if growers > 0 { spare / growers } else { 0 };

        let mut x = inner.x;
        for &child in &node.children {
            let width = if tree.node(child).flag("grow") {
                each
            } else {
                natural_width(tree, child)
            };
            place(tree, child, Rect::new(x, inner.y, width, inner.h), rects);
            x += width + gap;
        }
        return;
    }

    // Vertical: every child gets the full width and its own measured height.
    let mut y = inner.y;
    for &child in &node.children {
        let height = measure(tree, child, inner.w);
        place(tree, child, Rect::new(inner.x, y, inner.w, height), rects);
        y += height + gap;
    }
}

/// How wide a node is when it is not being stretched.
fn natural_width(tree: &Tree, index: usize) -> i32 {
    let node = tree.node(index);
    match node.tag {
        Tag::Text | Tag::Icon => Canvas::text_width(label_of(node), scale_of(node)),
        Tag::Button => Canvas::text_width(label_of(node), TEXT_SCALE) + PADDING * 2,
        Tag::Checkbox => {
            CHECKBOX_SIZE + 8 + Canvas::text_width(label_of(node), TEXT_SCALE)
        }
        _ => 160,
    }
}

/// Paint a laid-out tree.
pub fn paint(canvas: &mut Canvas, tree: &Tree, layout: &Layout, focus: Option<usize>) {
    paint_node(canvas, tree, layout, Tree::ROOT, focus);
}

fn paint_node(
    canvas: &mut Canvas,
    tree: &Tree,
    layout: &Layout,
    index: usize,
    focus: Option<usize>,
) {
    let node = tree.node(index);
    let rect = layout.rects[index];
    let disabled = node.disabled();
    let ink = if disabled { MUTED } else { TEXT };

    match node.tag {
        Tag::Window => canvas.fill_rect(rect, BACKGROUND),

        Tag::Text => {
            let color = match node.attr("emphasis") {
                Some("muted") => MUTED,
                Some("accent") => ACCENT,
                Some("danger") => DANGER,
                _ => ink,
            };
            canvas.draw_text(label_of(node), rect.x, rect.y, scale_of(node), color);
        }

        Tag::Divider => canvas.fill_rect(rect, BORDER),

        Tag::Icon => {
            canvas.draw_text(node.attr("alt").unwrap_or("?"), rect.x, rect.y, TEXT_SCALE, MUTED);
        }

        Tag::Group | Tag::Dialog | Tag::List => {
            canvas.fill_rect(rect, if node.tag == Tag::Dialog { RAISED } else { SURFACE });
            canvas.stroke_rect(rect, 1, BORDER);
            if let Some(label) = node.attr("label") {
                canvas.draw_text(label, rect.x + PADDING, rect.y + PADDING, TEXT_SCALE, MUTED);
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
            canvas.stroke_rect(rect, 1, if focus == Some(index) { TEXT } else { BORDER });

            let label = label_of(node);
            let x = rect.x + (rect.w - Canvas::text_width(label, TEXT_SCALE)) / 2;
            let y = rect.y + (rect.h - Canvas::text_height(TEXT_SCALE)) / 2;
            canvas.draw_text(label, x, y, TEXT_SCALE, ink);
        }

        Tag::Field | Tag::Editor => {
            canvas.fill_rect(rect, BACKGROUND);
            let edge = match (focus == Some(index), node.flag("invalid")) {
                (_, true) => DANGER,
                (true, _) => ACCENT,
                _ => BORDER,
            };
            canvas.stroke_rect(rect, 1, edge);

            let value = value_of(node);
            let (content, color) = if value.is_empty() {
                (node.attr("placeholder").unwrap_or("").to_owned(), MUTED)
            } else {
                (value, ink)
            };
            canvas.clipped(rect.inset(1), |inner| {
                inner.draw_text(&content, rect.x + 8, rect.y + 9, TEXT_SCALE, color);
            });
        }

        Tag::Checkbox => {
            let box_rect = Rect::new(rect.x, rect.y + 8, CHECKBOX_SIZE, CHECKBOX_SIZE);
            canvas.fill_rect(box_rect, BACKGROUND);
            canvas.stroke_rect(box_rect, 1, if focus == Some(index) { ACCENT } else { BORDER });
            if node.flag("checked") {
                canvas.fill_rect(box_rect.inset(4), if disabled { MUTED } else { ACCENT });
            }
            canvas.draw_text(
                label_of(node),
                rect.x + CHECKBOX_SIZE + 8,
                rect.y + 9,
                TEXT_SCALE,
                ink,
            );
        }

        Tag::Item => {
            if node.flag("selected") {
                canvas.fill_rect(rect, SELECTED);
            }
            if focus == Some(index) {
                canvas.stroke_rect(rect, 1, ACCENT);
            }
            canvas.draw_text(label_of(node), rect.x + 8, rect.y + 9, TEXT_SCALE, ink);
        }

        // Pure arrangement draws nothing at all.
        Tag::VStack | Tag::HStack | Tag::Scroll => {}
    }

    // Children paint after their parent, so a surface never covers what sits
    // on it, and clipped inside it, so nothing escapes its container.
    let clip = if framed(node.tag) || node.tag == Tag::Scroll {
        rect
    } else {
        canvas.clip()
    };

    canvas.clipped(clip, |canvas| {
        for &child in &tree.node(index).children {
            paint_node(canvas, tree, layout, child, focus);
        }
    });
}
