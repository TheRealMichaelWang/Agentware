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
use std::sync::OnceLock;

use crate::awml::{Node, Tag, Tree};
use crate::document::Document;
use crate::images::Images;
use crate::paint::font::{Family, Fonts, Style, Weight};
use crate::paint::{Canvas, Color, Rect};

// The palette an application names colours out of. An app may also give a hex
// value; these exist so the common cases stay consistent between applications
// and follow the system theme.
//
// The palette is the theme: it comes from the theme file the settings name
// (`awproto::theme`), installed whole by [`set_theme`] at startup and again
// when the setting changes. One struct behind one static rather than a
// colour per static, so the palette cannot be half-swapped and adding a
// colour is one field, not four edits. It is a static rather than a value
// threaded through every paint call for the same reason the scale is:
// one value, set at one moment, read everywhere. Everything paints by
// asking, so the frame after a theme change is simply in the new palette.
//
// The initial value is deliberately NOT a theme. Themes live in
// /default_themes and only there; a palette compiled in here would drift
// from the file it copied and defeat the point of loading one. This is the
// emergency monochrome the screen wears when no theme file loads at all:
// legible enough to reach the settings and fix it, wrong enough that nobody
// mistakes a broken image for a working one. A fallback that looked right
// would be a bug nobody reports.
use awproto::theme::Theme;
use std::sync::RwLock;

const EMERGENCY: Theme = Theme {
    background: 0x000000,
    surface: 0x161616,
    raised: 0x2a2a2a,
    pressed: 0x3d3d3d,
    border: 0x555555,
    text: 0xffffff,
    muted: 0x9a9a9a,
    accent: 0xd0d0d0,
    accent_deep: 0xb0b0b0,
    danger: 0xe8e8e8,
    danger_deep: 0xc8c8c8,
    ok: 0xd0d0d0,
    selected: 0x3d3d3d,
};

static THEME: RwLock<Theme> = RwLock::new(EMERGENCY);

/// The palette as it stands. The lock is uncontended on a one-thread
/// compositor and cannot be poisoned by [`set_theme`]'s panic-free write,
/// so the emergency arm is unreachable and honest rather than load-bearing.
fn theme() -> Theme {
    THEME.read().map(|held| *held).unwrap_or(EMERGENCY)
}

/// Install a palette, whole.
pub fn set_theme(theme: &Theme) {
    if let Ok(mut held) = THEME.write() {
        *held = *theme;
    }
}

/// The deepest layer: the desk behind everything, window bodies.
pub fn background() -> Color { theme().background }
pub fn surface() -> Color { theme().surface }
pub fn raised() -> Color { theme().raised }
/// One step above raised, for a control under the pointer or being pressed.
pub fn pressed() -> Color { theme().pressed }
pub fn border() -> Color { theme().border }
pub fn text() -> Color { theme().text }
pub fn muted() -> Color { theme().muted }
pub fn accent() -> Color { theme().accent }
pub fn accent_deep() -> Color { theme().accent_deep }
pub fn danger() -> Color { theme().danger }
pub fn danger_deep() -> Color { theme().danger_deep }
pub fn ok() -> Color { theme().ok }
pub fn selected() -> Color { theme().selected }

/// A colour nudged brighter, for the top edge of a gradient.
///
/// Gradients here are lighting, not decoration: a surface lit faintly from
/// above reads as raised without a heavier border doing the work. The nudge is
/// small on purpose; a gradient anyone's eye snags on is too strong.
pub fn lift(color: Color, by: u8) -> Color {
    let channel = |shift: u32| ((color >> shift) & 0xff).saturating_add(by as u32).min(255);
    (channel(16) << 16) | (channel(8) << 8) | channel(0)
}

/// The interface scale, set once at startup before anything is measured.
///
/// Every metric below is a *logical* size multiplied by this. It exists because
/// the guest resolution is whatever monitor QEMU was told to be, and 13px type
/// that is right at 1000 rows tall is illegibly small at 1440. One number, set
/// once, scales the whole interface coherently; scattering per-element tweaks
/// is how proportions drift apart.
static SCALE: OnceLock<f32> = OnceLock::new();

/// Fix the scale for the lifetime of the process. Must happen before layout.
pub fn set_scale(scale: f32) {
    let _ = SCALE.set(scale.clamp(1.0, 3.0));
}

pub fn scale() -> f32 {
    *SCALE.get().unwrap_or(&1.0)
}

/// A logical dimension in physical pixels.
pub fn sc(logical: i32) -> i32 {
    (logical as f32 * scale()).round() as i32
}

/// Corner radii. Everything drawn gets one, because a hard corner at these
/// sizes is what makes a surface look like a drawn rectangle rather than a
/// panel, and one square element among rounded ones looks like a bug.
///
/// Kept small. A large radius on a small control is the single loudest thing an
/// interface can do, and it reads as a toy rather than as a tool.
pub fn radius_window() -> i32 { sc(8) }
pub fn radius_surface() -> i32 { sc(6) }
pub fn radius_control() -> i32 { sc(5) }
pub fn radius_small() -> i32 { sc(3) }

