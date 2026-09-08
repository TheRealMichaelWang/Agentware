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
use crate::sheet::{self, Sheets};

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
/// How tall a window's menu bar is: one control's worth, which is what makes
/// it the same height as the taskbar's row and every other band on screen.
fn menu_band_h(fonts: &Fonts) -> i32 {
    control_h(fonts)
}
/// The margin before the first menu in the bar, so the words do not start
/// flush against the window's edge.
fn menu_lead() -> i32 { sc(6) }
/// The space between one menu title and the next.
fn menu_gap() -> i32 { sc(2) }

/// The band a window's menus stand in: one row across the whole width of it.
///
/// One function, because the row the menus are laid out in and the band that
/// is painted under them have to be the same rectangle, and because the
/// content below starts where this ends.
fn menu_band(fonts: &Fonts, area: Rect) -> Rect {
    Rect::new(area.x, area.y, area.w, menu_band_h(fonts))
}

/// Whether a window has a menu bar, which is whether it has any menus.
fn has_menus(tree: &Tree, index: usize) -> bool {
    tree.node(index).tag == Tag::Window
        && tree.node(index).children.iter().any(|&child| tree.node(child).tag == Tag::Menu)
}

/// How wide a window has to be to show its own menu bar.
fn menu_bar_width(fonts: &Fonts, tree: &Tree, index: usize) -> i32 {
    let menus: Vec<usize> = tree
        .node(index)
        .children
        .iter()
        .copied()
        .filter(|&child| tree.node(child).tag == Tag::Menu)
        .collect();
    if menus.is_empty() {
        return 0;
    }
    let titles: i32 = menus.iter().map(|&menu| natural_width(fonts, tree, menu)).sum();
    titles + menu_gap() * (menus.len() as i32 - 1) + menu_lead() * 2
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
/// How far a dialog's shadow reaches beyond it. Named because `footprint`
/// has to know, and a shadow that outgrew the number it is culled against
/// would be clipped at the edge of a partial repaint and nowhere else.
const DIALOG_SHADOW: i32 = 18;
/// An editor is this many lines tall.
const EDITOR_LINES: i32 = 4;
/// The furthest a spreadsheet may run in either direction.
///
/// A ceiling rather than a limit anyone should reach: these are numbers a
/// client chose, and the geometry is computed from them once a frame.
const MAX_SHEET_SIDE: u32 = 100_000;

/// How far a spreadsheet element says its sheet runs, columns then rows.
///
/// In one place because there are two readers and a disagreement between them
/// is invisible: laying the grid out decides what can be seen and reached, and
/// cutting the sheet decides what is kept, so a shape read differently by the
/// two is either cells held and never drawn or cells drawn and then dropped.
/// An absent or unreadable number is one, which is a sheet with a single cell
/// rather than a sheet of nothing, so an element that declares no shape still
/// has somewhere to put a value.
pub fn sheet_shape(node: &Node) -> (u32, u32) {
    let number = |name: &str| {
        node.attr(name).and_then(|value| value.parse::<u32>().ok()).unwrap_or(1)
    };
    (number("columns").clamp(1, MAX_SHEET_SIDE), number("rows").clamp(1, MAX_SHEET_SIDE))
}
/// How much of a sheet a window opens showing, when nothing else decides.
const SHEET_ROWS_MIN: i32 = 12;
const SHEET_COLUMNS_MIN: i32 = 5;
/// A column with no declared width is this many characters wide, which is
/// about what a spreadsheet gives an untouched column.
const DEFAULT_COLUMN_CHARS: i32 = 10;
/// The narrowest a column may be dragged, in characters, so one cannot be
/// pulled shut and then be impossible to find again.
const MIN_COLUMN_CHARS: i32 = 2;
/// How close to a column's trailing edge a press takes hold of it.
pub fn column_grip() -> i32 { sc(5) }

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
    // A control with no value is empty; its placeholder is a separate thing.
    let value = node.attr("value").unwrap_or_default();
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
            // A tab is chrome-sized. The navigation bar renders its whole
            // document at `sm`, so tabs matching it in every way but their
            // type would still not match. An application that sets a size
            // still wins, because the inherited value is applied below.
            _ if node.tag == Tag::Tab => body_size() * 0.85,
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
    /// Total extent the content wanted, along this scroller's axis.
    pub content: i32,
    /// How much of it is visible.
    pub viewport: i32,
    pub offset: i32,
    /// Across rather than down.
    pub horizontal: bool,
    /// Whether the bar stays on screen instead of fading after the content
    /// stops moving.
    ///
    /// A grid's does. How much sheet there is below the screen is something a
    /// spreadsheet has to say all the time, and its bar is the only thing
    /// saying it; everywhere else a permanent groove down the side of the
    /// content is most of what makes a list look heavy.
    pub permanent: bool,
    /// What one unit of this scroller is worth in pixels: a row's height for
    /// a grid, one pixel for everything else. What a wheel notch moves.
    pub step: i32,
}

