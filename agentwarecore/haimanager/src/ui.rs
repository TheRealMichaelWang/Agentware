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

pub struct Layout {
    /// One rectangle per node, indexed as the arena is.
    pub rects: Vec<Rect>,
    /// The region each node is visible within, after clipping by every
    /// container above it.
    pub clips: Vec<Rect>,
    pub scrollers: Vec<Scroller>,
    /// The geometry of every spreadsheet in the document. Almost always
    /// empty, and never more than a handful.
    pub grids: Vec<Grid>,
}

impl Layout {
    /// A layout for a client that has not sent anything yet.
    pub fn empty() -> Layout {
        Layout {
            rects: Vec::new(),
            clips: Vec::new(),
            scrollers: Vec::new(),
            grids: Vec::new(),
        }
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

    /// The words under a point: the topmost `text` element with something in
    /// it, where nothing pressable is in the way.
    ///
    /// Separate from [`Layout::hit`] because that answers "what would a press
    /// act on", and static text is not something a press acts on. It is
    /// something a press can start selecting, which is a different question
    /// with a different answer, and folding them together would put a control
    /// and a caption in the same list.
    pub fn text_at(&self, tree: &Tree, x: i32, y: i32) -> Option<usize> {
        (0..tree.nodes.len()).rev().find(|&index| {
            tree.node(index).tag == Tag::Text
                && !tree.node(index).text.trim().is_empty()
                && !tree.blocked(index)
                && self.visible_at(index, x, y)
        })
    }

    /// The control under a point, with a dropdown's floating options taking
    /// precedence over whatever they hang across. Layout does not know what
    /// floats; the tree does, so callers pass it and this checks the floating
    /// nodes before the rest.
    pub fn hit_with_overlays(&self, tree: &Tree, x: i32, y: i32) -> Option<usize> {
        for select in tree.open_overlays().into_iter().rev() {
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
    /// id: acting on a node inside one scrolls it, and the compositor
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
/// The compositor's own state that layout has to read.
///
/// All of it is ephemeral, none of it is in any tree, and it grew from one
/// map to four things, which is where a list of arguments stops being
/// readable. What they have in common is that the application neither sets
/// them nor is told about them.
pub struct Ephemeral<'a> {
    /// Offsets down, one per scroll container.
    pub scroll: &'a mut HashMap<String, i32>,
    /// Offsets across, which only a spreadsheet has.
    pub scroll_x: &'a mut HashMap<String, i32>,
    /// Column widths the human dragged, keyed by the element and the column's
    /// number in it.
    pub columns: &'a HashMap<String, i32>,
    /// Where the last right-press landed, while it still stands. An open menu
    /// hangs from here, which is what makes a context menu appear under the
    /// hand rather than wherever the application put the menu.
    pub context_at: Option<(i32, i32)>,
}

/// Lay out a document that cannot hold a table: the compositor's own chrome.
///
/// The navigation bar and the start menu are built here rather than by any
/// application, so they have neither an offset across nor a dragged column,
/// and saying that once is better than every caller carrying two maps that
/// stay empty.
pub fn layout_chrome(
    fonts: &Fonts,
    doc: &Document,
    frame: &Frame,
    scroll: &mut HashMap<String, i32>,
) -> Layout {
    let mut across = HashMap::new();
    let columns = HashMap::new();
    let mut state = Ephemeral {
        scroll,
        scroll_x: &mut across,
        columns: &columns,
        context_at: None,
    };
    layout(fonts, doc, frame, &mut state)
}

/// Chrome with a point for an open menu to hang from.
///
/// The compositor's own edit menu is a document like any other, and it hangs
/// where the other button was pressed through exactly the machinery an
/// application's context menu uses: one open `menu`, no label, and a point.
pub fn layout_menu_at(fonts: &Fonts, doc: &Document, frame: &Frame, at: (i32, i32)) -> Layout {
    let mut down = HashMap::new();
    let mut across = HashMap::new();
    let columns = HashMap::new();
    let mut state = Ephemeral {
        scroll: &mut down,
        scroll_x: &mut across,
        columns: &columns,
        context_at: Some(at),
    };
    layout(fonts, doc, frame, &mut state)
}

pub fn layout(fonts: &Fonts, doc: &Document, frame: &Frame, state: &mut Ephemeral) -> Layout {
    let count = doc.tree.nodes.len();
    let bounds = match frame {
        Frame::Whole(rect) => *rect,
        Frame::Regions(regions) => regions.background,
    };

    let mut placer = Placer {
        fonts,
        doc,
        scroll: state.scroll,
        scroll_x: state.scroll_x,
        columns: state.columns,
        context_at: state.context_at,
        rects: vec![Rect::new(0, 0, 0, 0); count],
        clips: vec![bounds; count],
        scrollers: Vec::new(),
        grids: Vec::new(),
    };

    match frame {
        Frame::Whole(rect) => placer.place(Tree::ROOT, *rect, *rect),
        Frame::Regions(regions) => placer.place_regions(*regions),
    }

    Layout {
        grids: placer.grids,
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
            let lines = fonts.line_count(label_of(node), &style, width).max(1) as i32;
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
        // A menu is a control the width of its label; its items float and
        // take no room in the flow. One with no label draws nothing and
        // holds a small slot, so that it still has a rectangle an agent can
        // name and a place for its items to hang from.
        Tag::Menu | Tag::Tab => control_height(fonts, tree, index),
        Tag::MenuItem => 0,
        // A strip is a row of tabs plus the band that shows above and below
        // them. Measured off a tab rather than off the strip, because a tab
        // sets its own smaller type and the band is sized to what it holds.
        // A strip nobody sized: the tabs, a cap above and the same below.
        // Given a height instead, as the navigation bar gives one, the cap
        // is worked out from it.
        Tag::Tabs => tabs_row(fonts, tree, index) + tabs_cap_default() * 2,

        // A table is its header plus the rows it was given. It only ever
        // holds the window the application sent, so this is a small number
        // however large the sheet is; `grow` is how one asks for the room to
        // show more, and the row count it reports is what the bar is drawn
        // against.
        // A grid is as tall as there is room for it. It has no children to
        // measure and no natural height of its own: what it shows is however
        // much of the sheet fits, so a window opens with a screenful and
        // `grow` is how one asks for more.
        Tag::Spreadsheet => control_height(fonts, tree, index) * (SHEET_ROWS_MIN + 1),
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
            // The menu bar is a band above the padding rather than a child in
            // it, so a window that has one is that much taller.
            let bar = if has_menus(tree, index) { menu_band_h(fonts) } else { 0 };
            height + padding + title + bar
        }
    }
}

/// The height of a node's children stacked, with the gaps between them.
fn measure_children(fonts: &Fonts, tree: &Tree, node: &Node, inner: i32) -> i32 {
    let gap = gap_of(node);
    let mut height = 0;
    let mut position = 0;
    for &child in &node.children {
        // A dialog floats over its window rather than stacking in it, and a
        // menu stands in the bar across the top of it, so neither adds
        // anything to the height of what is stacked.
        if node.tag == Tag::Window
            && matches!(tree.node(child).tag, Tag::Dialog | Tag::Menu)
        {
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
    /// Offsets across, for the one thing that scrolls that way.
    scroll_x: &'a mut HashMap<String, i32>,
    /// Column widths the human has dragged, in pixels, keyed by the column's
    /// identity. Ephemeral state like scroll and focus: the application
    /// declares what a column should start at and never hears that it moved,
    /// because where a column's edge sits is no more its business than where
    /// its window is.
    columns: &'a HashMap<String, i32>,
    /// Where an open menu should hang from, when a right-press put it there.
    context_at: Option<(i32, i32)>,
    rects: Vec<Rect>,
    clips: Vec<Rect>,
    scrollers: Vec<Scroller>,
    grids: Vec<Grid>,
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

        // Two of a window's children are not in its flow, and neither of them
        // is placed by the application.
        //
        // Its **menus** are its menu bar: one row across the top of the
        // window, under the title bar the compositor draws and above
        // everything the application put inside. An application says it has
        // an Edit menu; where a menu bar goes is not a thing an application
        // gets an opinion about, which is why the parser refuses a `menu`
        // anywhere else.
        //
        // Its **dialogs** float over the content, centred, so an application
        // opens one by adding it to its tree and closes one by leaving it
        // out, and never says where it goes.
        if tag == Tag::Window {
            let menus: Vec<usize> = children
                .iter()
                .copied()
                .filter(|&child| tree.node(child).tag == Tag::Menu)
                .collect();
            let dialogs: Vec<usize> = children
                .iter()
                .copied()
                .filter(|&child| tree.node(child).tag == Tag::Dialog)
                .collect();
            children.retain(|child| !matches!(tree.node(*child).tag, Tag::Dialog | Tag::Menu));

            let mut inner = inner;
            if !menus.is_empty() {
                let band = menu_band(self.fonts, area);
                let row = Rect::new(
                    band.x + menu_lead(),
                    band.y,
                    (band.w - menu_lead() * 2).max(0),
                    band.h,
                );
                self.place_row(&menus, row, menu_gap(), inside);
                // The content starts under the bar, and is padded from there
                // as it would have been from the top of the window.
                let below = Rect::new(area.x, band.y + band.h, area.w, (area.h - band.h).max(0));
                inner = below.inset(padding);
            }

            self.place_column(&children, inner, gap, inside);
            for dialog in dialogs {
                let w = dialog_w().min(inner.w);
                let h = measure(self.fonts, &self.doc.tree, dialog, w).min(inner.h);
                let rect = Rect::new(inner.x + (inner.w - w) / 2, inner.y + (inner.h - h) / 2, w, h);
                self.place(dialog, rect, inside);
            }
            return;
        }

        // A strip of tabs: each at its own width, left to right, the way a
        // row lays out fixed children, inside a band that stands a little
        // taller than they do.
        if tag == Tag::Tabs {
            // The frame between the caps is what the navigation bar handed
            // its buttons, measured off the band rather than off the strip's
            // own box so that the first tab starts where the band does. An
            // application's strip sits inside a padded window and its band
            // already reaches the window's edges; measuring from the box
            // left its tabs a margin the bar does not have.
            let frame = tabs_frame(self.fonts, inner, inside);
            // The row inside it is a tab's own height, not the frame's, and
            // the frame is then a *clip* over it. Sizing the row to the frame
            // instead changes the lighting by a step per row: it looks the
            // same and is not.
            let room = Rect::new(
                frame.x,
                frame.y,
                frame.w,
                tabs_row(self.fonts, &self.doc.tree, index),
            );
            let cut = inside.intersect(&frame).unwrap_or(Rect::new(frame.x, frame.y, 0, 0));
            self.place_row(&children, room, gap, cut);
            return;
        }

        // A dropdown's options and a menu's items are not in the flow. Open,
        // they hang below in a column, over whatever is there, and above
        // instead if the window has no room below. Closed, they are nowhere:
        // an empty rectangle, so nothing paints them and nothing hits them.
        if tag == Tag::Select || tag == Tag::Menu {
            let open = node.flag("open");
            let row = control_height(self.fonts, tree, index);
            // A dropdown's list is the width of its box, because the box is
            // showing one of the same values. A menu's is the width of its
            // widest item, because a menu button says "Edit" and its items
            // say things like "Paste as values".
            let width = if tag == Tag::Menu {
                children
                    .iter()
                    .map(|&child| natural_width(self.fonts, tree, child))
                    .max()
                    .unwrap_or(0)
                    .max(area.w)
            } else {
                area.w
            };
            let count = children.len() as i32;
            // A menu the human opened with the other button hangs from where
            // they pressed; everything else hangs from its own box. Nothing
            // in the tree says which, because the compositor is what saw the
            // press.
            let (anchor_x, above, below) = match self.context_at.filter(|_| tag == Tag::Menu) {
                Some((x, y)) => (x, y, y),
                None => (area.x, area.y, area.y + area.h),
            };
            // Floating over what follows means escaping the container's clip:
            // the options answer to the document's, not to the group or list
            // the box happens to sit in. Clipping them locally silently ate
            // every option past a short container's edge, which the theme
            // dropdown found by living in a group exactly one row tall.
            let float_clip = self.clips[Tree::ROOT];
            let fits_below = below + row * count <= float_clip.y + float_clip.h;
            let top = if fits_below || above - row * count < float_clip.y {
                below
            } else {
                above - row * count
            };
            for (at, child) in children.into_iter().enumerate() {
                let rect = if open {
                    Rect::new(anchor_x, top + row * at as i32, width, row)
                } else {
                    Rect::new(0, 0, 0, 0)
                };
                self.rects[child] = rect;
                self.clips[child] = if open { float_clip } else { Rect::new(0, 0, 0, 0) };
            }
            return;
        }

        if tag == Tag::Spreadsheet {
            self.place_sheet(index, inner, inside);
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
                // A control with a border all the way round is kept inside
                // what is actually visible, as well as inside the row. It
                // only ever differs in a strip of tabs, where the row is a
                // tab's height and the frame clipping it is shorter: a tab
                // with no bottom edge is the whole point of a tab, and a box
                // with no bottom edge is a bug. The navigation bar's rename
                // field is where it showed.
                let bounded = if matches!(node.tag, Tag::Field | Tag::Select) {
                    inner.h.min((clip.y + clip.h - inner.y).max(0))
                } else {
                    inner.h
                };
                let own = measure(self.fonts, &self.doc.tree, child, width).min(bounded);
                slot = Rect::new(x, inner.y + (bounded - own) / 2, width, own);
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

    /// Place a grid: a header that does not scroll down, a row-label gutter
    /// that does not scroll across, and the window of rows the application
    /// sent.
    ///
    /// Two axes with two different owners, which is the whole of the design.
    /// A spreadsheet: a header of column letters, a gutter of row numbers,
    /// and a grid of cells that are not nodes.
    ///
    /// Nothing here is placed in the arena except the element itself. What
    /// comes out instead is one [`Grid`], which is arithmetic: where a cell is
    /// and which cell a point is over are both computed from it, by painting
    /// and by hit testing, from the same numbers. That is what makes a sheet
    /// of a million cells cost the same as a sheet of ten.
    fn place_sheet(&mut self, index: usize, area: Rect, clip: Rect) {
        let doc = self.doc;
        let tree = &doc.tree;
        let node = tree.node(index);

        let source = node.attr("source").unwrap_or_default().to_owned();
        let row_h = control_height(self.fonts, tree, index);
        let number = |name: &str, fallback: u32| {
            node.attr(name).and_then(|value| value.parse::<u32>().ok()).unwrap_or(fallback)
        };
        // Bounded, because these are numbers a client chose and the arithmetic
        // below runs per frame. A sheet wider than this is not a sheet.
        let rows = number("rows", 1).clamp(1, MAX_SHEET_SIDE);
        let columns = number("columns", 1).clamp(1, MAX_SHEET_SIDE);

        // The gutter is as wide as the largest row number it will ever show,
        // so it does not change width as the sheet is scrolled.
        let label_style = Style { size: style_at(tree, index).size * 0.9, ..style_at(tree, index) };
        let gutter_w = self.fonts.measure(&rows.to_string(), &label_style) + control_pad() * 2;
        let width = character_width(self.fonts, tree, index) * DEFAULT_COLUMN_CHARS
            + control_pad() * 2;

        let key = doc.key(index).to_owned();
        let wide: Vec<(u32, i32)> = {
            let mut wide: Vec<(u32, i32)> = self
                .columns
                .iter()
                .filter_map(|(at, &w)| {
                    let column = at.strip_prefix(&format!("{key}#"))?.parse::<u32>().ok()?;
                    (column < columns).then_some((column, w))
                })
                .collect();
            wide.sort_unstable();
            wide
        };

        self.rects[index] = area;
        self.clips[index] = clip;

        let header = Rect::new(area.x + gutter_w, area.y, (area.w - gutter_w).max(0), row_h);
        let body = Rect::new(
            area.x + gutter_w,
            area.y + row_h,
            (area.w - gutter_w).max(0),
            (area.h - row_h).max(0),
        );
        let gutter = Rect::new(area.x, area.y + row_h, gutter_w, body.h);

        let mut grid = Grid {
            node: index,
            source,
            body,
            header,
            gutter,
            row_h,
            rows,
            columns,
            offset: (0, 0),
            width,
            wide,
        };

        // Both offsets are the compositor's, and both are clamped here
        // because only layout knows how big the content turned out to be.
        let (content_w, content_h) = grid.content();
        let across = self
            .scroll_x
            .get(&key)
            .copied()
            .unwrap_or(0)
            .clamp(0, (content_w - body.w).max(0));
        let down = self
            .scroll
            .get(&key)
            .copied()
            .unwrap_or(0)
            .clamp(0, (content_h - body.h).max(0));
        self.scroll_x.insert(key.clone(), across);
        self.scroll.insert(key, down);
        grid.offset = (across, down);

        if content_h > body.h {
            self.scrollers.push(Scroller {
                node: index,
                content: content_h,
                viewport: body.h,
                offset: down,
                horizontal: false,
                permanent: true,
                step: row_h,
            });
        }
        if content_w > body.w {
            self.scrollers.push(Scroller {
                node: index,
                content: content_w,
                viewport: body.w,
                offset: across,
                horizontal: true,
                permanent: false,
                step: width,
            });
        }

        self.grids.push(grid);
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
            horizontal: false,
            permanent: false,
            step: 1,
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

/// The strip above the tabs, and the strip below them before the hairline.
///
/// Both taken off the navigation bar, read out of a capture a pixel at a
/// time, because guessing at this produced the wrong answer twice. Down a
/// column through one of its tabs the bar is: three rows of `raised`, then
/// the tab itself standing on `background`, then two more rows of `raised`,
/// then one row of `border`.
///
/// The important half is what the tabs stand on. They stand on the same
/// colour as the desk behind the bar, not on a raised band, and the raised
/// rows are thin edges above and below. Painting a full band and putting
/// tabs on top of it is what makes them look like buttons lying on a bar
/// rather than tabs cut into one.
/// How much raised shows above the tabs, and so below them too.
///
/// Derived from the height the strip was given rather than fixed, because
/// that is what the navigation bar did: it worked its inset out from the bar
/// it had to fill, so the arithmetic came out differently at every interface
/// scale. Fixed constants agreed with it at 1.0 and at nothing else, which
/// is a whole class of bug this codebase has a rule against and I wrote
/// anyway: every metric is a logical size through `sc`, and a *derived* one
/// stays derived.
///
/// The `+ 12` is unscaled and the size is the small one, both copied from
/// the bar verbatim. They are not what anyone would write now; they are what
/// the bar is, and the bar is not to change.
fn tabs_cap(fonts: &Fonts, height: i32) -> i32 {
    let row = fonts.line_height(&Style { size: 11.0 * scale(), ..Style::default() }) + 12;
    ((height - row) / 2).max(2)
}

/// The natural cap, for a strip nobody has given a height to.
fn tabs_cap_default() -> i32 { sc(3) }

/// How tall the tabs in a strip stand. Measured off a tab rather than off
/// the strip, since a tab sets its own smaller type.
fn tabs_row(fonts: &Fonts, tree: &Tree, strip: usize) -> i32 {
    tree.node(strip)
        .children
        .iter()
        .copied()
        .find(|&child| tree.node(child).tag == Tag::Tab)
        .map(|tab| control_height(fonts, tree, tab))
        .unwrap_or_else(|| control_height(fonts, tree, strip))
}
/// The margin before the first tab and after the last. The band runs to the
/// edges under it; the tabs do not start there.
///
/// Unscaled, like the `+ 12` in [`tabs_cap`], and for the same reason: the
/// navigation bar's frame was inset by a raw ten pixels, so scaling this
/// agreed with it at 1.0 and nowhere else.
fn tabs_lead() -> i32 { 10 }

/// The band a strip paints: the width of whatever it is clipped to rather
/// than of its own box. A row of tabs standing in the middle of a window
/// with a margin either side is a row of buttons.
fn tabs_band(rect: Rect, clip: Rect) -> Rect {
    Rect::new(clip.x, rect.y, clip.w, rect.h)
}

/// The frame the tabs stand in: the band, inset by the cap above and below
/// and the margin at each end.
///
/// One function because the band and the tabs have to agree about where the
/// strip begins. They did not when the band was drawn across the clip and
/// the tabs were laid out from the strip's box, which is a difference only
/// an application ever saw, since the bar's box is the bar.
fn tabs_frame(fonts: &Fonts, rect: Rect, clip: Rect) -> Rect {
    let band = tabs_band(rect, clip);
    let cap = tabs_cap(fonts, band.h);
    Rect::new(
        band.x + tabs_lead(),
        band.y + cap,
        (band.w - tabs_lead() * 2).max(0),
        (band.h - cap * 2).max(0),
    )
}

/// Which slot a tab dragged to `x` belongs in, given every tab's rectangle
/// in display order and which of them is the one being carried.
///
/// The navigation bar's rule, shared with an application's strip because
/// there is no second way to do this that is worth having: count the other
/// tabs whose middle the pointer has passed. It holds no state, so several
/// motions arriving between two repaints all agree, which is what a paced
/// test never catches and mouse speed always does.
pub fn tab_slot(rects: &[Rect], held: usize, x: i32) -> usize {
    rects
        .iter()
        .enumerate()
        .filter(|(at, _)| *at != held)
        .filter(|(_, rect)| x > rect.x + rect.w / 2)
        .count()
}

/// Where the cross that closes a tab is drawn, measured in from the right.
pub fn tab_close_w() -> i32 { sc(20) }

/// A closable tab's label with the room for its cross on the end.
///
/// Four spaces, which is not a measurement anyone would choose: it is what
/// the navigation bar reserved before this element existed, where the room
/// was four literal spaces written into the label. Measured as one string
/// rather than added on afterwards, because a proportional font's advances
/// do not accumulate the same way and the two differ by a pixel.
fn tab_label_room(label: &str, closable: bool) -> String {
    if closable { format!("{label}    ") } else { label.to_owned() }
}

/// The cross at a tab's right end.
///
/// Lives here rather than in the navigation bar because the bar is not the
/// only thing with tabs any more: an application's strip draws the same
/// cross, and one function is what keeps them the same cross.
pub fn draw_tab_close(canvas: &mut Canvas, tab: Rect, active: bool) {
    let cx = (tab.x + tab.w - tab_close_w() / 2 - sc(2)) as f32;
    let cy = (tab.y + tab.h / 2) as f32;
    let r = sc(3) as f32;
    let ink = if active { text() } else { muted() };
    let thickness = sc(1).max(1);
    canvas.stroke_line(cx - r, cy - r, cx + r, cy + r, thickness, ink);
    canvas.stroke_line(cx - r, cy + r, cx + r, cy - r, thickness, ink);
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
    let children = |floating: bool| {
        node.children
            .iter()
            .copied()
            .filter(move |&child| {
                !(floating && matches!(tree.node(child).tag, Tag::Dialog | Tag::Menu))
            })
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
        // A screenful of columns, not the whole sheet: a grid is wider than
        // any window and the bar across the bottom is what says so.
        Tag::Spreadsheet => {
            (character_width(fonts, tree, index) * DEFAULT_COLUMN_CHARS + control_pad() * 2)
                * SHEET_COLUMNS_MIN
        }
        Tag::MenuItem => 0,
        Tag::Menu | Tag::Tab | Tag::Tabs => natural_width(fonts, tree, index),
        Tag::HStack => {
            let gaps = gap_of(node) * (node.children.len().saturating_sub(1)) as i32;
            children(false).sum::<i32>() + gaps
        }
        // A dialog floats and sizes itself, so it asks the window for
        // nothing; the window's own content is what the window is for. The
        // menus are a row rather than one of the stacked children, so they
        // ask for their sum, and the window is at least wide enough to show
        // its own menu bar.
        Tag::Window => {
            let widest = children(true).max().unwrap_or(0);
            let content = widest + if padded(node) { padding() * 2 } else { 0 };
            content.max(menu_bar_width(fonts, tree, index))
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
        Tag::Item | Tag::Option | Tag::MenuItem => {
            fonts.measure(label_of(node), &style) + control_pad() * 2
        }
        Tag::Tab => {
            let room = tab_label_room(label_of(node), node.flag("closable"));
            fonts.measure(&room, &style) + button_pad() * 2
        }
        // A menu with no label is a place for its items to hang from and
        // nothing else, so it asks for almost nothing.
        Tag::Menu => {
            let label = label_of(node);
            if label.is_empty() {
                sc(2)
            } else {
                fonts.measure(label, &style) + button_pad() * 2
            }
        }
        Tag::Tabs => {
            let gap = gap_of(node);
            node.children
                .iter()
                .map(|&child| natural_width(fonts, tree, child))
                .sum::<i32>()
                + gap * (node.children.len().saturating_sub(1)) as i32
        }
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
        Tag::Spreadsheet => wanted_width(fonts, tree, index),
        _ => sc(160),
    }
}
/// The width of one character in a node's own style. A digit's, because a
/// spreadsheet column is mostly numbers and `0` is a fair average anyway.
fn character_width(fonts: &Fonts, tree: &Tree, index: usize) -> i32 {
    fonts.measure("0", &style_at(tree, index)).max(1)
}

/// The narrowest a dragged column may be made.
pub fn column_floor(fonts: &Fonts, tree: &Tree, index: usize) -> i32 {
    character_width(fonts, tree, index) * MIN_COLUMN_CHARS
}

/// Break text into lines no wider than `width`, at spaces where possible.
///
/// Greedy, which is what every desktop does and what a reader expects: a line
/// takes as many words as fit. A single word wider than the line is broken
/// between characters rather than overflowing, because a URL or a long number
/// clipped at the edge is text the human cannot read at all. Newlines in the
/// text break lines too, so a message typed with returns keeps them.
pub fn wrap<'a>(fonts: &Fonts, text: &'a str, style: &Style, width: i32) -> Vec<&'a str> {
    fonts
        .break_lines(text, style, width)
        .iter()
        .map(|&(from, to)| &text[from as usize..to as usize])
        .collect()
}

/// The first `caret` characters of a string, as a slice.
fn prefix(text: &str, caret: usize) -> &str {
    match text.char_indices().nth(caret) {
        Some((at, _)) => &text[..at],
        None => text,
    }
}

/// The selected run, low to high, or `None` when the caret is a point.
fn selection(focus: &Focus) -> Option<(usize, usize)> {
    let anchor = focus.anchor?;
    let (from, to) = (anchor.min(focus.caret), anchor.max(focus.caret));
    (from != to).then_some((from, to))
}

/// Where a text control's content sits: the left edge it starts at, the top
/// of its first line, and how far apart its lines are.
#[derive(Clone, Copy)]
struct TextBox {
    left: i32,
    top: i32,
    step: i32,
}

/// Paint the highlight behind a selected run.
///
/// Behind, so the text stays the text: a selection that covered the words
/// would be a selection nobody could read. Walked a line at a time, because
/// an editor's run can cross newlines and each line starts again at the left.
fn paint_selection(
    canvas: &mut Canvas,
    fonts: &Fonts,
    value: &str,
    style: &Style,
    box_: TextBox,
    (from, to): (usize, usize),
) {
    let TextBox { left, top, step } = box_;
    let mut at = 0;
    for (row, line) in value.split('\n').enumerate() {
        let length = line.chars().count();
        let start = at.max(from);
        let end = (at + length).min(to);
        if start < end {
            let x0 = left + fonts.measure(prefix(line, start - at), style);
            let x1 = left + fonts.measure(prefix(line, end - at), style);
            canvas.fill_rect(Rect::new(x0, top + row as i32 * step, (x1 - x0).max(1), step), selected());
        }
        // The newline the split consumed counts too, so a run that crosses
        // lines lands on the right characters of the next one.
        at += length + 1;
    }
}

pub fn paint(
    canvas: &mut Canvas,
    fonts: &Fonts,
    content: &Content,
    tree: &Tree,
    layout: &Layout,
    focus: &Focus,
) {
    paint_subtree(canvas, fonts, content, tree, layout, Tree::ROOT, focus);
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
    content: &Content,
    tree: &Tree,
    layout: &Layout,
    index: usize,
    focus: &Focus,
) {
    paint_node(canvas, fonts, content, tree, layout, index, focus);

    // The floating options of an open dropdown are painted by a popup pass
    // after everything else, which for a whole document happens in the
    // window's own arm. A region is a branch painted *without* its window,
    // so that pass never runs for it, and the desk's dropdown was a control
    // that opened invisibly: hit-testable, painted nowhere. The same pass
    // runs here for any open select inside this branch; the root is left to
    // the window arm, or every open list would paint twice.
    if index != Tree::ROOT {
        for select in tree.open_overlays() {
            if tree.within(select, index) {
                paint_popup(canvas, fonts, content, tree, layout, select, focus);
            }
        }
    }
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
    let furthest = scroller.furthest().max(1);

    if scroller.horizontal {
        let track = Rect::new(
            rect.x + sc(2),
            rect.y + rect.h - scrollbar_w() - sc(2),
            rect.w - sc(4),
            scrollbar_w(),
        );
        let span = (track.w * scroller.viewport / scroller.content).max(sc(24));
        let travel = track.w - span;
        let left = track.x + travel * scroller.offset / furthest;
        return Some((track, Rect::new(left, track.y, span, track.h)));
    }

    let track = Rect::new(
        rect.x + rect.w - scrollbar_w() - sc(2),
        rect.y + sc(2),
        scrollbar_w(),
        rect.h - sc(4),
    );
    let span = (track.h * scroller.viewport / scroller.content).max(sc(24));
    let travel = track.h - span;
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
    for scroller in layout.scrollers.iter().filter(|s| s.node == index) {
        // A scroll container's bar appears while its content is moving and
        // leaves after. A grid's stays: how much sheet there is below the
        // screen is something a spreadsheet has to say all the time, and it
        // is the only thing saying it.
        if !scroller.permanent && !scroller.horizontal && lit != Some(index) {
            continue;
        }
        let Some((track, thumb)) = scrollbar_geometry(layout.rects[index], scroller) else {
            continue;
        };
        // No track drawn, only the thumb. A permanent groove down the side of
        // every scrollable thing is most of what makes a list look heavy.
        canvas.fill_round_rect(thumb, track.w.min(track.h) / 2, muted());
    }
}

/// An open dropdown's options: a raised panel hanging off the box, over
/// whatever it covers, painted after everything else in the window.
fn paint_popup(
    canvas: &mut Canvas,
    fonts: &Fonts,
    content: &Content,
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
        paint_node(canvas, fonts, content, tree, layout, option, focus);
    }
}

/// The face every pressable surface in the system wears.
///
/// One function because the navigation bar's agentdesk tabs *are* buttons:
/// `nav_markup` writes them as `button` with `emphasis="primary"` on the
/// current one. A `tab` element that painted itself would be the same thing
/// drawn twice, and the two would drift the first time either was touched.
/// What differs between them is only what goes on top of the face: a label,
/// an icon, a glyph.
fn paint_control_face(
    canvas: &mut Canvas,
    rect: Rect,
    emphasis: Option<&str>,
    disabled: bool,
    pressed: bool,
    focused: bool,
) {
    let fill = match (disabled, pressed, emphasis) {
        (true, _, _) => surface(),
        (_, true, Some("primary")) => accent_deep(),
        (_, true, Some("danger")) => danger_deep(),
        // `self::`, because callers have a local `pressed` shadowing the
        // palette function of the same name.
        (_, true, _) => self::pressed(),
        (_, _, Some("primary")) => accent(),
        (_, _, Some("danger")) => danger(),
        _ => raised(),
    };
    // Lit faintly from above, except when disabled (flat says inert) or
    // pressed (a control being pushed in should not look raised).
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
}

/// The area a node's own painting can touch.
///
/// Its rectangle, with two exceptions: a strip of tabs paints its band the
/// full width of whatever it is clipped to rather than of its own box, and a
/// dialog carries a shadow that falls outside it. Getting this wrong does not
/// fail loudly, which is why it is one function: a node that paints outside
/// what this returns loses that part of itself at the edge of a partial
/// repaint, and a partial repaint is exactly the case nobody looks at.
fn footprint(tag: Tag, rect: Rect, clip: Rect) -> Rect {
    match tag {
        Tag::Tabs => tabs_band(rect, clip),
        Tag::Dialog => rect.inset(-DIALOG_SHADOW),
        _ => rect,
    }
}

fn paint_node(
    canvas: &mut Canvas,
    fonts: &Fonts,
    content: &Content,
    tree: &Tree,
    layout: &Layout,
    index: usize,
    focus: &Focus,
) {
    // Nothing in this branch can paint outside this node's clip: layout gives
    // a child its parent's clip or a piece of it, never more. So a clip the
    // canvas has already excluded is a whole branch that can be skipped
    // rather than walked, which is what makes a repaint of the region a
    // dragged window swept cost the window and not the desk.
    if layout.clips[index].intersect(&canvas.clip()).is_none() {
        return;
    }

    let node = tree.node(index);
    let rect = layout.rects[index];
    let disabled = node.disabled();
    let style = style_at(tree, index);
    let focused = focus.node == Some(index);
    let pressed = focus.pressed == Some(index);

    // A pressed control sinks by a pixel. Small enough not to reflow
    // anything, large enough that a still frame shows which control was just
    // acted on.
    //
    // A tab and a menu are exempt, and the exemption lives here rather than
    // in either click path. What they did is visible in what they became:
    // the tab is now the chosen one, the menu is now open. A flash on top of
    // that reads as a button being clicked, which is the one thing they must
    // not look like. The navigation bar's tabs have never flashed, because
    // its clicks never reach `Client::act`; expressing the rule in the paint
    // both paths share is what stops the two ever disagreeing again.
    let shows_press = !matches!(node.tag, Tag::Tab | Tag::Menu);
    let pressed = pressed && shows_press;
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

    // A node whose own box is out of view paints nothing, and settling that
    // here rather than one primitive at a time is the difference between a
    // scrolled-away transcript line costing a rectangle test and costing a
    // line break plus a glyph lookup per character. Said as an empty clip
    // rather than as a branch, because every arm below clips and a clip
    // nothing intersects discards the lot at the top; the alternative is
    // another level of indentation around five hundred lines. Children are
    // still walked: a scroll container's content is taller than the
    // container, so a box out of view can hold something in view.
    let clip = match footprint(node.tag, rect, clip).intersect(&canvas.clip()) {
        Some(_) => clip,
        None => Rect::new(rect.x, rect.y, 0, 0),
    };

    canvas.clipped(clip, |canvas| match node.tag {
        Tag::Window => {
            canvas.fill_rect(rect, background());
            // The menu bar's band, drawn by the window rather than by the
            // menus in it, because a bar is a bar all the way across and not
            // only where a title happens to sit. A raised strip with a
            // hairline under it, which is what every other band on screen is.
            if has_menus(tree, index) {
                let band = menu_band(fonts, rect);
                canvas.fill_rect(band, surface());
                canvas.fill_rect(Rect::new(band.x, band.y + band.h - 1, band.w, 1), border());
            }
        }

        // Text sits in the vertical middle of whatever box it was given. In a
        // column the box is exactly its lines and this changes nothing; in a
        // row beside buttons the box is the row's height, and a label at the
        // top of it reads as misplaced next to controls whose text is centred.
        Tag::Text => {
            let words = label_of(node);
            // The run the human dragged out, behind the words, so they stay
            // readable. Static text is not a control and holds no caret; the
            // compositor tracks a run over it for one reason, which is that
            // an answer worth reading is an answer worth copying.
            if let Some((node, from, to)) = focus.run
                && node == index
            {
                paint_text_run(canvas, fonts, words, &style, rect, (from, to));
            }
            let lines = wrap(fonts, words, &style, rect.w);
            let (top, step) = text_rows(fonts, lines.len(), &style, rect);
            for (row, line) in lines.into_iter().enumerate() {
                canvas.draw_text(fonts, line, rect.x, top + row as i32 * step, &style, ink);
            }
        }

        Tag::Divider => canvas.fill_rect(rect, border()),

        // A picture, fitted to cover its rectangle. A source that cannot be
        // loaded leaves the words meant for an agent: the alt text, muted, so a
        // broken path is visible on screen rather than a silent hole.
        Tag::Image => match node.attr("src").and_then(|src| content.images.get(src, rect.w, rect.h)) {
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
            canvas.shadow(rect, radius_surface(), DIALOG_SHADOW, 120);
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
            paint_control_face(canvas, rect, emphasis, disabled, pressed, focused);

            let label = label_of(node);
            let icon = node
                .attr("icon")
                .and_then(|name| content.images.icons.get(name, if node.flag("tile") { tile_icon() } else { button_icon() }));
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
                // The highlight goes down first, so the words sit on top of
                // it rather than under it.
                if focused && !empty && let Some(range) = selection(focus) {
                    paint_selection(inner, fonts, &value, &style, TextBox { left: x, top, step }, range);
                }

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

        // The grid itself: the surface under it, the frozen gutter down its
        // left, and the row labels in that gutter. The gutter is painted here
        // rather than by each row because it is one strip that happens to
        // hold one label per row, and because a row does not know how wide it
        // is: that is a property of the table's widest label.
        // A grid, drawn from the sheet the element points at. Nothing here
        // is a node: the geometry comes from the one `Grid` layout produced
        // and the values from the stream the application published, so a
        // screenful costs a screenful whatever the sheet is.
        Tag::Spreadsheet => {
            let Some(grid) = layout.grids.iter().find(|grid| grid.node == index) else {
                return;
            };
            let sheet = content.sheets.get(&grid.source);
            let cursor = node.attr("cursor").and_then(sheet::parse);
            let run = node.attr("selection").and_then(sheet::parse_range);
            // What is being typed wins over what was published: the local copy
            // is the one the human is changing, and it becomes the published
            // one when the application echoes it back.
            let editing = focus
                .cell
                .as_ref()
                .filter(|(at, _)| focused && Some(*at) == cursor)
                .map(|(_, value)| value.as_str());
            let ((first_column, first_row), (last_column, last_row)) = grid.visible();
            let numbers = Style { size: style.size * 0.9, ..style };

            canvas.fill_rect(rect, background());

            // The cells, then the lines between them, then what is chosen on
            // top: a run painted behind the words the way a text selection is.
            canvas.clipped(grid.body.intersect(&clip).unwrap_or(grid.body), |canvas| {
                for row in first_row..=last_row {
                    for column in first_column..=last_column {
                        let at = (column, row);
                        let box_ = grid.cell_rect(at);
                        let chosen = run.is_some_and(|((x0, y0), (x1, y1))| {
                            (x0..=x1).contains(&column) && (y0..=y1).contains(&row)
                        });
                        if chosen {
                            canvas.fill_rect(box_, selected());
                        }
                        canvas.fill_rect(
                            Rect::new(box_.x + box_.w - 1, box_.y, 1, box_.h),
                            border(),
                        );
                        canvas.fill_rect(
                            Rect::new(box_.x, box_.y + box_.h - 1, box_.w, 1),
                            border(),
                        );
                        // The cell being typed into is drawn last, over its
                        // neighbours and as wide as its contents.
                        if editing.is_some() && Some(at) == cursor {
                            continue;
                        }
                        let Some(value) = sheet.and_then(|s| s.get(at)) else {
                            continue;
                        };
                        canvas.clipped(box_.inset(1), |cell| {
                            cell.draw_text(
                                fonts,
                                value,
                                box_.x + control_pad(),
                                box_.y + (box_.h - fonts.line_height(&style)) / 2,
                                &style,
                                ink,
                            );
                        });
                    }
                }

                // The cell being typed into, over everything: a box as wide as
                // what is in it, so a value longer than its column can be read
                // and edited rather than trimmed at the column's edge. What
                // every spreadsheet does, and the reason it can be done here
                // is that the box is the compositor's own copy rather than a
                // node with a rectangle somebody else decided.
                if let (Some(value), Some(at)) = (editing, cursor) {
                    let box_ = grid.cell_rect(at);
                    let step = fonts.line_height(&style);
                    let wanted = fonts.measure(value, &style) + control_pad() * 2 + sc(4);
                    let box_ = Rect::new(
                        box_.x,
                        box_.y,
                        box_.w.max(wanted).min((grid.body.x + grid.body.w - box_.x).max(box_.w)),
                        box_.h,
                    );
                    let left = box_.x + control_pad();
                    let top = box_.y + (box_.h - step) / 2;
                    canvas.fill_rect(box_, background());
                    canvas.clipped(box_.inset(1), |cell| {
                        if let Some(range) = selection(focus) {
                            paint_selection(
                                cell,
                                fonts,
                                value,
                                &style,
                                TextBox { left, top, step },
                                range,
                            );
                        }
                        cell.draw_text(fonts, value, left, top, &style, ink);
                        if focus.caret_visible {
                            let column = focus.caret.min(value.chars().count());
                            let caret = left + fonts.measure(prefix(value, column), &style);
                            cell.fill_rect(Rect::new(caret, top, sc(2).max(2), step), accent());
                        }
                    });
                    canvas.stroke_rect(box_, sc(2).max(2), accent());
                } else if let Some(at) = cursor {
                    // The cursor when nothing is being typed: a ring rather
                    // than a fill, so which cell is current and which cells
                    // are chosen stay two visibly different things.
                    canvas.stroke_rect(grid.cell_rect(at), sc(2).max(2), accent());
                }
            });

            // The column letters. Frozen: they do not scroll down, and the
            // cursor's column is lit so a wide sheet still says where you are.
            canvas.clipped(grid.header.intersect(&clip).unwrap_or(grid.header), |canvas| {
                canvas.fill_round_rect_vgrad(grid.header, 0, lift(raised(), 6), raised());
                for column in first_column..=last_column {
                    let box_ = Rect::new(
                        grid.body.x + grid.column_x(column) - grid.offset.0,
                        grid.header.y,
                        grid.column_w(column),
                        grid.header.h,
                    );
                    if cursor.is_some_and(|(at, _)| at == column) {
                        canvas.fill_rect(box_, selected());
                    }
                    canvas.fill_rect(Rect::new(box_.x + box_.w - 1, box_.y, 1, box_.h), border());
                    let label = sheet::column_name(column);
                    let width = fonts.measure(&label, &style);
                    canvas.draw_text(
                        fonts,
                        &label,
                        box_.x + ((box_.w - width) / 2).max(control_pad()),
                        grid.header.y + (grid.header.h - fonts.line_height(&style)) / 2,
                        &style,
                        text(),
                    );
                }
            });

            // The row numbers, frozen the other way.
            canvas.clipped(grid.gutter.intersect(&clip).unwrap_or(grid.gutter), |canvas| {
                canvas.fill_round_rect_vgrad(grid.gutter, 0, lift(surface(), 4), surface());
                for row in first_row..=last_row {
                    let top = grid.body.y + row as i32 * grid.row_h - grid.offset.1;
                    if cursor.is_some_and(|(_, at)| at == row) {
                        canvas.fill_rect(
                            Rect::new(grid.gutter.x, top, grid.gutter.w, grid.row_h),
                            selected(),
                        );
                    }
                    let label = (row + 1).to_string();
                    let width = fonts.measure(&label, &numbers);
                    canvas.draw_text(
                        fonts,
                        &label,
                        grid.gutter.x + (grid.gutter.w - width) / 2,
                        top + (grid.row_h - fonts.line_height(&numbers)) / 2,
                        &numbers,
                        muted(),
                    );
                }
            });

            // The corner where the two frozen strips meet, and the edges.
            canvas.fill_round_rect_vgrad(
                Rect::new(rect.x, rect.y, grid.gutter.w, grid.header.h),
                0,
                lift(raised(), 6),
                raised(),
            );
            canvas.fill_rect(
                Rect::new(rect.x + grid.gutter.w - 1, rect.y, 1, rect.h),
                border(),
            );
            canvas.fill_rect(
                Rect::new(rect.x, rect.y + grid.header.h - 1, rect.w, 1),
                border(),
            );
            canvas.stroke_round_rect(rect, radius_small(), 1, border());
        }

        // A menu: a label you press, or nothing at all. An unlabelled one is
        // a place for its items to hang from, opened by whatever the
        // application decided a right-click means.
        Tag::Menu => {
            let label = label_of(node);
            if label.is_empty() {
                // Nothing to draw: it exists to be named and to hang its
                // items from.
            } else {
                // Words on the band, lit only while its list is showing. The
                // fill used to follow focus as well, so pressing it once left
                // a filled box standing behind the word for as long as the
                // keyboard pointed there, and a menu wearing a box is a
                // button. Focus is a hairline instead: visible to whoever is
                // tabbing through and to nobody else.
                if node.flag("open") {
                    canvas.fill_round_rect(rect, radius_control(), theme().pressed);
                } else if focused && !disabled {
                    canvas.stroke_round_rect(rect, radius_control(), 1, accent());
                }
                let width = fonts.measure(label, &style);
                canvas.draw_text(
                    fonts,
                    label,
                    rect.x + ((rect.w - width) / 2).max(button_pad()),
                    centred(rect.h),
                    &style,
                    ink,
                );
            }
        }

        Tag::MenuItem => {
            if focused {
                canvas.fill_rect(rect, theme().pressed);
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

        // The band, which is the whole of why the navigation bar's tabs look
        // the way they do.
        //
        // Its tabs are unemphasised buttons on a `raised` strip, and an
        // unemphasised button fills with `raised`: the fill disappears into
        // the band and what is left is text. The plus at its end disappears
        // the same way. So a tab strip paints the band first and everything
        // in it comes out looking like the navigation bar without being told
        // to, including whatever an application puts in the strip beside its
        // tabs.
        Tag::Tabs => {
            // The navigation bar's structure, reproduced: a thin raised cap,
            // the tabs standing on the same colour as the desk behind, a
            // thin raised foot, and the hairline under it. The tabs are not
            // on the raised part; that is the whole difference between a tab
            // cut into a bar and a button lying on one.
            //
            // It reaches the document's edges rather than the strip's own
            // rectangle, because the bar the screen paints under the
            // navigation document does, and one that stopped short of the
            // window's sides would be a floating row.
            let band = tabs_band(rect, clip);
            let line = band.y + band.h - 1;
            // Raised everywhere, then the desk's own colour punched into the
            // middle where the tabs stand. What is left raised is a cap
            // above, a foot below, and a margin at each end: exactly what the
            // navigation bar was, where the screen filled the bar and the
            // document's window painted its inset frame over it. The punched
            // rectangle is the same frame the tabs were laid out in, from
            // the same function, so the two cannot part company.
            canvas.fill_rect(band, raised());
            canvas.fill_rect(tabs_frame(fonts, rect, clip), background());
            canvas.fill_rect(Rect::new(band.x, line, band.w, 1), border());
        }

        // A tab is a button that says which one you are on, exactly as the
        // navigation bar's agentdesks are: the chosen one wears the primary
        // emphasis, the rest wear none and vanish into the band behind them.
        // Through the same face, so the two cannot drift apart.
        Tag::Tab => {
            let chosen = node.flag("selected");
            // Neither the press nor the focus ring is drawn. Both are states
            // the navigation bar's tabs never show, and the ring is what
            // makes switching quickly flash: the moment a tab is pressed it
            // takes focus and outlines itself in the accent, and only a
            // frame later, once the application has answered, does it fill.
            // An outline that becomes a fill is a flash. Being the chosen
            // one is the only thing a tab says about itself.
            paint_control_face(canvas, rect, chosen.then_some("primary"), disabled, false, false);

            // The words sit where they sat when the room for the cross was
            // four spaces on the end of them: the padded string is centred
            // and only the label itself is drawn.
            let label = label_of(node);
            let room = tab_label_room(label, node.flag("closable"));
            let width = fonts.measure(&room, &style);
            canvas.clipped(rect.inset(1), |canvas| {
                canvas.draw_text(
                    fonts,
                    label,
                    rect.x + ((rect.w - width) / 2).max(control_pad()),
                    centred(rect.h),
                    &style,
                    ink,
                );
            });
            if node.flag("closable") {
                draw_tab_close(canvas, rect, chosen);
            }
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
                paint_node(canvas, fonts, content, tree, layout, child, focus);
            }
        }
        let dialogs: Vec<usize> =
            node.children.iter().copied().filter(|&c| tree.node(c).tag == Tag::Dialog).collect();
        if !dialogs.is_empty() {
            canvas.clipped(clip, |canvas| canvas.dim(rect, 96));
        }
        for child in dialogs {
            paint_node(canvas, fonts, content, tree, layout, child, focus);
        }
        for select in tree.open_overlays() {
            paint_popup(canvas, fonts, content, tree, layout, select, focus);
        }
    } else if node.tag == Tag::Select {
        // Options are painted by the popup pass, not here.
    } else {
        for &child in &node.children {
            paint_node(canvas, fonts, content, tree, layout, child, focus);
        }
    }

    if matches!(node.tag, Tag::Scroll | Tag::Spreadsheet) {
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
/// Where a text element's wrapped lines are drawn: the top of the first one
/// and the step between them.
///
/// One function, because a run the human drags out has to land on the
/// characters they saw, which means the hit test and the paint have to agree
/// about where those characters are.
fn text_rows(fonts: &Fonts, lines: usize, style: &Style, rect: Rect) -> (i32, i32) {
    let step = fonts.line_height(style);
    let block = lines as i32 * step;
    (rect.y + ((rect.h - block) / 2).max(0), step)
}

/// Which character of a wrapped text element a point lands on.
///
/// The wrapped lines come from the same cache the layout and the paint use,
/// so this costs a lookup rather than breaking the text again.
pub fn text_offset_at(
    fonts: &Fonts,
    text: &str,
    style: &Style,
    rect: Rect,
    at: (i32, i32),
) -> usize {
    let lines = fonts.break_lines(text, style, rect.w);
    if lines.is_empty() {
        return 0;
    }
    let (top, step) = text_rows(fonts, lines.len(), style, rect);
    let row = (((at.1 - top) / step.max(1)).max(0) as usize).min(lines.len() - 1);
    let (from, to) = lines[row];
    let line = &text[from as usize..to as usize];
    // The offset is counted in characters from the start of the whole string,
    // so what is before this line counts too.
    let before = text[..from as usize].chars().count();
    before + caret_from_x(fonts, line, style, rect.x, at.0)
}

/// Paint the highlight behind a run of *wrapped* text.
///
/// The twin of [`paint_selection`], which walks the newlines an editor's value
/// carries. This one walks the lines the compositor chose when it broke the
/// text to the width it was given, because static text has no newlines of its
/// own to walk.
fn paint_text_run(
    canvas: &mut Canvas,
    fonts: &Fonts,
    text: &str,
    style: &Style,
    rect: Rect,
    (from, to): (usize, usize),
) {
    let lines = fonts.break_lines(text, style, rect.w);
    let (top, step) = text_rows(fonts, lines.len(), style, rect);
    for (row, &(start, end)) in lines.iter().enumerate() {
        let line = &text[start as usize..end as usize];
        let before = text[..start as usize].chars().count();
        let length = line.chars().count();
        let head = from.max(before);
        let tail = to.min(before + length);
        if head >= tail {
            continue;
        }
        let x0 = rect.x + fonts.measure(prefix(line, head - before), style);
        let x1 = rect.x + fonts.measure(prefix(line, tail - before), style);
        canvas.fill_rect(
            Rect::new(x0, top + row as i32 * step, (x1 - x0).max(1), step),
            selected(),
        );
    }
}

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