// Density. These are the numbers that decide whether the result looks like an
// interface or like a toy, and every one of them was too large.
//
// The reference points are the desktops people actually use: a 13px system font,
// a control about 28px tall, and single-digit padding almost everywhere. Chunky
// controls do not read as friendly at this scale, they read as unfinished.
fn body_size() -> f32 { 13.0 * scale() }
fn padding() -> i32 { sc(10) }
/// Space above and below the taskbar's row of controls. Public because the
/// screen sizes the band from it: the band is a control plus this twice.
pub fn taskbar_inset() -> i32 { sc(8) }
/// How tall one control is: the number the taskbar band is built around.
pub fn control_h(fonts: &Fonts) -> i32 {
    fonts.line_height(&Style { size: body_size(), ..Style::default() }) + control_pad() * 2
}
/// The icon on a tile button, and the room around it.
fn tile_icon() -> i32 { sc(28) }
fn tile_pad() -> i32 { sc(10) }
/// The icon beside a button's label, when it has one.
fn button_icon() -> i32 { sc(16) }
/// How wide a dialog is, floating over its window. Fixed rather than fitted,
/// because a dialog is a form and forms read best at one width; a window
/// narrower than this gets a dialog as wide as itself.
fn dialog_w() -> i32 { sc(460) }
/// An image that is not filling a region is this wide, at 16:9. AWML has no
/// natural size to ask a picture for, so a picture placed among controls takes
/// a slot the size of a preview; one that fills a region takes the region.
fn image_w() -> i32 { sc(160) }
/// Space above and below the text inside a control.
fn control_pad() -> i32 { sc(6) }
/// Space either side of the text inside a button.
fn button_pad() -> i32 { sc(12) }
fn checkbox_size() -> i32 { sc(14) }
const DIVIDER: i32 = 1;
/// An editor is this many lines tall.
const EDITOR_LINES: i32 = 4;
/// Width of the indicator drawn beside overflowing scroll content.
fn scrollbar_w() -> i32 { sc(5) }
/// How far one notch of the wheel moves a scroll container.
pub fn wheel_step() -> i32 { sc(48) }
/// The stored offset of a scroll container that is following its end.
const AT_END: i32 = i32::MAX;