impl Scroller {
    /// How far this can travel: zero when everything already fits.
    pub fn furthest(&self) -> i32 {
        (self.content - self.viewport).max(0)
    }
}

/// What there is to draw from, besides the tree: pictures the compositor
/// loaded, and sheets an application published.
///
/// One parameter rather than two because they are the same kind of thing. The
/// tree names a wallpaper and the compositor holds the pixels; the tree names
/// a sheet and the compositor holds the cells. Neither is in the markup, and
/// both are what the markup points at.
pub struct Content<'a> {
    pub images: &'a Images,
    pub sheets: &'a Sheets,
}

/// A spreadsheet's geometry, worked out once by layout and read by everything
/// else: painting, hit testing, the fake cursor, the scrollbars.
///
/// A grid has no nodes, so there are no rectangles in the arena to ask. This
/// is what replaces them, and it is arithmetic rather than storage: a
/// thousand columns cost one record, not a thousand.
pub struct Grid {
    pub node: usize,
    /// The sheet this element points at.
    pub source: String,
    /// Where cells are drawn: below the column header, right of the row
    /// gutter, and clipped to the element.
    pub body: Rect,
    /// The band of column letters across the top.
    pub header: Rect,
    /// The strip of row numbers down the left.
    pub gutter: Rect,
    pub row_h: i32,
    pub rows: u32,
    pub columns: u32,
    /// How far the content is scrolled, across and down, in pixels. Both are
    /// the compositor's: the application publishes cells and is never told
    /// where the window onto them is, exactly as it is never told where its
    /// own window is.
    pub offset: (i32, i32),
    /// What a column is wide when nobody has dragged it.
    pub width: i32,
    /// The ones somebody has, sorted. Almost always empty, which is why the
    /// arithmetic below can afford to walk it.
    pub wide: Vec<(u32, i32)>,
}

impl Grid {
    pub fn column_w(&self, column: u32) -> i32 {
        self.wide
            .iter()
            .find(|(at, _)| *at == column)
            .map_or(self.width, |&(_, w)| w)
    }

    /// How far along the content a column starts, before scrolling.
    pub fn column_x(&self, column: u32) -> i32 {
        let mut x = column as i32 * self.width;
        for &(at, w) in &self.wide {
            if at < column {
                x += w - self.width;
            }
        }
        x
    }

    /// The whole content's size, for the scrollbars.
    pub fn content(&self) -> (i32, i32) {
        (self.column_x(self.columns), self.rows as i32 * self.row_h)
    }

    /// Which column a point along the content lands in.
    fn column_at(&self, x: i32) -> Option<u32> {
        if x < 0 {
            return None;
        }
        // Uniform until proven otherwise, which is the usual case; the walk
        // is only for the columns somebody has dragged.
        if self.wide.is_empty() {
            let column = (x / self.width.max(1)) as u32;
            return (column < self.columns).then_some(column);
        }
        let mut at = 0;
        for column in 0..self.columns {
            let next = at + self.column_w(column);
            if x < next {
                return Some(column);
            }
            at = next;
        }
        None
    }

    /// Where a cell is on screen. Outside the body when it is scrolled away,
    /// which is what the clip is for.
    pub fn cell_rect(&self, at: sheet::Ref) -> Rect {
        Rect::new(
            self.body.x + self.column_x(at.0) - self.offset.0,
            self.body.y + at.1 as i32 * self.row_h - self.offset.1,
            self.column_w(at.0),
            self.row_h,
        )
    }

    /// Which cell a point on screen lands on, or `None` when it is not over
    /// the cells at all.
    pub fn cell_at(&self, x: i32, y: i32) -> Option<sheet::Ref> {
        if !self.body.contains(x, y) {
            return None;
        }
        let column = self.column_at(x - self.body.x + self.offset.0)?;
        let row = (y - self.body.y + self.offset.1) / self.row_h.max(1);
        let row = u32::try_from(row).ok()?;
        (row < self.rows).then_some((column, row))
    }

