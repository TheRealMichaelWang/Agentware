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

use crate::awml::{Node, Tag, Tree};
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

pub fn layout(fonts: &Fonts, tree: &Tree, area: Rect) -> Layout {
    let mut rects = vec![Rect::new(0, 0, 0, 0); tree.nodes.len()];
    place(fonts, tree, Tree::ROOT, area, &mut rects);
    Layout { rects }
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

fn place(fonts: &Fonts, tree: &Tree, index: usize, area: Rect, rects: &mut [Rect]) {
    rects[index] = area;
    let node = tree.node(index);

    let padding = if padded(node.tag) { PADDING } else { 0 };
    let mut inner = area.inset(padding);

    // A container that labels itself takes the top of the space before the
    // children divide what is left.
    if titled(node.tag) && node.attr("label").is_some() {
        let used = line_height(fonts, tree, index) + gap_of(node);
        inner = Rect::new(inner.x, inner.y + used, inner.w, inner.h - used);
    }

    let gap = gap_of(node);

    if node.tag == Tag::HStack {
        // Children keep their natural width and share the height. A child
        // marked `grow` takes what is left over, which is how a field sits
        // beside a fixed-width button.
        let mut fixed = 0;
        let mut growers = 0;
        for &child in &node.children {
            if tree.node(child).flag("grow") {
                growers += 1;
            } else {
                fixed += natural_width(fonts, tree, child);
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
                natural_width(fonts, tree, child)
            };
            place(fonts, tree, child, Rect::new(x, inner.y, width, inner.h), rects);
            x += width + gap;
        }
        return;
    }

    let mut y = inner.y;
    for &child in &node.children {
        let height = measure(fonts, tree, child, inner.w);
        place(fonts, tree, child, Rect::new(inner.x, y, inner.w, height), rects);
        y += height + gap;
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

pub fn paint(
    canvas: &mut Canvas,
    fonts: &Fonts,
    tree: &Tree,
    layout: &Layout,
    focus: Option<usize>,
) {
    paint_node(canvas, fonts, tree, layout, Tree::ROOT, focus);
}

fn paint_node(
    canvas: &mut Canvas,
    fonts: &Fonts,
    tree: &Tree,
    layout: &Layout,
    index: usize,
    focus: Option<usize>,
) {
    let node = tree.node(index);
    let rect = layout.rects[index];
    let disabled = node.disabled();
    let style = style_at(tree, index);

    // Disabled always wins over a colour the application chose: a control that
    // cannot be used must not look like one that can.
    let ink = if disabled { MUTED } else { color_at(tree, index, TEXT) };

    // Text sits vertically centred in whatever box it was given.
    let centred = |height: i32| rect.y + (height - fonts.line_height(&style)) / 2;

    match node.tag {
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
                canvas.draw_text(
                    fonts,
                    label,
                    rect.x + PADDING,
                    rect.y + PADDING,
                    &style,
                    MUTED,
                );
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
            let x = rect.x + (rect.w - fonts.measure(label, &style)) / 2;
            canvas.draw_text(fonts, label, x, centred(rect.h), &style, ink);
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
            let y = if node.tag == Tag::Editor {
                rect.y + CONTROL_PAD
            } else {
                centred(rect.h)
            };
            canvas.clipped(rect.inset(1), |inner| {
                inner.draw_text(fonts, &content, rect.x + CONTROL_PAD, y, &style, color);
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
            canvas.stroke_rect(box_rect, 1, if focus == Some(index) { ACCENT } else { BORDER });
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
            if focus == Some(index) {
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
    }

    // Children paint after their parent, so a surface never covers what sits on
    // it, and clipped inside it, so nothing escapes its container.
    let clip = if framed(node.tag) || node.tag == Tag::Scroll {
        rect
    } else {
        canvas.clip()
    };

    canvas.clipped(clip, |canvas| {
        for &child in &tree.node(index).children {
            paint_node(canvas, fonts, tree, layout, child, focus);
        }
    });
}