fn gap_of(node: &Node) -> i32 {
    match node.attr("gap") {
        Some("none") => 0,
        Some("sm") => sc(4),
        Some("lg") => sc(14),
        _ => sc(8),
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
            Some("heading") => body_size() * 1.45,
            Some("subheading") => body_size() * 1.15,
            Some("caption") => body_size() * 0.85,
            _ => body_size(),
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
    let named = match value {
        "xs" => Some(0.75),
        "sm" => Some(0.85),
        "md" => Some(1.0),
        "lg" => Some(1.3),
        "xl" => Some(1.75),
        _ => None,
    };
    if let Some(factor) = named {
        return Some(body_size() * factor);
    }

    // Clamped, because a client asking for 4000px would have every glyph
    // rasterize a coverage map the size of the screen. The scale applies to an
    // app-chosen size too: it named a logical size, not a number of photons.
    value.parse::<f32>().ok().map(|size| size.clamp(6.0, 200.0) * scale())
}

/// Resolve a node's text colour, falling back to `default`.
pub fn color_at(tree: &Tree, index: usize, default: Color) -> Color {
    let Some(value) = tree.inherited(index, "color") else {
        return default;
    };

    match value {
        "text" => text(),
        "muted" => muted(),
        "accent" => accent(),
        "danger" => danger(),
        "ok" => ok(),
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
    /// Whether the caret is in the lit half of its blink.
    ///
    /// The clock lives in the compositor, not here: a caret blinks in exactly
    /// one place on a screen, the place keystrokes go, so its phase is screen
    /// state rather than a property of every window that remembers a focus.
    pub caret_visible: bool,
    /// The scroll container whose bar is currently showing, if any.
    ///
    /// Scrollbars auto-hide: one appears while its content is being moved and
    /// lingers briefly after, which is what lets it sit over the content's edge
    /// without permanently covering anything.
    pub scrollbar: Option<usize>,
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
    /// Nothing behind an open dialog answers, however visible it is: that is
    /// what makes the dialog modal for the human, and the agent's view says
    /// the same thing about the same nodes.
    ///
    /// Walked in reverse so later siblings, which paint on top, win. Layout
    /// elements are skipped: clicking the gap between two buttons should hit
    /// nothing, not the stack that arranged them. A node scrolled outside its
    /// container is skipped too, because nothing is there to click.
    pub fn hit(&self, tree: &Tree, x: i32, y: i32) -> Option<usize> {
        (0..tree.nodes.len())
            .rev()
            .find(|&index| {
                tree.node(index).tag.is_control()
                    && !tree.blocked(index)
                    && !tree.folded(index)
                    && self.visible_at(index, x, y)
            })
    }

    /// The control under a point, with a dropdown's floating options taking
    /// precedence over whatever they hang across. Layout does not know what
    /// floats; the tree does, so callers pass it and this checks the floating
    /// nodes before the rest.
    pub fn hit_with_overlays(&self, tree: &Tree, x: i32, y: i32) -> Option<usize> {
        for select in tree.open_selects().into_iter().rev() {
            if let Some(option) = tree
                .node(select)
                .children
                .iter()
                .rev()
                .copied()
                .find(|&child| !tree.node(child).disabled() && self.visible_at(child, x, y))
            {
                return Some(option);
            }
        }
        self.hit(tree, x, y)
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
        self.scrollers_at(x, y).next()
    }

    /// Every scroll container under a point, innermost first.
    ///
    /// The wheel wants the list, not just the innermost: a container that has
    /// nowhere further to go in the wheel's direction hands the notch to the
    /// one enclosing it, which is how a list inside a page scrolls the page
    /// once the list is at its end, and how a list that never overflowed
    /// does not swallow the wheel and leave the page stuck. Without this a
    /// pointer that landed on an inner container after the first notch made
    /// every notch after it do nothing.
    pub fn scrollers_at(&self, x: i32, y: i32) -> impl Iterator<Item = &Scroller> {
        self.scrollers
            .iter()
            .rev()
            .filter(move |scroller| self.visible_at(scroller.node, x, y))
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
    line_height(fonts, tree, index) + control_pad() * 2
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

    // The vertical twin of the `width` hint: compositor-internal, physical
    // pixels, for chrome that knows the box it wants. The start menu's prompt
    // is the one user; a large box for a large ask.
    if let Some(height) = node.attr("height").and_then(|value| value.parse::<i32>().ok()) {
        return height.max(sc(20));
    }

    match node.tag {
        // Text wraps to the width it is given, so a paragraph in a narrow pane
        // is as tall as its lines rather than one line clipped at the edge.
        Tag::Text => {
            let style = style_at(tree, index);
            let lines = wrap(fonts, label_of(node), &style, width).len().max(1) as i32;
            lines * fonts.line_height(&style)
        }
        Tag::Icon => line_height(fonts, tree, index),
        Tag::Image => image_w().min(width.max(1)) * 9 / 16,
        // A rule across a column is a hairline tall; one down a row takes the
        // row's height and asks for none of its own.
        Tag::Divider if node.attr("dir") == Some("vertical") => 0,
        Tag::Divider => DIVIDER,
        // A tile is a button stood on end: icon above label, for a grid of
        // things to open. Compositor chrome for now, like `width`.
        Tag::Button if node.flag("tile") => {
            tile_pad() * 2 + tile_icon() + sc(6) + line_height(fonts, tree, index)
        }
        Tag::Button | Tag::Field | Tag::Item | Tag::Select => control_height(fonts, tree, index),
        // Options take no room in the flow: they float below their dropdown
        // while it is open and are nowhere while it is not.
        Tag::Option => 0,
        Tag::Checkbox => control_height(fonts, tree, index).max(checkbox_size()),
        Tag::Editor => line_height(fonts, tree, index) * EDITOR_LINES + control_pad() * 2,

        // A row is as tall as its tallest child, each measured at the width
        // the row will actually give it: fixed children their natural width,
        // growers an equal share of what is left. Measuring every child at
        // the row's full width undercounted a paragraph in a narrow slot and
        // let it spill out of the row.
        Tag::HStack => {
            let gap = gap_of(node);
            let growers = node.children.iter().filter(|&&c| tree.node(c).flag("grow")).count() as i32;
            let fixed: i32 = node
                .children
                .iter()
                .filter(|&&c| !tree.node(c).flag("grow"))
                .map(|&c| natural_width(fonts, tree, c))
                .sum();
            let gaps = gap * (node.children.len().saturating_sub(1)) as i32;
            let each = if growers > 0 { ((width - fixed - gaps).max(0)) / growers } else { 0 };
            node.children
                .iter()
                .map(|&child| {
                    let slot = if tree.node(child).flag("grow") {
                        each
                    } else {
                        natural_width(fonts, tree, child)
                    };
                    measure(fonts, tree, child, slot)
                })
                .max()
                .unwrap_or(0)
        }

        // A scroll container that grows is a viewport onto its content, and a
        // viewport sized to less than a few rows is a viewport onto nothing.
        // It measures as its content, so a window fits it, but never as less
        // than a few rows, so a browser or a list opens with room to browse
        // in rather than as a slit the size of its first two entries.
        Tag::Scroll if node.flag("grow") => {
            let content = measure_children(fonts, tree, node, width);
            content.max(viewport_min())
        }

        // Everything else stacks vertically: the children's heights plus the
        // gaps between them, plus padding for anything that insets.
        //
        // A scroll container measures as its content, so one whose content fits
        // takes exactly the room it needs and never scrolls. It only scrolls
        // once something gives it less than that, which is what `grow` does.
        _ => {
            let padding = if padded(node) { padding() * 2 } else { 0 };
            let gap = gap_of(node);
            let height = measure_children(fonts, tree, node, width - padding);
            let title = if titled(node.tag) && node.attr("label").is_some() {
                label_height(fonts, tree, index) + gap
            } else {
                0
            };
            height + padding + title
        }
    }
}

/// The height of a node's children stacked, with the gaps between them.
fn measure_children(fonts: &Fonts, tree: &Tree, node: &Node, inner: i32) -> i32 {
    let gap = gap_of(node);
    let mut height = 0;
    let mut position = 0;
    for &child in &node.children {
        // A dialog floats over its window rather than stacking in it, so it
        // adds nothing to the window's height.
        if node.tag == Tag::Window && tree.node(child).tag == Tag::Dialog {
            continue;
        }
        if position > 0 {
            height += gap;
        }
        position += 1;
        height += measure(fonts, tree, child, inner);
    }
    height
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
            let name = self.doc.tree.node(child).attr("region");
            let claim = regions.claim(name);
            match claim {
                // Inset, so a region's content does not sit flush against the
                // edge of the region. The compositor owns the division, so it
                // owns the breathing room too; there is no attribute an
                // agentdesk could set to take it back. Two exceptions, both
                // the compositor's: the background gets none, because what
                // goes there is a wallpaper and a wallpaper reaches the edges;
                // the taskbar gets less above and below, because it is one row
                // of controls in a band sized to hold exactly that.
                Some(rect) => {
                    let inner = match name {
                        Some("background") => rect,
                        Some("taskbar") => Rect::new(
                            rect.x + padding(),
                            rect.y + taskbar_inset(),
                            rect.w - padding() * 2,
                            rect.h - taskbar_inset() * 2,
                        ),
                        _ => rect.inset(padding()),
                    };
                    self.place(child, inner, rect)
                }
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
        let padding = if padded(node) { padding() } else { 0 };
        let mut inner = area.inset(padding);

        // A container that labels itself takes the top of the space before the
        // children divide what is left.
        if titled(tag) && node.attr("label").is_some() {
            let used = label_height(self.fonts, tree, index) + gap_of(node);
            inner = Rect::new(inner.x, inner.y + used, inner.w, inner.h - used);
        }

        let gap = gap_of(node);
        let mut children = node.children.clone();
        let inside = if clipping(tag) {
            clip.intersect(&area).unwrap_or(Rect::new(area.x, area.y, 0, 0))
        } else {
            clip
        };

        // A window's dialogs float over its content, centred, rather than
        // stacking below it. They are pulled out of the flow here and placed
        // after the rest, at a dialog's width and their own height, so an
        // application opens one by adding it to its tree and closes one by
        // leaving it out, and never says where it goes.
        if tag == Tag::Window {
            let dialogs: Vec<usize> = children
                .iter()
                .copied()
                .filter(|&child| tree.node(child).tag == Tag::Dialog)
                .collect();
            children.retain(|child| tree.node(*child).tag != Tag::Dialog);
            self.place_column(&children, inner, gap, inside);
            for dialog in dialogs {
                let w = dialog_w().min(inner.w);
                let h = measure(self.fonts, &self.doc.tree, dialog, w).min(inner.h);
                let rect = Rect::new(inner.x + (inner.w - w) / 2, inner.y + (inner.h - h) / 2, w, h);
                self.place(dialog, rect, inside);
            }
            return;
        }

        // A dropdown's options are not in the flow. Open, they hang below it
        // in a column the dropdown's width, over whatever is there, and above
        // it instead if the window has no room below. Closed, they are
        // nowhere: an empty rectangle, so nothing paints them and nothing
        // hits them.
        if tag == Tag::Select {
            let open = node.flag("open");
            let row = control_height(self.fonts, tree, index);
            let count = children.len() as i32;
            let below = area.y + area.h;
            // Floating over what follows means escaping the container's clip:
            // the options answer to the document's, not to the group or list
            // the box happens to sit in. Clipping them locally silently ate
            // every option past a short container's edge, which the theme
            // dropdown found by living in a group exactly one row tall.
            let float_clip = self.clips[Tree::ROOT];
            let fits_below = below + row * count <= float_clip.y + float_clip.h;
            let top = if fits_below || area.y - row * count < float_clip.y {
                below
            } else {
                area.y - row * count
            };
            for (at, child) in children.into_iter().enumerate() {
                let rect = if open {
                    Rect::new(area.x, top + row * at as i32, area.w, row)
                } else {
                    Rect::new(0, 0, 0, 0)
                };
                self.rects[child] = rect;
                self.clips[child] = if open { float_clip } else { Rect::new(0, 0, 0, 0) };
            }
            return;
        }

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
            let node = self.doc.tree.node(child);
            let width = if node.flag("grow") {
                each
            } else {
                natural_width(self.fonts, &self.doc.tree, child)
            };
            // A one-line control in a taller row keeps its own height and
            // sits in the middle of the row, rather than being stretched to
            // the height of whatever paragraph or stack it shares the row
            // with. Containers and text take the row: they have insides that
            // want the room, or centre themselves when painted.
            let mut slot = Rect::new(x, inner.y, width, inner.h);
            if matches!(node.tag, Tag::Button | Tag::Field | Tag::Select | Tag::Checkbox) {
                let own = measure(self.fonts, &self.doc.tree, child, width).min(inner.h);
                slot = Rect::new(x, inner.y + (inner.h - own) / 2, width, own);
            }
            self.place(child, slot, clip);
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

        // `anchor="end"` is a transcript's request: keep the end in view as
        // content grows, until the human scrolls away from it. The offset map
        // holds AT_END for such a container while it is at its end, so that
        // the next layout, with more content, resolves it to the new end
        // rather than to a number that used to be the end. A concrete offset
        // is what the human's wheel writes, and it means "here, not the end".
        let key = self.doc.key(index).to_owned();
        let follows_end = tree.node(index).attr("anchor") == Some("end");
        let offset = match self.scroll.get(&key).copied() {
            Some(AT_END) => furthest,
            Some(stored) => stored.clamp(0, furthest),
            None if follows_end => furthest,
            None => 0,
        };
        self.scroll.insert(key, if follows_end && offset >= furthest { AT_END } else { offset });
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

/// How tall a whole document wants to be at a given width.
///
/// This is the measure pass run from the root, and it exists for exactly one
/// caller: sizing a window to its content when the first tree arrives. A scroll
/// container measures as its full content here, which is what a clamp against
/// the workspace is for; the container only actually scrolls once layout gives
/// it less room than it asked.
pub fn natural_height(fonts: &Fonts, tree: &Tree, width: i32) -> i32 {
    measure(fonts, tree, Tree::ROOT, width)
}

/// How wide a whole document wants to be so that nothing in it is squeezed.
///
/// The width sibling of [`natural_height`], for the same one caller: sizing a
/// window to its content when the first tree arrives. AWML describes
/// affordances rather than arrangement and has no width to declare, so this
/// is derived: a row wants the sum of what its children want, a column wants
/// the widest of them, a control wants its label and its padding, and text
/// wants its own length up to a cap, because text wraps and a paragraph must
/// not size a window to its longest line. A spacer wants nothing. What comes
/// out is the narrowest width at which every control sits inside its
/// container at its natural size, which is what a window should open at.
pub fn document_width(fonts: &Fonts, tree: &Tree) -> i32 {
    wanted_width(fonts, tree, Tree::ROOT)
}

/// Room at a dropdown's right end for the chevron that says it opens.
fn chevron_w() -> i32 { sc(22) }
/// Whether a document's top-level content asks to fill whatever it is given.
///
/// An application marks a container `grow` to say "this takes the room": a
/// browser's listing, a settings page, a transcript. When that mark is on
/// something directly under the window, the application is saying the window
/// as a whole should have room, and the compositor opens it with some rather
/// than at the size of its first tree. A calculator marks nothing at that
/// level and opens the size of a calculator.
pub fn wants_room(tree: &Tree) -> bool {
    tree.node(Tree::ROOT)
        .children
        .iter()
        .any(|&child| tree.node(child).flag("grow") && tree.node(child).tag != Tag::Dialog)
}

/// The most a run of text asks a window for. Longer text wraps.
fn text_cap() -> i32 { sc(320) }
/// The least a growing scroll container measures as: a few rows of content.
fn viewport_min() -> i32 { sc(180) }

fn wanted_width(fonts: &Fonts, tree: &Tree, index: usize) -> i32 {
    let node = tree.node(index);
    let style = style_at(tree, index);
    let children = |skip_dialogs: bool| {
        node.children
            .iter()
            .copied()
            .filter(move |&child| !(skip_dialogs && tree.node(child).tag == Tag::Dialog))
            .map(|child| wanted_width(fonts, tree, child))
    };
    match node.tag {
        Tag::Text | Tag::Icon => {
            let label = label_of(node);
            if label.trim().is_empty() {
                0
            } else {
                fonts.measure(label, &style).min(text_cap())
            }
        }
        Tag::Divider if node.attr("dir") == Some("vertical") => DIVIDER,
        Tag::Divider => 0,
        Tag::Image | Tag::Button | Tag::Field | Tag::Editor | Tag::Checkbox | Tag::Item | Tag::Select => {
            natural_width(fonts, tree, index)
        }
        Tag::Option => 0,
        Tag::HStack => {
            let gaps = gap_of(node) * (node.children.len().saturating_sub(1)) as i32;
            children(false).sum::<i32>() + gaps
        }
        // A dialog floats and sizes itself, so it asks the window for
        // nothing; the window's own content is what the window is for.
        Tag::Window => {
            let widest = children(true).max().unwrap_or(0);
            widest + if padded(node) { padding() * 2 } else { 0 }
        }
        Tag::Dialog => children(false).max().unwrap_or(0) + padding() * 2,
        _ => children(false).max().unwrap_or(0),
    }
}

/// How wide a node is when it is not being stretched.
fn natural_width(fonts: &Fonts, tree: &Tree, index: usize) -> i32 {
    let node = tree.node(index);
    let style = style_at(tree, index);

    // A compositor-internal sizing hint, in physical pixels, not part of the
    // application catalogue. It exists for chrome the compositor builds about
    // geometry it already knows: the tab rename field takes the width of the
    // tab it replaces instead of jumping to the fallback below.
    if let Some(width) = node.attr("width").and_then(|value| value.parse::<i32>().ok()) {
        return width.max(sc(40));
    }

    match node.tag {
        Tag::Text | Tag::Icon => fonts.measure(label_of(node), &style),
        Tag::Image => image_w(),
        Tag::Divider if node.attr("dir") == Some("vertical") => DIVIDER,
        Tag::Button => {
            // A glyph button is a small square: the shape is the label.
            if node.attr("glyph").is_some() {
                return fonts.line_height(&style) + control_pad() * 2;
            }
            let icon = if node.attr("icon").is_some() && !node.flag("tile") {
                button_icon() + sc(6)
            } else {
                0
            };
            fonts.measure(label_of(node), &style) + button_pad() * 2 + icon
        }
        Tag::Checkbox => checkbox_size() + 8 + fonts.measure(label_of(node), &style),
        Tag::Item | Tag::Option => fonts.measure(label_of(node), &style) + control_pad() * 2,
        // Wide enough for its widest option, so choosing one never changes
        // the width of the row it sits in.
        Tag::Select => {
            let widest = node
                .children
                .iter()
                .map(|&child| fonts.measure(label_of(tree.node(child)), &style))
                .max()
                .unwrap_or(0)
                .max(fonts.measure(node.attr("placeholder").unwrap_or(""), &style));
            widest + control_pad() * 2 + chevron_w()
        }
        // A text entry that is not told to grow is the width of a search box:
        // room for a sentence, which is what one in a row is for.
        Tag::Field | Tag::Editor => sc(260),
        _ => sc(160),
    }
}

/// Break text into lines no wider than `width`, at spaces where possible.
///
/// Greedy, which is what every desktop does and what a reader expects: a line
/// takes as many words as fit. A single word wider than the line is broken
/// between characters rather than overflowing, because a URL or a long number
/// clipped at the edge is text the human cannot read at all. Newlines in the
/// text break lines too, so a message typed with returns keeps them.
pub fn wrap<'a>(fonts: &Fonts, text: &'a str, style: &Style, width: i32) -> Vec<&'a str> {
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        if paragraph.is_empty() {
            lines.push(paragraph);
            continue;
        }
        let mut start = 0;
        let mut last_space: Option<usize> = None;
        let mut at = 0;
        while at < paragraph.len() {
            let next = paragraph[at..]
                .char_indices()
                .nth(1)
                .map(|(offset, _)| at + offset)
                .unwrap_or(paragraph.len());
            if fonts.measure(&paragraph[start..next], style) > width && next > start {
                // Over the edge. Break at the last space if there was one, else
                // right here, but always make progress by at least one char.
                let (line_end, resume) = match last_space {
                    Some(space) if space > start => (space, space + 1),
                    _ if at > start => (at, at),
                    _ => (next, next),
                };
                lines.push(&paragraph[start..line_end]);
                start = resume;
                last_space = None;
                at = start;
                continue;
            }
            if paragraph[at..].starts_with(' ') {
                last_space = Some(at);
            }
            at = next;
        }
        if start < paragraph.len() || lines.is_empty() {
            lines.push(&paragraph[start..]);
        }
    }
    lines
}

/// The first `caret` characters of a string, as a slice.
fn prefix(text: &str, caret: usize) -> &str {
    match text.char_indices().nth(caret) {
        Some((at, _)) => &text[..at],
        None => text,
    }
}

pub fn paint(
    canvas: &mut Canvas,
    fonts: &Fonts,
    images: &Images,
    tree: &Tree,
    layout: &Layout,
    focus: &Focus,
) {
    paint_subtree(canvas, fonts, images, tree, layout, Tree::ROOT, focus);
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
    images: &Images,
    tree: &Tree,
    layout: &Layout,
    index: usize,
    focus: &Focus,
) {
    paint_node(canvas, fonts, images, tree, layout, index, focus);
}

/// A thin indicator beside content that overflows.
///
/// Drawn after a scroll container's children so it sits above them, and only
/// when there is something out of sight: a bar on content that fits would say
/// something untrue.
/// The track and thumb of a scroll container's bar, or `None` when the content
/// fits and there is nothing to indicate.
///
/// One function, used by both painting and hit testing, so the thumb the hand
/// grabs is exactly the thumb the eye sees. Two copies of this arithmetic would
/// drift, and a scrollbar that moves under a click it does not answer to is the
/// kind of bug nobody files and everybody feels.
pub fn scrollbar_geometry(rect: Rect, scroller: &Scroller) -> Option<(Rect, Rect)> {
    if scroller.content <= scroller.viewport || scroller.viewport <= 0 {
        return None;
    }

    let track = Rect::new(
        rect.x + rect.w - scrollbar_w() - sc(2),
        rect.y + sc(2),
        scrollbar_w(),
        rect.h - sc(4),
    );
    let span = (track.h * scroller.viewport / scroller.content).max(sc(24));
    let travel = track.h - span;
    let furthest = (scroller.content - scroller.viewport).max(1);
    let top = track.y + travel * scroller.offset / furthest;
    Some((track, Rect::new(track.x, top, track.w, span)))
}

/// A thin indicator beside content that overflows.
///
/// Drawn after a scroll container's children so it sits above them, and only
/// while [`Focus::scrollbar`] says this container's content is being moved: the
/// bar hugs the content's edge, so earning its keep means leaving when the
/// scrolling stops.
fn paint_scrollbar(canvas: &mut Canvas, layout: &Layout, index: usize, lit: Option<usize>) {
    if lit != Some(index) {
        return;
    }
    let Some(scroller) = layout.scrollers.iter().find(|s| s.node == index) else {
        return;
    };
    let Some((track, thumb)) = scrollbar_geometry(layout.rects[index], scroller) else {
        return;
    };
    // No track drawn, only the thumb. A permanent groove down the side of
    // every scrollable thing is most of what makes a list look heavy.
    canvas.fill_round_rect(thumb, track.w / 2, muted());
}

/// An open dropdown's options: a raised panel hanging off the box, over
/// whatever it covers, painted after everything else in the window.
fn paint_popup(
    canvas: &mut Canvas,
    fonts: &Fonts,
    images: &Images,
    tree: &Tree,
    layout: &Layout,
    select: usize,
    focus: &Focus,
) {
    let options = &tree.node(select).children;
    let Some(&first) = options.first() else { return };
    let Some(&last) = options.last() else { return };
    let top = layout.rects[first];
    let bottom = layout.rects[last];
    let panel = Rect::new(top.x, top.y, top.w, bottom.y + bottom.h - top.y);
    if panel.w <= 0 || panel.h <= 0 {
        return;
    }
    // The options' clip, not the box's: the panel floats with them, past
    // whatever container the box itself is confined to.
    let clip = layout.clips[first];
    canvas.clipped(clip, |canvas| {
        canvas.shadow(panel, radius_control(), sc(10), 120);
        canvas.fill_round_rect(panel, radius_control(), raised());
        canvas.stroke_round_rect(panel, radius_control(), 1, border());
    });
    for &option in options {
        paint_node(canvas, fonts, images, tree, layout, option, focus);
    }
}

fn paint_node(
    canvas: &mut Canvas,
    fonts: &Fonts,
    images: &Images,
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
    let ink = if disabled { muted() } else { color_at(tree, index, text()) };

    // Text sits vertically centred in whatever box it was given.
    let centred = |height: i32| rect.y + (height - fonts.line_height(&style)) / 2;

    // Nothing draws outside the region its containers left it. The clip is
    // computed once during layout and reused here, so what is painted and what
    // is hit-testable cannot disagree.
    let clip = layout.clips[index];

    canvas.clipped(clip, |canvas| match node.tag {
        Tag::Window => canvas.fill_rect(rect, background()),

        // Text sits in the vertical middle of whatever box it was given. In a
        // column the box is exactly its lines and this changes nothing; in a
        // row beside buttons the box is the row's height, and a label at the
        // top of it reads as misplaced next to controls whose text is centred.
        Tag::Text => {
            let step = fonts.line_height(&style);
            let lines = wrap(fonts, label_of(node), &style, rect.w);
            let block = lines.len() as i32 * step;
            let top = rect.y + ((rect.h - block) / 2).max(0);
            for (row, line) in lines.into_iter().enumerate() {
                canvas.draw_text(fonts, line, rect.x, top + row as i32 * step, &style, ink);
            }
        }

        Tag::Divider => canvas.fill_rect(rect, border()),

        // A picture, fitted to cover its rectangle. A source that cannot be
        // loaded leaves the words meant for an agent: the alt text, muted, so a
        // broken path is visible on screen rather than a silent hole.
        Tag::Image => match node.attr("src").and_then(|src| images.get(src, rect.w, rect.h)) {
            Some(bitmap) => canvas.blit(&bitmap, rect.x, rect.y),
            None => {
                canvas.fill_rect(rect, surface());
                canvas.draw_text(
                    fonts,
                    node.attr("alt").unwrap_or("?"),
                    rect.x + control_pad(),
                    rect.y + control_pad(),
                    &style,
                    muted(),
                );
            }
        },

        Tag::Icon => {
            canvas.draw_text(
                fonts,
                node.attr("alt").unwrap_or("?"),
                rect.x,
                centred(rect.h),
                &style,
                muted(),
            );
        }

        Tag::Dialog => {
            canvas.shadow(rect, radius_surface(), 18, 120);
            canvas.fill_round_rect(rect, radius_surface(), raised());
            canvas.stroke_round_rect(rect, radius_surface(), 1, border());
            if let Some(label) = node.attr("label") {
                canvas.draw_text(fonts, label, rect.x + padding(), rect.y + padding(), &style, muted());
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
                canvas.draw_text(fonts, label, rect.x, rect.y, &heading, muted());
                let rule = rect.y + fonts.line_height(&heading) + 3;
                canvas.fill_rect(Rect::new(rect.x, rule, rect.w, 1), border());
            }
        }

        Tag::Button => {
            let emphasis = node.attr("emphasis");
            let fill = match (disabled, pressed, emphasis) {
                (true, _, _) => surface(),
                (_, true, Some("primary")) => accent_deep(),
                (_, true, Some("danger")) => danger_deep(),
                // `self::`, because the local `pressed` above shadows the
                // palette function here.
                (_, true, _) => self::pressed(),
                (_, _, Some("primary")) => accent(),
                (_, _, Some("danger")) => danger(),
                _ => raised(),
            };
            // Lit faintly from above, except when disabled (flat says inert)
            // or pressed (a control being pushed in should not look raised).
            if disabled || pressed {
                canvas.fill_round_rect(rect, radius_control(), fill);
            } else {
                canvas.fill_round_rect_vgrad(rect, radius_control(), lift(fill, 10), fill);
            }
            if focused && !disabled {
                canvas.stroke_round_rect(rect, radius_control(), 2, accent());
            } else if emphasis.is_none() {
                canvas.stroke_round_rect(rect, radius_control(), 1, border());
            }

            let label = label_of(node);
            let icon = node
                .attr("icon")
                .and_then(|name| images.icons.get(name, if node.flag("tile") { tile_icon() } else { button_icon() }));
            // A named glyph the compositor draws, for chrome-shaped buttons
            // whose meaning is a shape rather than a word: the pane's send
            // and stop. Compositor-internal like `width`; never an agent's
            // to see, because the description carries the meaning.
            if let Some(glyph) = node.attr("glyph") {
                draw_button_glyph(canvas, rect, glyph, ink);
            } else if node.flag("tile") {
                // Icon centred above the label. A missing icon leaves the
                // label where it is, so the grid does not jump.
                let top = rect.y + tile_pad();
                if let Some(icon) = icon {
                    canvas.blend_pixmap(icon.as_ref(), rect.x + (rect.w - tile_icon()) / 2, top);
                }
                let x = rect.x + (rect.w - fonts.measure(label, &style)) / 2;
                canvas.clipped(rect.inset(2), |canvas| {
                    canvas.draw_text(fonts, label, x, top + tile_icon() + sc(6), &style, ink);
                });
            } else {
                let icon_w = icon.as_ref().map(|_| button_icon() + sc(6)).unwrap_or(0);
                let width = fonts.measure(label, &style) + icon_w;
                let mut x = rect.x + (rect.w - width) / 2;
                if let Some(icon) = icon {
                    canvas.blend_pixmap(icon.as_ref(), x, rect.y + (rect.h - button_icon()) / 2);
                    x += icon_w;
                }
                canvas.draw_text(fonts, label, x, centred(rect.h), &style, ink);
            }
        }

        Tag::Field | Tag::Editor => {
            canvas.fill_round_rect(rect, radius_control(), background());
            let edge = match (focused, node.flag("invalid")) {
                (_, true) => danger(),
                (true, _) => accent(),
                _ => border(),
            };
            canvas.stroke_round_rect(rect, radius_control(), if focused { 2 } else { 1 }, edge);

            let value = value_of(node);
            let empty = value.is_empty();
            let content = if empty {
                node.attr("placeholder").unwrap_or("").to_owned()
            } else {
                value.clone()
            };
            let color = if empty { muted() } else { ink };

            // The content slides to keep the caret in view. Only while
            // focused: an unfocused control shows its start, and has no caret
            // to follow anyway.
            let (hshift, vshift) = if focused {
                text_scroll(fonts, &value, &style, node.tag, focus.caret, rect)
            } else {
                (0, 0)
            };
            let x = rect.x + control_pad() - hshift;
            let top =
                (if node.tag == Tag::Editor { rect.y + control_pad() } else { centred(rect.h) })
                    - vshift;
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

                if !focused || !focus.caret_visible {
                    return;
                }

                // The caret is the compositor's, not the application's. It is
                // placed against the value rather than against whatever
                // placeholder is standing in for it.
                let (row, column) = caret_position(&value, focus.caret, node.tag);
                let line = value.lines().nth(row).unwrap_or("");
                let caret_x = x + fonts.measure(prefix(line, column), &style);
                inner.fill_rect(Rect::new(caret_x, top + row as i32 * step, sc(2).max(2), step), accent());
            });
        }

        Tag::Checkbox => {
            let box_rect = Rect::new(
                rect.x,
                rect.y + (rect.h - checkbox_size()) / 2,
                checkbox_size(),
                checkbox_size(),
            );
            let checked = node.flag("checked");
            canvas.fill_round_rect(
                box_rect,
                radius_small(),
                if checked && !disabled { accent() } else { background() },
            );
            if !checked || disabled {
                canvas.stroke_round_rect(box_rect, radius_small(), 1, if focused { accent() } else { border() });
            }
            if checked && disabled {
                canvas.fill_round_rect(box_rect.inset(4), 2, muted());
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
                    canvas.stroke_line(ax, ay, bx, by, sc(2).max(2), background());
                }
            }
            canvas.draw_text(
                fonts,
                label_of(node),
                rect.x + checkbox_size() + 8,
                centred(rect.h),
                &style,
                ink,
            );
        }

        // The dropdown as it sits closed or open: a box like a field with the
        // chosen option's words and a chevron. Its options paint separately,
        // last, so they float over what follows.
        Tag::Select => {
            let open = node.flag("open");
            canvas.fill_round_rect(rect, radius_control(), background());
            let edge = if focused || open { accent() } else { border() };
            canvas.stroke_round_rect(rect, radius_control(), if focused || open { 2 } else { 1 }, edge);
            let chosen = node.attr("value").unwrap_or("");
            let shown = node
                .children
                .iter()
                .map(|&child| tree.node(child))
                .find(|option| option.attr("value").unwrap_or(label_of(option)) == chosen)
                .map(label_of)
                .filter(|label| !label.is_empty());
            let (words, color) = match shown {
                Some(label) => (label.to_owned(), ink),
                None => (node.attr("placeholder").unwrap_or("").to_owned(), muted()),
            };
            canvas.clipped(Rect::new(rect.x, rect.y, rect.w - chevron_w(), rect.h), |canvas| {
                canvas.draw_text(fonts, &words, rect.x + control_pad(), centred(rect.h), &style, color);
            });
            // The chevron: down while closed, up while open.
            let cx = (rect.x + rect.w - chevron_w() / 2) as f32;
            let cy = (rect.y + rect.h / 2) as f32;
            let r = sc(3) as f32;
            let t = sc(1).max(1);
            let dy = if open { -r * 0.7 } else { r * 0.7 };
            canvas.stroke_line(cx - r, cy - dy * 0.5, cx, cy + dy * 0.5, t, muted());
            canvas.stroke_line(cx, cy + dy * 0.5, cx + r, cy - dy * 0.5, t, muted());
        }

        Tag::Option => {
            if node.flag("selected") {
                canvas.fill_rect(rect, selected());
            }
            if focused {
                canvas.stroke_rect(rect, 1, accent());
            }
            canvas.draw_text(fonts, label_of(node), rect.x + control_pad(), centred(rect.h), &style, ink);
        }

        Tag::Item => {
            if node.flag("selected") {
                canvas.fill_rect(rect, selected());
            }
            if focused {
                canvas.stroke_rect(rect, 1, accent());
            }
            canvas.draw_text(
                fonts,
                label_of(node),
                rect.x + control_pad(),
                centred(rect.h),
                &style,
                ink,
            );
        }

        // Pure arrangement draws nothing at all.
        Tag::VStack | Tag::HStack | Tag::Scroll => {}
    });

    // Children in order, except that a window's dialogs go last, over the
    // content they float above, with the content dimmed under them so what is
    // inert looks inert; and open dropdowns' options go after even those.
    let node = tree.node(index);
    if node.tag == Tag::Window {
        for &child in &node.children {
            if tree.node(child).tag != Tag::Dialog {
                paint_node(canvas, fonts, images, tree, layout, child, focus);
            }
        }
        let dialogs: Vec<usize> =
            node.children.iter().copied().filter(|&c| tree.node(c).tag == Tag::Dialog).collect();
        if !dialogs.is_empty() {
            canvas.clipped(clip, |canvas| canvas.dim(rect, 96));
        }
        for child in dialogs {
            paint_node(canvas, fonts, images, tree, layout, child, focus);
        }
        for select in tree.open_selects() {
            paint_popup(canvas, fonts, images, tree, layout, select, focus);
        }
    } else if node.tag == Tag::Select {
        // Options are painted by the popup pass, not here.
    } else {
        for &child in &node.children {
            paint_node(canvas, fonts, images, tree, layout, child, focus);
        }
    }

    if node.tag == Tag::Scroll {
        canvas.clipped(clip, |canvas| paint_scrollbar(canvas, layout, index, focus.scrollbar));
    }
}