    /// The rectangle of cells currently on screen, as two corners. What
    /// painting walks, and the only thing that decides how much work a frame
    /// is: a sheet of a million cells and one of ten paint the same screenful.
    pub fn visible(&self) -> (sheet::Ref, sheet::Ref) {
        let first_column = self.column_at(self.offset.0.max(0)).unwrap_or(0);
        let last_column = self
            .column_at(self.offset.0 + self.body.w)
            .unwrap_or(self.columns.saturating_sub(1));
        let first_row = (self.offset.1 / self.row_h.max(1)).max(0) as u32;
        let last_row = ((self.offset.1 + self.body.h) / self.row_h.max(1)) as u32;
        (
            (first_column, first_row.min(self.rows.saturating_sub(1))),
            (last_column, last_row.min(self.rows.saturating_sub(1))),
        )
    }
}

/// Where the compositor believes the caret is, and in which node.
///
/// Both halves are ephemeral state the application never sees. Keeping them out
/// of the tree is what stops a full-tree resend from moving the human's cursor
/// on every keystroke.
#[derive(Default, Clone)]
pub struct Focus {
    pub node: Option<usize>,
    /// Position in characters within the focused control's value.
    pub caret: usize,
    /// Where a selection started, when the human is holding one. The
    /// selected run is everything between this and the caret, in either
    /// order. `None` means nothing is selected and the caret is a point.
    pub anchor: Option<usize>,
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
    /// The cell of a spreadsheet being typed into, and the compositor's copy
    /// of what is in it.
    ///
    /// A cell is not a node, so it cannot be `node`; and what is being typed
    /// is not yet what the application published, so it cannot come from the
    /// sheet. Painting it from here is what makes a keystroke in a grid show
    /// at once, exactly as one in a field does.
    pub cell: Option<(sheet::Ref, String)>,
    /// A run of static text the human has dragged out: which node, and the
    /// two character offsets into it.
    ///
    /// Apart from `node` and `anchor` because it is a different thing. Those
    /// are a control's caret, which is where the next keystroke goes; this is
    /// words on a page that nothing can be typed into, and the only reason
    /// the compositor knows about them is so they can be copied.
    pub run: Option<(usize, usize, usize)>,
}


mod layout;
pub use layout::*;

mod paint;
pub use paint::*;

#[cfg(test)]
mod tests {
    use super::*;

    /// A document laid out into a known rectangle, for asking where things
    /// landed. The scale is fixed at 1 so the numbers below are the logical
    /// ones; `set_scale` is a `OnceLock`, so the first test to run sets it
    /// and the rest agree with it.
    fn placed(source: &str, area: Rect) -> (Document, Layout) {
        set_scale(1.0);
        let fonts = Fonts::load().expect("the faces are compiled in");
        let doc = Document::parse(source, 1).expect("valid markup");
        let mut down = HashMap::new();
        let mut across = HashMap::new();
        let columns = HashMap::new();
        let mut state = Ephemeral {
            scroll: &mut down,
            scroll_x: &mut across,
            columns: &columns,
            context_at: None,
        };
        let layout = layout(&fonts, &doc, &Frame::Whole(area), &mut state);
        (doc, layout)
    }

    const SHEET: &str = r#"<window title="Sheet" pad="none">
        <spreadsheet id="sheet" grow="true" source="book" version="1" rows="1000"
                     columns="26" cursor="A1" description="The grid"/>
      </window>"#;

    /// A grid has no nodes in it. What layout produces is one record of
    /// arithmetic, and everything else — painting, hit testing, the fake
    /// cursor — reads its cells out of that rather than out of the arena.
    #[test]
    fn a_spreadsheet_is_geometry_rather_than_nodes() {
        let area = Rect::new(0, 0, 400, 300);
        let (doc, layout) = placed(SHEET, area);
        let index = doc.index_of("#sheet").expect("the grid");

        // Four elements: the window, the grid, and nothing per cell.
        assert_eq!(doc.tree.nodes.len(), 2, "a grid put nodes in the tree");
        let grid = layout.grids.iter().find(|grid| grid.node == index).expect("one grid");
        assert_eq!(grid.source, "book");
        assert_eq!(grid.rows, 1000);
        assert_eq!(grid.columns, 26);

        // The header is across the top and the gutter down the left, and the
        // cells begin where the two of them stop.
        assert_eq!(grid.header.y, area.y, "the header is not at the top");
        assert_eq!(grid.gutter.x, area.x, "the gutter is not at the left");
        assert_eq!(grid.body.x, grid.gutter.x + grid.gutter.w);
        assert_eq!(grid.body.y, grid.header.y + grid.header.h);

        // A point in the body is the cell it looks like it is in, and a cell
        // is where the hit test says it is: one set of numbers, both ways.
        let a1 = grid.cell_rect((0, 0));
        assert_eq!(grid.cell_at(a1.x + 2, a1.y + 2), Some((0, 0)));
        let c3 = grid.cell_rect((2, 2));
        assert_eq!(grid.cell_at(c3.x + 2, c3.y + 2), Some((2, 2)));
        assert_eq!(c3.x, a1.x + grid.column_w(0) + grid.column_w(1));
        assert_eq!(c3.y, a1.y + grid.row_h * 2);

        // Above the body is the header, not a cell.
        assert_eq!(grid.cell_at(a1.x + 2, grid.header.y + 1), None);

        // Only what fits is ever walked: a thousand rows and a screenful are
        // the same amount of painting.
        let ((_, first), (_, last)) = grid.visible();
        assert_eq!(first, 0);
        assert!(
            last < 40,
            "a screenful of a thousand-row sheet came out as {} rows",
            last + 1
        );
    }

    /// Line breaking carries a running width forward instead of measuring the
    /// whole prefix again at every character. The prefix version was
    /// quadratic in the line's length, which is what made a long transcript
    /// cost a sixth of a second to lay out; this is the check that the sum
    /// still says the same thing.
    #[test]
    fn wrapping_breaks_at_the_last_space_that_fits() {
        set_scale(1.0);
        let fonts = Fonts::load().unwrap();
        let style = Style::default();
        let text = "the quick brown fox jumps over the lazy dog and keeps going a while yet";
        for width in [60, 80, 120, 200, 400] {
            let lines = wrap(&fonts, text, &style, width);
            for line in &lines {
                let drawn = fonts.measure(line, &style);
                assert!(
                    drawn <= width,
                    "a line of {drawn}px ran past {width}px: {line:?}"
                );
            }
            assert_eq!(lines.join(" "), text, "wrapping at {width}px changed the words");
        }

        // A word longer than the width is broken rather than left to run off
        // the side, and the break still makes progress.
        let lines = wrap(&fonts, "unbreakableword", &style, 30);
        assert!(lines.len() > 1, "a word wider than its box was not broken");
        assert_eq!(lines.concat(), "unbreakableword", "breaking lost characters");

        // Blank lines survive: a paragraph break in a message is content.
        assert_eq!(wrap(&fonts, "a\n\nb", &style, 400), vec!["a", "", "b"]);
    }

    /// Not a check, a measurement: what a long transcript costs to lay out
    /// and to paint. Run with `--ignored --nocapture`.
    #[test]
    #[ignore]
    fn bench_pane() {
        set_scale(1.5);
        let fonts = Fonts::load().unwrap();
        for lines in [10usize, 100, 400, 2000] {
            let mut src = String::from("<window pad=\"none\"><vstack><scroll grow=\"true\" anchor=\"end\"><vstack gap=\"sm\">\n");
            for i in 0..lines {
                // Distinct per size, so a bigger run is not warmed by the
                // lines a smaller one already broke.
                src.push_str(&format!("<text role=\"caption\" color=\"muted\">line {lines}-{i}: the agent said something of about this length, which is what a telemetry line looks like</text>\n"));
            }
            src.push_str("</vstack></scroll></vstack></window>");
            let doc = Document::parse(&src, 1).unwrap();
            let mut down = HashMap::new();
            let mut across = HashMap::new();
            let columns = HashMap::new();
            let area = Rect::new(0, 0, 500, 1300);
            // Cold and warm are different questions. Cold is the first time
            // a line is seen, which happens once per line ever; warm is every
            // re-render after it, which is once a second while a turn runs.
            let sheets = Sheets::default();
            let mut lay = || {
                let mut state = Ephemeral {
                    scroll: &mut down,
                    scroll_x: &mut across,
                    columns: &columns,
                            context_at: None,
                };
                layout(&fonts, &doc, &Frame::Whole(area), &mut state)
            };
            let t0 = std::time::Instant::now();
            let _ = lay();
            let cold = t0.elapsed();
            let t0 = std::time::Instant::now();
            let layout = lay();
            let laid = t0.elapsed();
            let images = crate::images::Images::new(0);
            let mut canvas = Canvas::new(500, 1300);
            let focus = Focus::default();
            let t1 = std::time::Instant::now();
            for _ in 0..10 {
                paint(&mut canvas, &fonts, &Content { images: &images, sheets: &sheets }, &doc.tree, &layout, &focus);
            }
            let painted = t1.elapsed() / 10;
            eprintln!(
                "{lines} lines ({} nodes): layout cold {:.2}ms warm {:.2}ms, paint {:.2}ms",
                doc.tree.nodes.len(),
                cold.as_secs_f32() * 1000.0,
                laid.as_secs_f32() * 1000.0,
                painted.as_secs_f32() * 1000.0
            );
        }
    }
}