/// The shapes a button's `glyph` names, drawn as strokes and fills the way
/// the window controls are, because the shipped font cannot be trusted to
/// carry a paper plane. Cheap by construction: a handful of lines per press,
/// nothing rasterized per frame.
fn draw_button_glyph(canvas: &mut Canvas, rect: Rect, glyph: &str, color: Color) {
    let size = sc(12);
    let cx = rect.x + rect.w / 2;
    let cy = rect.y + rect.h / 2;
    match glyph {
        // A paper plane: a dart pointing right, with a folded tail.
        "send" => {
            let l = (cx - size / 2) as f32;
            let r = (cx + size / 2) as f32;
            let t = (cy - size / 2) as f32;
            let b = (cy + size / 2) as f32;
            let mid = cy as f32;
            let tail = l + size as f32 * 0.3;
            let th = sc(2).max(2);
            canvas.stroke_line(l, t, r, mid, th, color);
            canvas.stroke_line(l, b, r, mid, th, color);
            canvas.stroke_line(l, t, tail, mid, th, color);
            canvas.stroke_line(l, b, tail, mid, th, color);
        }
        // The square that means stop, everywhere it means anything.
        "stop" => {
            let side = size - sc(2);
            canvas.fill_round_rect(
                Rect::new(cx - side / 2, cy - side / 2, side, side),
                radius_small(),
                color,
            );
        }
        // A glyph nobody has drawn yet: a hollow box says so on screen
        // rather than a silently blank button.
        _ => canvas.stroke_rect(Rect::new(cx - size / 2, cy - size / 2, size, size), 1, color),
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

/// How far a text control's content is shifted so its caret stays in view:
/// left by the first number, up by the second.
///
/// A value longer than its box used to draw from the start regardless, so the
/// caret walked off the right edge and vanished, and typing appended to text
/// nobody could see. The content slides instead, exactly as far as the caret
/// needs and no further, so a caret at the start shows the start and a caret
/// past the edge drags the text along. Derived from the caret rather than
/// stored, which is what keeps the painter and the click hit test agreeing:
/// both call this, so where text is drawn and where a click lands cannot
/// drift. The vertical half is the same idea for an editor's lines.
pub fn text_scroll(
    fonts: &Fonts,
    value: &str,
    style: &Style,
    tag: Tag,
    caret: usize,
    rect: Rect,
) -> (i32, i32) {
    let (row, column) = caret_position(value, caret, tag);
    let line = value.split('\n').nth(row).unwrap_or("");
    let ahead = fonts.measure(prefix(line, column), style);
    // A margin keeps the caret a step inside the edge, so the next character
    // has somewhere visible to go.
    let inner_w = (rect.w - control_pad() * 2).max(sc(20));
    let hshift = (ahead - inner_w + sc(6)).max(0);

    let vshift = if tag == Tag::Editor {
        let step = fonts.line_height(style).max(1);
        let inner_h = (rect.h - control_pad() * 2).max(step);
        ((row as i32 + 1) * step - inner_h).max(0)
    } else {
        0
    };
    (hshift, vshift)
}

/// Which character of a text control's value a click landed on, row and all.
///
/// The shift is computed from the caret as it stands, because that is the
/// shift the content was painted with: a click lands on what the human sees.
pub fn caret_at_point(
    fonts: &Fonts,
    value: &str,
    style: &Style,
    tag: Tag,
    rect: Rect,
    caret_now: usize,
    at: (i32, i32),
) -> usize {
    let (x, y) = at;
    let (hshift, vshift) = text_scroll(fonts, value, style, tag, caret_now, rect);
    let left = rect.x + control_pad() - hshift;
    if tag != Tag::Editor {
        return caret_from_x(fonts, value, style, left, x);
    }

    let step = fonts.line_height(style).max(1);
    let top = rect.y + control_pad() - vshift;
    let lines: Vec<&str> = value.split('\n').collect();
    let row = (((y - top) / step).max(0) as usize).min(lines.len().saturating_sub(1));
    let column = caret_from_x(fonts, lines[row], style, left, x);
    lines[..row].iter().map(|line| line.chars().count() + 1).sum::<usize>() + column
}
