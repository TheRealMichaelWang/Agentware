//! Everything that is on the display at once: workspaces, their regions, the
//! application windows inside them, and the navigation bar above all of them.
//!
//! ## Why the compositor owns the division
//!
//! A workspace is four regions. Three belong to the agentdesk and one belongs to
//! application processes, and the agentdesk declares which of its top-level
//! nodes goes in which. That declaration is honoured only on a desk connection,
//! and the check is not an attribute anywhere: an application's tree is laid out
//! into the rectangle its window occupies, and nothing on that path ever reads
//! the word `region`. An application can write it and it will mean nothing.
//!
//! The same argument runs the other way. `apps` is not a region an agentdesk may
//! claim, because claiming it would mean drawing over its own applications, and
//! the human would have no way to reach past what the workspace put there.
//!
//! ## The navigation bar belongs to no workspace
//!
//! It is drawn here, by the compositor, because it is how the human leaves a
//! workspace. If a workspace drew it, an agent working in that workspace could
//! be one wedged process away from trapping the human inside it.
//!
//! It is still AWML, built here and run through the same parser, layout engine
//! and painter as everything else. That is not decoration: it means the bar
//! cannot drift from the style of what it sits above, and it means the code that
//! resolves a click to a node is one implementation rather than two.
//!
//! ## The start menu is chrome
//!
//! The Agentware mark at the left end of the taskbar opens a panel centred over
//! the workspace: a prompt that becomes a new agentdesk, and a grid of every
//! installed application. It is drawn here rather than by the agentdesk for the
//! reasons the dock and the navigation bar are: it needs the icons only the
//! compositor holds, it must vanish on a click anywhere else, which only the
//! compositor sees, and it must work when the workspace under it does not.
//! What it asks for goes to PID 1 the way the stop button and a tab's close do:
//! `create-desk` with the prompt, `open-app` into the workspace on screen. An
//! agent still opens applications only through its agentdesk; this is the
//! human's door, and it is the same door for every workspace.
//!
//! ## The stop button does not pass through the agentdesk
//!
//! Clicking it sends `interrupt` to the supervisor. The agentdesk is the process
//! most likely to be busy at exactly the moment the human wants to stop
//! something: it is streaming telemetry, managing apps, and rendering a growing
//! transcript. If it drew the button and received its click, a wedged agentdesk
//! would mean a human who cannot stop a running agent, and absolute human
//! authority would hold only while everything else was healthy.
//!
//! ## Input arbitration
//!
//! While an agent is running a turn, the human cannot click into that
//! workspace's `apps` region. Two processes driving the same cursor and the same
//! tree is the failure this prevents. The freeze is scoped to that one region
//! and specifically not to the screen: the navigation bar, the stop button, the
//! chat input and the transcript all stay live, or the freeze would be a trap
//! rather than a safety measure.

use std::collections::HashMap;
use std::os::fd::{OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use awproto::{agent, display};

use crate::client::{Client, Kind, Progress};
use crate::clipboard::Clipboard;
use crate::cursor;
use crate::document::Document;
use crate::editmenu::{EditMenu, Offer, Target};
use crate::images::Images;
use crate::sheet;
use crate::startmenu::{AGENTWARE_ICON, AGENTWARE_SVG, StartMenu, StartOutcome};
use crate::text::{Edit, Editing, MultiPress};
use crate::trail::Trail;
use crate::input::{Button, Event, Key};
use crate::paint::font::{Family, Fonts, Style};
use crate::paint::{Canvas, Rect};
use crate::ui::{self, Focus, Frame, Layout, Regions};

/// Height of the navigation bar, which sits above every workspace.
fn nav_height() -> i32 { ui::sc(32) }
/// Height of the agentdesk's taskbar: the full-width band along the bottom of
/// a workspace, sized to hold one row of controls with the taskbar inset above
/// and below. It was parked at zero height for a while, when all it held was a
/// duplicate of the dock. Now it holds the clock, the launcher and the way to
/// start a new workspace, and the dock sits in the middle of it, so one band
/// does what two used to.
fn taskbar_height(fonts: &Fonts) -> i32 { ui::control_h(fonts) + ui::taskbar_inset() * 2 }
/// Width of the handle left behind when the pane is collapsed.
fn pane_handle_w() -> i32 { ui::sc(12) }
/// Height of the title bar the compositor draws around an application window.
fn window_title_h() -> i32 { ui::sc(24) }
/// How far a window's painting reaches beyond its own rectangle: its shadow,
/// at the largest blur a focused one wears.
fn window_reach() -> i32 { ui::sc(22) }
/// The square icon at the left end of a title bar.
fn title_icon() -> i32 { ui::sc(14) }
/// How far each successive window is offset, so none opens exactly on another.
fn cascade() -> i32 { ui::sc(26) }
fn window_margin() -> i32 { ui::sc(14) }
/// Width of one title bar button. The three sit flush at the bar's right end,
/// each the full height of the bar, which makes them targets rather than dots.
fn title_button_w() -> i32 { ui::sc(34) }
/// One square tile per open window, centred in the taskbar band. An icon with
/// a running dot beneath it, so it fits the band's height with a little air.
fn dock_tile() -> i32 { ui::sc(32) }
/// The icon inside a tile, leaving room below for the running dot.
fn dock_icon() -> i32 { ui::sc(20) }
/// The gap between tiles.
fn dock_gap() -> i32 { ui::sc(4) }
/// The start button at the left end of the taskbar band, and its mark.
fn start_button_w() -> i32 { ui::sc(40) }
fn start_icon() -> i32 { ui::sc(22) }

/// What a point in a title bar means.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Title {
    Close,
    Minimize,
    Maximize,
    /// Anywhere else on the bar: pick the window up.
    Drag,
}

/// A window being moved or resized by the human.
///
/// Only the human: there is no resize in the agent's action vocabulary, and
/// arrangement is something the compositor does *for* an agent, never something
/// an agent asks for.
struct Drag {
    fd: RawFd,
    mode: DragMode,
}

#[derive(Clone, Copy)]
enum DragMode {
    /// Carried by the title bar. Remembers where in the window the pointer took
    /// hold, so the window does not jump to centre itself under it.
    Move { grab: (i32, i32) },
    /// Pulled by an edge. Either axis, or both from the corner.
    Resize { right: bool, bottom: bool },
}

/// A tab held down, not yet known to be a click or a drag.
struct TabDrag {
    /// The workspace id, which survives the reordering the drag itself causes.
    id: u32,
    start_x: i32,
    /// Where inside the tab the pointer took hold, so the ghost rides under
    /// the hand instead of snapping its corner to it.
    grab_dx: i32,
    /// The pointer's latest x, for drawing the ghost.
    at_x: i32,
    moved: bool,
}

/// The width of the close glyph zone at a tab's right end.
/// How close to an edge a press counts as taking hold of it.
fn resize_band() -> i32 { ui::sc(7) }
/// Smaller than this and a window is all chrome.
fn min_window() -> (i32, i32) { (ui::sc(320), ui::sc(200)) }

/// How long the conversation pane takes to fold away or return.
const PANE_FOLD: Duration = Duration::from_millis(200);

/// Half a caret blink: lit for this long, dark for this long.
///
/// Restarted by every keystroke, so the caret is solid while someone is typing
/// and only blinks while the field is waiting.
const BLINK: Duration = Duration::from_millis(530);

// There is no constant here for the time between an agent's keystrokes, and
// the reason is worth writing down where one used to be. Typing is still one
// event per character, which is the guarantee: an application receives the
// seventeen events a person typing would have produced and never a value that
// appeared in one step. What has gone is the *pause* between them. They are
// synthesized together, when the intent arrives, because an agent waiting out
// seventeen pauses before it is told anything is an agent whose model cannot
// think about the next step until the animation of the last one has finished.


/// Where the keyboard is pointed.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Surface {
    Nav,
    Desk,
    App(RawFd),
    /// The start menu, while it is open.
    Start,
}

/// One application window.
///
/// The rectangle is the window's own, not derived from a slot, because the human
/// can move and resize it and neither the compositor nor the application gets to
/// put it back.
struct Window {
    fd: RawFd,
    rect: Rect,
    /// Where it returns to when it stops being maximized.
    restored: Rect,
    minimized: bool,
    maximized: bool,
}

struct Workspace {
    id: u32,
    desk: Option<RawFd>,
    /// Front to back is last to first: the final entry is on top.
    windows: Vec<Window>,
    /// The agent running a turn here, if there is one. Its presence is what
    /// freezes the `apps` region.
    agent: Option<RawFd>,
    /// How many windows have ever opened here, so the next one cascades off the
    /// last rather than landing exactly on it.
    opened: usize,
    /// Whether the conversation pane is folded away.
    ///
    /// Compositor state, not the agentdesk's, for the same reason the stop
    /// button is: the pane is most of the screen, and a wedged workspace must
    /// not be able to keep it.
    pane_collapsed: bool,
    /// What the human renamed this agentdesk to, if they have. The default is
    /// derived from the id. Chrome state, so it lives here: the agentdesk
    /// process does not know its own tab's name any more than an app knows
    /// where its window is.
    name: Option<String>,
    /// How far along the fold is, 0 fully open to 1 fully away.
    ///
    /// Kept separate from the target so the pane travels rather than teleports.
    /// A third of the screen appearing in one frame reads as a glitch even when
    /// it is exactly what was asked for; the same change over a fifth of a
    /// second reads as a thing moving.
    pane_t: f32,
}

pub struct Screen {
    clients: Vec<Client>,
    workspaces: Vec<Workspace>,
    current: usize,
    focus: Surface,

    nav: Option<Document>,
    nav_layout: Layout,
    nav_scroll: HashMap<String, i32>,

    bounds: Rect,
    /// A diagnostic overlay, off by default. Screenshots are how graphics get
    /// checked, and a still frame cannot otherwise say which connection is
    /// holding which version.
    pub debug: bool,
    /// Pictures: app icons, parsed and rasterized when an app attaches so
    /// drawing them is a lookup, and the images documents point at, fitted
    /// and cached on first paint.
    images: Images,
    /// The taskbar band's height, fixed at startup from the font metrics the
    /// band is sized around.
    taskbar_h: i32,
    /// Requests for the supervisor. Collected here rather than sent from here
    /// because the control connection is owned by the event loop, and a screen
    /// that could write to PID 1 from inside a click handler is a screen that
    /// can block inside one.
    requests: Vec<Vec<String>>,
    notes: Vec<String>,
    drag: Option<Drag>,
    /// The client whose scrollbar thumb is being dragged, so pointer motion
    /// keeps reaching it even when the pointer leaves the bar.
    scroll_drag: Option<RawFd>,
    /// The client whose slider thumb the hand is holding, so motion keeps
    /// reaching it once the pointer wanders off the track.
    slider_drag: Option<RawFd>,
    /// The client the pointer is dragging a text selection out of, on the
    /// same terms: a selection that stopped extending the moment the pointer
    /// left the field would be a selection nobody could finish.
    text_drag: Option<RawFd>,
    /// The client whose column edge is being dragged.
    column_drag: Option<RawFd>,
    /// The client a run of cells is being dragged out of.
    cell_drag: Option<RawFd>,
    /// The client whose tab is being carried along its strip.
    tabs_drag: Option<RawFd>,
    /// What the human last copied. One per machine, held here because the
    /// compositor is the only process that sees both ends of a copy: it owns
    /// the keyboard the chord arrives on and the text being edited.
    clipboard: Clipboard,
    /// The compositor's cut/copy/paste menu, while the other button has one
    /// open. Chrome, like the start menu: no application is told it exists
    /// and no agent can reach it.
    edit: Option<EditMenu>,
    /// A tab held by the pointer. Whether it becomes a drag or a click is
    /// decided by whether it moves before it is released.
    tab_drag: Option<TabDrag>,
    /// The agentdesk tab being renamed: its workspace index, the text so far,
    /// and the width of the tab it replaced, so entering the editor does not
    /// change the tab's size under the click that opened it.
    /// The agentdesk tab being renamed: which workspace, the text box, and
    /// the width the tab had. The text box is the same one an application's
    /// field is, so this one can be selected in, copied out of and pasted
    /// into as well; it used to be a `String` with `push` and `pop`.
    renaming: Option<(usize, Editing, i32)>,
    /// Whether a run is being dragged out of the rename field right now.
    rename_drag: bool,
    /// Presses counted for the chrome's own text boxes, so a double click in
    /// the rename field takes a word there as it does everywhere else.
    presses: MultiPress,
    /// The start menu, while it is open.
    start: Option<StartMenu>,

    /// The agent's cursor and the queue of places it still has to be seen.
    /// It draws and decides nothing; see [`crate::trail`].
    trail: Trail,
    /// Where the agent's pointer is, and whose workspace it is in. Kept after a
    /// flight lands, so the human can see what was just touched.
    agent_cursor: Option<(u32, i32, i32)>,
    /// The overlay wants restamping: a pointer moved without the scene changing.
    overlay_dirty: bool,
    /// The region a window drag disturbed this pass: where the window was and
    /// where it now is, shadows included. `None` means no drag damage.
    drag_damage: Option<Rect>,
    /// What the pointer was last over, at the granularity that changes pixels:
    /// its shape, and any hover-lit window control. Motion that does not change
    /// this signature repaints nothing but the cursor patch.
    hover: (u8, Option<(RawFd, u8)>),
    /// When the last animation frame was advanced, for time-based motion.
    last_frame: Instant,
    /// When the caret last had a reason to be visible: a keystroke, a click
    /// into text, or an agent typing. Phase is measured from here.
    blink_epoch: Instant,
    /// The phase most recently painted, so a flip is worth exactly one frame.
    blink_shown: bool,
    /// Whether anything was mid-animation on the previous pass.
    ///
    /// Only used to produce one final frame after the last one finishes. Without
    /// it a pressed control stays pressed on screen forever, because the thing
    /// that would have repainted it is the animation that has just stopped.
    animated: bool,
}

impl Screen {
    pub fn new(bounds: Rect, fonts: &Fonts) -> Screen {
        let mut images = Images::new(ui::background());
        images.icons.install(AGENTWARE_ICON, AGENTWARE_SVG);
        images.icons.prepare(AGENTWARE_ICON, &[start_icon()]);
        Screen {
            clients: Vec::new(),
            workspaces: Vec::new(),
            current: 0,
            focus: Surface::Desk,
            nav: None,
            nav_layout: Layout::empty(),
            nav_scroll: HashMap::new(),
            bounds,
            debug: false,
            images,
            taskbar_h: taskbar_height(fonts),
            requests: Vec::new(),
            notes: Vec::new(),
            drag: None,
            scroll_drag: None,
            slider_drag: None,
            text_drag: None,
            column_drag: None,
            cell_drag: None,
            tabs_drag: None,
            clipboard: Clipboard::default(),
            edit: None,
            tab_drag: None,
            renaming: None,
            rename_drag: false,
            presses: MultiPress::default(),
            start: None,
            trail: Trail::new(),
            agent_cursor: None,
            overlay_dirty: false,
            drag_damage: None,
            hover: (0, None),
            last_frame: Instant::now(),
            blink_epoch: Instant::now(),
            blink_shown: true,
            animated: false,
        }
    }

    // ---- geometry ----------------------------------------------------------

    fn nav_rect(&self) -> Rect {
        Rect::new(self.bounds.x, self.bounds.y, self.bounds.w, nav_height())
    }

    fn workspace_area(&self) -> Rect {
        Rect::new(
            self.bounds.x,
            self.bounds.y + nav_height(),
            self.bounds.w,
            self.bounds.h - nav_height(),
        )
    }

    fn regions(&self) -> Regions {
        self.regions_for(self.current)
    }

    fn regions_for(&self, at: usize) -> Regions {
        let fold = self
            .workspaces
            .get(at)
            .map(|workspace| workspace.pane_t)
            .unwrap_or(0.0);
        let area = self.workspace_area();
        // Proportional, with bounds. A fixed width was a third of a small
        // screen and a sliver of a large one; a conversation column wants to be
        // a modest sixth of either.
        let full = (area.w * 17 / 100).clamp(ui::sc(240), ui::sc(320));
        // Smoothstepped, so the pane leaves and arrives gently instead of at
        // full speed. The eased value drives the actual layout: the windows and
        // the desk are genuinely mid-way, not sliding pictures of themselves.
        let open = 1.0 - fold;
        let eased = open * open * (3.0 - 2.0 * open);
        let pane = (full as f32 * eased).round() as i32;
        Regions::carve(area, self.taskbar_h, pane)
    }

    /// Advance every pane that is not where it is meant to be.
    fn advance_panes(&mut self, fonts: &Fonts) -> bool {
        let now = Instant::now();
        // Clamped, because the clock keeps running while nothing animates and
        // the first frame after an idle hour must not teleport the pane.
        let dt = now.duration_since(self.last_frame).as_secs_f32().min(0.05);
        self.last_frame = now;

        let mut moved = false;
        for workspace in &mut self.workspaces {
            let target = if workspace.pane_collapsed { 1.0 } else { 0.0 };
            if (workspace.pane_t - target).abs() > f32::EPSILON {
                let step = dt / PANE_FOLD.as_secs_f32();
                workspace.pane_t = if workspace.pane_t < target {
                    (workspace.pane_t + step).min(target)
                } else {
                    (workspace.pane_t - step).max(target)
                };
                moved = true;
            }
        }

        if moved {
            // The layout follows the motion for real: the desk's pane narrows,
            // and a maximized window widens into the room it frees.
            self.reframe(fonts);
        }
        moved
    }

    /// Which client keystrokes currently land in.
    fn keyboard_client(&self) -> Option<RawFd> {
        match self.focus {
            Surface::App(fd) => Some(fd),
            Surface::Desk => self.workspaces.get(self.current).and_then(|w| w.desk),
            Surface::Nav | Surface::Start => None,
        }
    }

    /// True while the caret is in the lit half of its blink.
    fn caret_phase(&self) -> bool {
        (self.blink_epoch.elapsed().as_millis() / BLINK.as_millis()).is_multiple_of(2)
    }

    /// How long until the caret next changes phase, if one is blinking.
    ///
    /// This is the event loop's wake-up, so an idle desk with no caret sleeps
    /// exactly as it did before: no caret, no deadline, no frames.
    pub fn until_blink(&self) -> Option<Duration> {
        let in_client = self
            .keyboard_client()
            .and_then(|fd| self.client(fd))
            .is_some_and(Client::focused_text);
        if !in_client && self.renaming.is_none() && self.start.is_none() {
            return None;
        }
        let period = BLINK.as_millis();
        let into = self.blink_epoch.elapsed().as_millis() % period;
        Some(Duration::from_millis((period - into) as u64 + 1))
    }

    fn panes_moving(&self) -> bool {
        self.workspaces.iter().any(|workspace| {
            let target = if workspace.pane_collapsed { 1.0 } else { 0.0 };
            (workspace.pane_t - target).abs() > f32::EPSILON
        })
    }

    /// The grip that folds the conversation pane away, on its leading edge.
    fn pane_handle(&self, at: usize) -> Rect {
        let regions = self.regions_for(at);
        let body = regions.pane;
        let x = if body.w == 0 {
            self.workspace_area().x + self.workspace_area().w - pane_handle_w()
        } else {
            body.x - pane_handle_w() / 2
        };
        Rect::new(x, body.y + body.h / 2 - 26, pane_handle_w(), 52)
    }

    pub fn toggle_pane(&mut self, fonts: &Fonts) -> bool {
        let at = self.current;
        let Some(workspace) = self.workspaces.get_mut(at) else { return false };
        // Only the target flips; the motion belongs to the ticks. Toggling
        // mid-flight turns the pane around from wherever it currently is.
        workspace.pane_collapsed = !workspace.pane_collapsed;
        let _ = fonts;
        true
    }

    /// Where windows may go: the apps region. The dock lives in the taskbar
    /// band below it, so nothing here changes size when a window opens.
    fn window_area(&self, at: usize) -> Rect {
        self.regions_for(at).apps
    }

    /// Where a window opens, before the human has an opinion about it.
    ///
    /// A cascade rather than a tiling because overlap is the point: a window
    /// that is partly covered is the case that makes "is this node reachable"
    /// a real question rather than a formality, and that question is what an
    /// agent's intent is checked against.
    ///
    /// The size is provisional: the smallest a resize would allow, because at
    /// this moment the client has not sent a tree and there is nothing to
    /// measure. [`Self::fit_window`] grows it to its content when the first
    /// tree arrives, usually before this rectangle is ever painted.
    fn opening_rect(&self, at: usize, opened: usize) -> Rect {
        let area = self.window_area(at);
        let (min_w, min_h) = min_window();
        // Wraps after a few, so the tenth window is not off the bottom corner.
        let step = cascade() * (opened % 5) as i32;
        Rect::new(area.x + window_margin() + step, area.y + window_margin() + step, min_w, min_h)
    }

    /// Size a window to what its content asks for.
    ///
    /// Runs once per client, when its first tree arrives. The width is what
    /// the tree wants so that every control sits inside its container at its
    /// natural size, never narrower than a resize would allow and never wider
    /// than the workspace leaves room for; the height is the measure pass at
    /// that width plus the title bar, so every element is visible without
    /// dead room below. AWML describes affordances rather than arrangement,
    /// so both are derived from the content, and a window with a wide row of
    /// buttons opens wide enough for the row while a calculator opens the size
    /// of a calculator. The clamps against the workspace are what a long list
    /// or a wide table runs into, and scrolling takes over from there.
    ///
    /// An application whose top-level content is marked `grow` is saying the
    /// window should have room, not just fit: a listing, a settings page, a
    /// document. Such a window opens at half the workspace in each direction,
    /// or its content's size if that is more. The proportion comes from the
    /// display, not from the application, so the same tree opens larger on a
    /// larger screen and no application carries a pixel size anywhere.
    ///
    /// First trees only. An application that re-renders larger does not get to
    /// move a window the human may have already taken hold of.
    fn fit_window(&mut self, fd: RawFd, fonts: &Fonts) {
        let (min_w, min_h) = min_window();
        let Some(at) = self
            .workspaces
            .iter()
            .position(|workspace| workspace.windows.iter().any(|window| window.fd == fd))
        else {
            return;
        };
        let area = self.window_area(at);
        let Some(client) = self.client(fd) else { return };
        let Some(wanted) = client.natural_width(fonts) else { return };
        let Some(window) = self.workspaces[at].windows.iter().find(|w| w.fd == fd) else {
            return;
        };
        let room_w = (area.x + area.w - window.rect.x - window_margin()).max(min_w);
        let roomy = client.wants_room();
        let width = if roomy { wanted.max(area.w / 2) } else { wanted }.clamp(min_w, room_w);
        let Some(mut content) = client.natural_height(fonts, width) else { return };
        if roomy {
            content = content.max(area.h / 2 - window_title_h());
        }

        let Some(window) = self.workspaces[at].windows.iter_mut().find(|w| w.fd == fd) else {
            return;
        };
        if window.maximized {
            return;
        }
        let room_h = (area.y + area.h - window.rect.y - window_margin()).max(min_h);
        window.rect.w = width;
        window.rect.h = (content + window_title_h()).clamp(min_h, room_h);
        window.restored = window.rect;
        self.reframe(fonts);
    }

    /// The dock: one tile per open window, centred in the taskbar band.
    ///
    /// Every window rather than only the minimized ones, because switching
    /// between windows is what a dock is for and half a switcher is worse than
    /// none. It is compositor chrome for the same reason the title bars are:
    /// which window is where is not something the agentdesk is told. It sits
    /// in the agentdesk's own band because that is where a person looks for
    /// it, and the agentdesk keeps the middle of its taskbar clear for it the
    /// way it leaves the title bars to the compositor: by design, not by
    /// protocol.
    fn dock_rect(&self, at: usize) -> Rect {
        let band = self.regions_for(at).taskbar;
        let count = self
            .workspaces
            .get(at)
            .map(|workspace| workspace.windows.len())
            .unwrap_or(0) as i32;
        let width = (dock_tile() + dock_gap()) * count - dock_gap();
        Rect::new(
            band.x + (band.w - width) / 2,
            band.y + (band.h - dock_tile()) / 2,
            width.max(0),
            dock_tile(),
        )
    }

    /// Where each window's tile sits.
    fn dock_pills(&self, at: usize) -> Vec<(RawFd, Rect)> {
        let Some(workspace) = self.workspaces.get(at) else { return Vec::new() };
        if workspace.windows.is_empty() {
            return Vec::new();
        }

        let dock = self.dock_rect(at);
        // In the order they opened rather than in z-order, so a tile does not
        // move under the pointer when the window behind it is raised.
        let mut pills: Vec<(RawFd, Rect)> =
            workspace.windows.iter().map(|window| (window.fd, dock)).collect();
        pills.sort_by_key(|(fd, _)| *fd);
        let mut x = dock.x;
        for (_, pill) in &mut pills {
            *pill = Rect::new(x, dock.y, dock_tile(), dock_tile());
            x += dock_tile() + dock_gap();
        }
        pills
    }

    /// Where each title bar button sits: minimize, maximize, close, flush right.
    ///
    /// Close is the outermost, so it lives in the corner a flung pointer lands
    /// in, and the glyph order matches what each does: the chevron points down
    /// at the dock the window will go to, the brackets push outward, the cross
    /// is the end.
    fn title_buttons(rect: Rect) -> [(Title, Rect); 3] {
        let bar = Rect::new(rect.x, rect.y, rect.w, window_title_h());
        let w = title_button_w();
        let right = bar.x + bar.w;
        [
            (Title::Minimize, Rect::new(right - w * 3, bar.y, w, bar.h)),
            (Title::Maximize, Rect::new(right - w * 2, bar.y, w, bar.h)),
            (Title::Close, Rect::new(right - w, bar.y, w, bar.h)),
        ]
    }

    /// What part of a window's title bar a point is on.
    fn title_hit(rect: Rect, x: i32, y: i32) -> Option<Title> {
        let bar = Rect::new(rect.x, rect.y, rect.w, window_title_h());
        if !bar.contains(x, y) {
            return None;
        }

        for (action, button) in Self::title_buttons(rect) {
            if button.contains(x, y) {
                return Some(action);
            }
        }
        Some(Title::Drag)
    }

    // ---- connections -------------------------------------------------------

    /// Adopt a descriptor the supervisor pushed, using the frame that named it.
    ///
    /// The workspace id comes off the wire from PID 1 and nowhere else. This is
    /// where identity becomes a capability: everything downstream that scopes an
    /// agent to its own desk, or hides the desk's chrome from it, is enforced by
    /// what was recorded here.
    pub fn attach(&mut self, fonts: &Fonts, fields: &[String], fd: OwnedFd) -> Result<String, String> {
        let field = |at: usize| fields.get(at).map(String::as_str).unwrap_or("");
        let desk: u32 = field(1).parse().unwrap_or(0);

        let (kind, name) = match field(0) {
            "desk-attached" => (Kind::Desk, "workspace".to_owned()),
            "app-attached" => (Kind::App, field(2).to_owned()),
            "agent-attached" => (Kind::Agent, "agent".to_owned()),
            other => return Err(format!("unknown handoff {other:?}")),
        };

        // The name arrived in the supervisor's handoff tag, which is the only
        // identity an app has; its icon is read from the package under that
        // name, so an app cannot wear another's.
        if kind == Kind::App {
            self.images.icons.prepare(&name, &[title_icon(), dock_icon()]);
        }

        let client = Client::adopt(kind, desk, name, UnixStream::from(fd))
            .map_err(|err| format!("could not adopt the descriptor: {err}"))?;
        let label = client.label();
        let fd = client.fd();
        self.clients.push(client);

        let at = self.workspace_at(desk);
        match kind {
            Kind::Desk => {
                self.workspaces[at].desk = Some(fd);
                // A workspace that has just appeared is what the human is
                // looking for.
                self.current = at;
                self.focus = Surface::Desk;
            }
            Kind::App => {
                let opened = self.workspaces[at].opened;
                let rect = self.opening_rect(at, opened);
                self.workspaces[at].opened += 1;
                self.workspaces[at].windows.push(Window {
                    fd,
                    rect,
                    restored: rect,
                    minimized: false,
                    maximized: false,
                });
                self.current = at;
                self.focus = Surface::App(fd);
            }
            Kind::Agent => self.workspaces[at].agent = Some(fd),
        }

        self.reframe(fonts);
        self.nav = None;
        Ok(label)
    }

    pub fn remove(&mut self, fd: RawFd, fonts: &Fonts) -> Option<String> {
        let at = self.clients.iter().position(|client| client.fd() == fd)?;
        let label = self.clients[at].label();
        self.clients.remove(at);

        for workspace in &mut self.workspaces {
            if workspace.desk == Some(fd) {
                workspace.desk = None;
            }
            if workspace.agent == Some(fd) {
                workspace.agent = None;
            }
            workspace.windows.retain(|window| window.fd != fd);
        }

        // A workspace with nothing left in it is gone. Nothing here restarts, so
        // there is no reason to keep an entry the human can navigate to and find
        // empty.
        let had_any = !self.workspaces.is_empty();
        self.workspaces
            .retain(|workspace| workspace.desk.is_some() || !workspace.windows.is_empty());
        self.current = self.current.min(self.workspaces.len().saturating_sub(1));

        // The last workspace going leaves a display with nothing on it, which
        // is not a desk. Ask for a blank one, exactly as at boot, so closing
        // every tab lands the human on an empty agentdesk rather than on a bar
        // with nothing under it. Only on the removal that emptied the list,
        // so a straggling connection closing later cannot ask twice.
        if had_any && self.workspaces.is_empty() {
            self.requests.push(vec!["create-desk".into()]);
            self.notes.push("last agentdesk closed; asking for a blank one".into());
        }

        if self.focus == Surface::App(fd) {
            self.focus = Surface::Desk;
        }

        // An agent that has gone leaves no pointer behind, and an intent whose
        // agent or target has gone has nobody to answer and nothing to act on.

        // An agent or an application that has gone leaves nobody to answer
        // and nothing to answer with.
        if self.clients.iter().all(|client| client.kind != Kind::Agent) {
            self.agent_cursor = None;
        }

        self.reframe(fonts);
        self.nav = None;
        Some(label)
    }

    /// The palette changed under everything painted.
    ///
    /// Colours are read at paint time, so the next frame is simply in the new
    /// palette; the one thing that baked a colour in is the wallpaper cache,
    /// whose composites hold the old background behind any transparency.
    pub fn retheme(&mut self) {
        self.images.set_background(ui::background());
    }

    pub fn client_mut(&mut self, fd: RawFd) -> Option<&mut Client> {
        self.clients.iter_mut().find(|client| client.fd() == fd)
    }

    /// An application's tree changed: tell the agent working in its
    /// workspace, if one is, so it re-reads rather than acting on a view of
    /// how things used to be.
    ///
    /// The name and nothing else crosses. The agent's remedy is the same
    /// `query view` it always had, which returns the whole present state;
    /// pushing the difference itself would reintroduce the drift the
    /// whole-tree protocol exists to prevent. Desk trees never arrive here,
    /// because the caller notifies for app clients only: chrome stays
    /// invisible to agents down to its updates.
    pub fn notify_agent_of_change(&mut self, changed_fd: RawFd) {
        let Some(changed) = self.client(changed_fd) else { return };
        if changed.kind != Kind::App {
            return;
        }
        let (desk, name) = (changed.desk, changed.name.clone());
        if let Some(agent) = self
            .clients
            .iter_mut()
            .find(|client| client.kind == Kind::Agent && client.desk == desk)
        {
            agent.send(&[agent::MSG_CHANGED, &name]);
        }
    }

    fn client(&self, fd: RawFd) -> Option<&Client> {
        self.clients.iter().find(|client| client.fd() == fd)
    }

    pub fn readable(&mut self, fd: RawFd, fonts: &Fonts) -> Option<Progress> {
        let mut progress = self.client_mut(fd)?.readable(fonts);
        if progress.first {
            self.fit_window(fd, fonts);
        }
        // A workspace's title can change with its tree, and the navigation bar
        // shows it.
        if progress.dirty {
            self.nav = None;
        }
        // A fresh tree may be the window an agent is waiting on.
        // Whether the *screen* changed, which is what the event loop uses this
        // for and is not the same question. A workspace nobody is looking at
        // has an agentdesk re-rendering on its own clock and applications
        // answering an agent's actions, and every one of those used to repaint
        // the desk in front of the human, and to throw away the small damage
        // rectangle a window drag would otherwise have cost.
        progress.dirty &= self.on_screen(fd);
        Some(progress)
    }

    /// Whether a workspace is the one on screen.
    /// Point the trail at the workspace on screen, and tell it the size of
    /// the room the cursor crosses.
    ///
    /// Called from wherever the answer might have moved rather than from
    /// every assignment to `current`: `Trail::follow` is a no-op when the
    /// workspace has not changed, so calling it too often costs nothing and
    /// missing one would leave the cursor drawing another desk's work.
    fn watch_on_screen(&mut self) {
        let desk = self.workspaces.get(self.current).map(|workspace| workspace.id);
        let apps = self.regions().apps;
        self.trail.set_stage((apps.x + 20, apps.y + 20), apps.w);
        self.trail.follow(desk);
    }

    /// The range the human set for how long the agent's cursor may take to
    /// show one action. Re-read whenever the settings file changes.
    pub fn set_pace(&mut self, pace: awproto::pace::Pace) {
        self.trail.set_pace(pace);
    }

    fn desk_on_screen(&self, desk: u32) -> bool {
        self.workspaces.get(self.current).is_some_and(|workspace| workspace.id == desk)
    }

    /// Whether anything this client draws is currently visible: its workspace
    /// is the one on screen, and if it owns a window, that window is not
    /// minimized.
    fn on_screen(&self, fd: RawFd) -> bool {
        let Some(client) = self.client(fd) else { return false };
        let Some(workspace) = self.workspaces.get(self.current) else { return false };
        if client.desk != workspace.id {
            return false;
        }
        match workspace.windows.iter().find(|window| window.fd == fd) {
            Some(window) => !window.minimized,
            // Not a window: the agentdesk's own regions, which are the
            // wallpaper, the pane and the taskbar.
            None => true,
        }
    }

    pub fn flush_all(&mut self) {
        for client in &mut self.clients {
            client.flush();
        }
    }

    pub fn broken(&self) -> Vec<RawFd> {
        self.clients
            .iter()
            .filter(|client| client.is_broken())
            .map(Client::fd)
            .collect()
    }

    /// Requests the compositor wants PID 1 to carry out, taken by the event
    /// loop that owns the control connection.
    pub fn take_requests(&mut self) -> Vec<Vec<String>> {
        std::mem::take(&mut self.requests)
    }

    pub fn take_notes(&mut self) -> Vec<String> {
        std::mem::take(&mut self.notes)
    }

    fn workspace_at(&mut self, id: u32) -> usize {
        if let Some(at) = self.workspaces.iter().position(|w| w.id == id) {
            return at;
        }
        self.workspaces.push(Workspace {
            id,
            desk: None,
            windows: Vec::new(),
            agent: None,
            opened: 0,
            pane_collapsed: false,
            name: None,
            pane_t: 0.0,
        });
        self.workspaces.len() - 1
    }

    /// Hand every client the part of the screen it currently occupies.
    fn reframe(&mut self, fonts: &Fonts) {
        // A maximized window means "fill the space", so it follows the space
        // when the space changes. A normal one keeps the rectangle the human
        // put it at: folding the pane away must not rearrange their desk.
        for at in 0..self.workspaces.len() {
            let area = self.window_area(at).inset(6);
            for window in &mut self.workspaces[at].windows {
                if window.maximized {
                    window.rect = area;
                }
            }
        }

        let mut frames: Vec<(RawFd, Frame)> = Vec::new();

        for (at, workspace) in self.workspaces.iter().enumerate() {
            if let Some(fd) = workspace.desk {
                frames.push((fd, Frame::Regions(self.regions_for(at))));
            }
            for window in &workspace.windows {
                frames.push((window.fd, Frame::Whole(content_of(window.rect))));
            }
        }

        for (fd, frame) in frames {
            if let Some(client) = self.client_mut(fd) {
                client.set_frame(fonts, frame);
            }
        }
    }

    // ---- input -------------------------------------------------------------

    pub fn handle(&mut self, fonts: &Fonts, event: Event) -> bool {
        match event {
            // The cursor is the compositor's, so a move is a repaint and nothing
            // else, unless a window or a scrollbar is being carried.
            Event::PointerMoved { x, y } => {
                self.overlay_dirty = true;

                let mut scene = false;
                if self.drag.is_some() {
                    self.drag_to(fonts, x, y);
                    scene = true;
                }
                if self.tab_drag.is_some() {
                    self.nav_drag_motion(fonts, x);
                    scene = true;
                }
                if let Some(fd) = self.scroll_drag
                    && let Some(client) = self.client_mut(fd)
                {
                    client.drag_scroll(fonts, x, y);
                    scene = true;
                }
                if let Some(fd) = self.slider_drag
                    && let Some(client) = self.client_mut(fd)
                {
                    scene |= client.drag_slider(fonts, x);
                }
                if let Some(fd) = self.text_drag
                    && let Some(client) = self.client_mut(fd)
                {
                    scene |= client.drag_select(fonts, x, y);
                }
                if let Some(fd) = self.column_drag
                    && let Some(client) = self.client_mut(fd)
                {
                    scene |= client.drag_column(fonts, x);
                }
                if let Some(fd) = self.cell_drag
                    && let Some(client) = self.client_mut(fd)
                {
                    scene |= client.drag_cells(x, y);
                }
                if let Some(fd) = self.tabs_drag
                    && let Some(client) = self.client_mut(fd)
                {
                    scene |= client.drag_tabs(fonts, x);
                }
                if let Some(menu) = &mut self.start
                    && menu.dragging()
                    && menu.drag(fonts, x, y)
                {
                    scene = true;
                }
                if self.rename_drag && self.drag_rename(fonts, x, y) {
                    scene = true;
                }

                // Hover feedback lives in the scene, so crossing on or off a
                // lit control is a scene change; sweeping across inert pixels
                // is not, and that is the difference between a pointer that
                // costs patches and one that costs frames.
                let hover = self.hover_signature(x, y);
                if hover != self.hover {
                    self.hover = hover;
                    scene = true;
                }
                scene
            }

            Event::ButtonReleased { button: Button::Left, .. } => {
                self.drag = None;
                if let Some(fd) = self.scroll_drag.take()
                    && let Some(client) = self.client_mut(fd)
                {
                    client.end_scroll_drag();
                }
                if let Some(fd) = self.slider_drag.take()
                    && let Some(client) = self.client_mut(fd)
                {
                    client.end_slider_drag();
                }
                if let Some(fd) = self.text_drag.take()
                    && let Some(client) = self.client_mut(fd)
                {
                    client.end_select();
                }
                if let Some(fd) = self.column_drag.take()
                    && let Some(client) = self.client_mut(fd)
                {
                    client.end_column_drag();
                }
                if let Some(fd) = self.cell_drag.take()
                    && let Some(client) = self.client_mut(fd)
                {
                    client.end_cell_drag();
                }
                if let Some(fd) = self.tabs_drag.take()
                    && let Some(client) = self.client_mut(fd)
                {
                    client.end_tab_drag();
                }
                if let Some(menu) = &mut self.start {
                    menu.end_drag();
                }
                if self.rename_drag {
                    self.rename_drag = false;
                    // A press that never moved is no run at all, the rule
                    // every text box on the machine follows.
                    if let Some((_, buffer, _)) = &mut self.renaming
                        && buffer.anchor == Some(buffer.caret)
                    {
                        buffer.anchor = None;
                    }
                }
                self.nav_release(fonts)
            }

            Event::ButtonPressed { button: Button::Left, x, y } => {
                self.blink_epoch = Instant::now();
                self.click(fonts, x, y)
            }

            // The other button. It reaches an application and nothing else:
            // there is no chrome that answers to it, and a workspace an agent
            // is driving stays frozen to it exactly as it is to the first.
            Event::ButtonPressed { button: Button::Right, x, y } => {
                // Words first, wherever they are. What is selected and what
                // is on the clipboard are the compositor's, so the menu that
                // acts on them is too, and the application is not told about
                // a press that was never about it.
                if self.open_chrome_edit(fonts, x, y) {
                    return true;
                }
                match self.surface_at(x, y) {
                    Some(Surface::App(fd)) if !self.agent_running() => {
                        self.raise(self.current, fd);
                        self.focus = Surface::App(fd);
                        if self.open_edit(fonts, fd, x, y) {
                            return true;
                        }
                        self.route_to(fd, fonts, event)
                    }
                    Some(Surface::Desk) => {
                        if let Some(fd) = self.workspaces.get(self.current).and_then(|w| w.desk)
                            && self.open_edit(fonts, fd, x, y)
                        {
                            return true;
                        }
                        self.route_desk(fonts, event)
                    }
                    _ => false,
                }
            }

            Event::Scrolled { delta, x, y } => match self.surface_at(x, y) {
                Some(Surface::App(fd)) => self.route_to(fd, fonts, event),
                Some(Surface::Desk) => self.route_desk(fonts, event),
                Some(Surface::Start) => self.start_wheel(fonts, delta, x, y),
                _ => false,
            },

            Event::KeyPressed(key) => {
                self.blink_epoch = Instant::now();
                // A keystroke puts the edit menu away, the way a keystroke
                // puts away every other menu. Escape does only that; anything
                // else goes on to wherever it was headed.
                if self.edit.take().is_some() && key == Key::Escape {
                    return true;
                }
                match self.focus {
                    Surface::App(fd) => self.route_to(fd, fonts, event),
                    Surface::Desk => self.route_desk(fonts, event),
                    Surface::Nav => self.nav_key(fonts, key),
                    Surface::Start => self.start_key(fonts, key),
                }
            }

            Event::KeyReleased(_) => match self.focus {
                Surface::App(fd) => self.route_to(fd, fonts, event),
                Surface::Desk => self.route_desk(fonts, event),
                Surface::Nav | Surface::Start => false,
            },

            _ => false,
        }
    }

    /// What the pointer should look like over a point.
    pub fn pointer_shape(&self, x: i32, y: i32) -> cursor::Shape {
        // A resize in progress keeps its cursor wherever the pointer strays:
        // the hand is still holding the edge, so the shape still tells the
        // truth about what motion does.
        let resizing = self.drag.as_ref().and_then(|drag| match drag.mode {
            DragMode::Resize { right, bottom } => Some((right, bottom)),
            DragMode::Move { .. } => None,
        });
        let hovered = resizing.or_else(|| {
            if let Some(Surface::App(fd)) = self.surface_at(x, y) {
                self.window(self.current, fd)
                    .and_then(|window| Self::resize_hit(window.rect, x, y))
                    .and_then(|mode| match mode {
                        DragMode::Resize { right, bottom } => Some((right, bottom)),
                        DragMode::Move { .. } => None,
                    })
            } else {
                None
            }
        });
        match hovered {
            Some((true, true)) => return cursor::Shape::ResizeDiag,
            Some((true, false)) => return cursor::Shape::ResizeH,
            Some((false, true)) => return cursor::Shape::ResizeV,
            _ => {}
        }

        if self.renaming.is_some()
            && let Some(doc) = &self.nav
            && let Some(index) = doc.index_of("#nav-rename")
            && self.nav_layout.rect_of(index).contains(x, y)
        {
            return cursor::Shape::Beam;
        }
        if self.start.as_ref().is_some_and(|menu| menu.over_prompt(x, y)) {
            return cursor::Shape::Beam;
        }

        let over_text = match self.surface_at(x, y) {
            Some(Surface::App(fd)) => {
                // The title bar is chrome, not content, whatever sits under it.
                let in_title = self
                    .window(self.current, fd)
                    .is_some_and(|window| y < window.rect.y + window_title_h());
                !in_title && self.client(fd).is_some_and(|client| client.text_at(x, y))
            }
            Some(Surface::Desk) => self
                .workspaces
                .get(self.current)
                .and_then(|workspace| workspace.desk)
                .and_then(|fd| self.client(fd))
                .is_some_and(|client| client.text_at(x, y)),
            _ => false,
        };

        if over_text { cursor::Shape::Beam } else { cursor::Shape::Arrow }
    }

    /// The pointer-dependent pixels under a point, compressed to a comparison.
    fn hover_signature(&self, x: i32, y: i32) -> (u8, Option<(RawFd, u8)>) {
        let shape = match self.pointer_shape(x, y) {
            cursor::Shape::Arrow => 0,
            cursor::Shape::Beam => 1,
            cursor::Shape::ResizeH => 2,
            cursor::Shape::ResizeV => 3,
            cursor::Shape::ResizeDiag => 4,
        };

        let button = match self.surface_at(x, y) {
            Some(Surface::App(fd)) => self
                .window(self.current, fd)
                .and_then(|window| {
                    Self::title_buttons(window.rect)
                        .into_iter()
                        .enumerate()
                        .find(|(_, (_, rect))| rect.contains(x, y))
                })
                .map(|(index, _)| (fd, index as u8)),
            _ => None,
        };

        (shape, button)
    }

    /// The overlay pass asking whether the pointer outran the scene.
    pub fn take_overlay_dirty(&mut self) -> bool {
        std::mem::take(&mut self.overlay_dirty)
    }

    /// The window-drag damage accumulated since the last present, if any.
    pub fn take_drag_damage(&mut self) -> Option<Rect> {
        self.drag_damage.take()
    }

    /// The agent's pointer, when it is in the workspace on screen.
    pub fn agent_pointer(&self) -> Option<(i32, i32)> {
        let (desk, x, y) = self.agent_cursor?;
        self.workspaces
            .get(self.current)
            .filter(|workspace| workspace.id == desk)
            .map(|_| (x, y))
    }

    /// Which surface owns a point.
    fn surface_at(&self, x: i32, y: i32) -> Option<Surface> {
        if self.nav_rect().contains(x, y) {
            return Some(Surface::Nav);
        }
        if self.start.is_some() && self.start_rect().contains(x, y) {
            return Some(Surface::Start);
        }

        if self.regions().apps.contains(x, y)
            && let Some(fd) = self.topmost_at(self.current, x, y)
        {
            return Some(Surface::App(fd));
        }

        // Everything the windows did not take is the workspace's own: the
        // wallpaper behind them, the pane, and the taskbar.
        Some(Surface::Desk)
    }

    fn click(&mut self, fonts: &Fonts, x: i32, y: i32) -> bool {
        // The edit menu, like every other menu, takes the press or is closed
        // by it, and either way the press goes no further.
        if let Some(menu) = &self.edit {
            if menu.contains(x, y) {
                return self.press_edit(fonts, x, y);
            }
            self.edit = None;
            return true;
        }

        // An open start menu takes the click or is closed by it. Closed and
        // consumed: the click that dismisses a menu is not also a click on
        // whatever was behind it, or a window would open on a stray press.
        if self.start.is_some() {
            if self.start_rect().contains(x, y) {
                return self.click_start(fonts, x, y);
            }
            self.close_start();
            return true;
        }

        let Some(surface) = self.surface_at(x, y) else { return false };
        // Where a click went is invisible in a screenshot, so it is traceable
        // in the log when the overlay is on.
        if self.debug {
            self.notes.push(format!(
                "click {x},{y} -> {}",
                match surface {
                    Surface::Nav => "navigation bar".to_owned(),
                    Surface::Desk => "workspace chrome".to_owned(),
                    Surface::App(fd) => format!("app on fd {fd}"),
                    Surface::Start => "start menu".to_owned(),
                }
            ));
        }

        match surface {
            Surface::Nav => {
                self.focus = Surface::Nav;
                self.click_nav(fonts, x, y)
            }

            Surface::App(fd) => {
                // Window management stays live during a turn. The freeze is
                // about not fighting an agent for the same tree, and moving a
                // window out of the way to watch what it is doing is not that.
                if let Some(mode) = self
                    .window(self.current, fd)
                    .and_then(|window| Self::resize_hit(window.rect, x, y))
                {
                    self.raise(self.current, fd);
                    self.focus = Surface::App(fd);
                    self.drag = Some(Drag { fd, mode });
                    return true;
                }

                let title = self
                    .window(self.current, fd)
                    .and_then(|window| Self::title_hit(window.rect, x, y));

                if let Some(action) = title {
                    self.raise(self.current, fd);
                    self.focus = Surface::App(fd);
                    return self.title_action(fonts, fd, action, x, y);
                }

                if self.agent_running() {
                    // A click into the workspace while an agent drives it is
                    // the human taking over, so it interrupts the turn: the
                    // same `interrupt` to PID 1 the stop button sends, from
                    // the same authority. The click itself goes nowhere; it
                    // was a claim on the workspace, not a press on whatever
                    // the agent happened to have under its cursor.
                    let desk = self.workspaces[self.current].id;
                    self.notes.push(format!(
                        "workspace {desk}: click while the agent runs; interrupting it"
                    ));
                    self.requests.push(vec!["interrupt".into(), desk.to_string()]);
                    return true;
                }
                self.raise(self.current, fd);
                self.focus = Surface::App(fd);
                let dirty =
                    self.route_to(fd, fonts, Event::ButtonPressed { button: Button::Left, x, y });
                self.note_drags(fd);
                dirty
            }

            // Only reachable while the menu is open, which was handled above.
            Surface::Start => true,

            Surface::Desk => {
                // All of these are compositor chrome sitting over the
                // workspace, so they are checked before the click is handed to
                // it.
                if self.pane_handle(self.current).contains(x, y) {
                    return self.toggle_pane(fonts);
                }
                if self.start_button_rect(self.current).contains(x, y) {
                    self.open_start(fonts);
                    return true;
                }

                if let Some((fd, _)) = self
                    .dock_pills(self.current)
                    .into_iter()
                    .find(|(_, pill)| pill.contains(x, y))
                {
                    self.raise(self.current, fd);
                    self.focus = Surface::App(fd);
                    self.reframe(fonts);
                    return true;
                }

                // The workspace floor, while an agent drives the workspace:
                // the same takeover a click on a window is. Scoped to the
                // area windows live in, so the pane stays a place to queue a
                // message or scroll the transcript mid-turn, exactly as the
                // input arbitration promises.
                if self.agent_running() && self.window_area(self.current).contains(x, y) {
                    let desk = self.workspaces[self.current].id;
                    self.notes.push(format!(
                        "workspace {desk}: click while the agent runs; interrupting it"
                    ));
                    self.requests.push(vec!["interrupt".into(), desk.to_string()]);
                    return true;
                }

                self.focus = Surface::Desk;
                let desk = self.workspaces.get(self.current).and_then(|w| w.desk);
                let dirty =
                    self.route_desk(fonts, Event::ButtonPressed { button: Button::Left, x, y });
                if let Some(fd) = desk {
                    self.note_drags(fd);
                }
                dirty
            }
        }
    }

    /// Offer cut, copy and paste over whatever words are under the point.
    ///
    /// Answers whether it took the press. It does not when the point is not
    /// on text, which is when the press is the application's `context` event
    /// and its own menu, and it does not when nothing would be offered: a
    /// menu whose every item is dead is worse than no menu.
    fn open_edit(&mut self, fonts: &Fonts, fd: RawFd, x: i32, y: i32) -> bool {
        let Some(offer) = self.client_mut(fd).and_then(|client| client.arm_edit(fonts, x, y))
        else {
            return false;
        };
        self.show_edit(fonts, offer, Target::Client(fd), x, y)
    }

    /// The same menu over one of the compositor's own text boxes.
    fn show_edit(
        &mut self,
        fonts: &Fonts,
        offer: Offer,
        target: Target,
        x: i32,
        y: i32,
    ) -> bool {
        let held = self.clipboard.text().is_some();
        self.edit = EditMenu::open(fonts, self.bounds, (x, y), offer, held, target);
        self.edit.is_some()
    }

    /// The other button over the start menu's prompt or the navigation bar's
    /// rename field. Chrome has text boxes and they answer to the same menu.
    fn open_chrome_edit(&mut self, fonts: &Fonts, x: i32, y: i32) -> bool {
        if let Some(menu) = &self.start {
            if !menu.over_prompt(x, y) {
                return false;
            }
            let offer = menu.offer();
            return self.show_edit(fonts, offer, Target::Prompt, x, y);
        }
        if self.renaming.is_some()
            && let Some(doc) = &self.nav
            && let Some(index) = doc.index_of("#nav-rename")
            && self.nav_layout.rect_of(index).contains(x, y)
            && let Some((_, buffer, _)) = &self.renaming
        {
            let offer = Offer { selection: buffer.selected().is_some(), editable: true };
            return self.show_edit(fonts, offer, Target::Rename, x, y);
        }
        false
    }

    /// Carry out what the edit menu decided, as the chord it stands for.
    ///
    /// Through `Client::handle`, so the menu and `Ctrl+C` are one
    /// implementation: the menu is a second way to say it, not a second thing
    /// that does it.
    fn press_edit(&mut self, fonts: &Fonts, x: i32, y: i32) -> bool {
        let Some(menu) = self.edit.take() else { return false };
        let Some(key) = menu.press(x, y) else { return true };
        match menu.target {
            Target::Client(fd) => {
                let clipboard = &mut self.clipboard;
                if let Some(client) = self.clients.iter_mut().find(|client| client.fd() == fd) {
                    client.handle(fonts, Event::KeyPressed(key), clipboard);
                }
            }
            Target::Prompt => {
                self.start_key(fonts, key);
            }
            Target::Rename => {
                self.nav_key(fonts, key);
            }
        }
        true
    }

    /// Close, minimize, maximize, or pick the window up.
    fn title_action(&mut self, fonts: &Fonts, fd: RawFd, action: Title, x: i32, y: i32) -> bool {
        let at = self.current;
        let area = self.window_area(at);
        let Some(index) = self
            .workspaces
            .get(at)
            .and_then(|w| w.windows.iter().position(|window| window.fd == fd))
        else {
            return false;
        };

        match action {
            Title::Drag => {
                let rect = self.workspaces[at].windows[index].rect;
                self.drag = Some(Drag { fd, mode: DragMode::Move { grab: (x - rect.x, y - rect.y) } });
                true
            }

            Title::Minimize => {
                self.workspaces[at].windows[index].minimized = true;
                // Whatever was underneath comes forward. Leaving focus on a
                // window that is no longer on screen would send the next
                // keystroke somewhere the human cannot see.
                self.focus = self.workspaces[at]
                    .windows
                    .iter()
                    .rev()
                    .find(|window| !window.minimized)
                    .map(|window| Surface::App(window.fd))
                    .unwrap_or(Surface::Desk);
                self.reframe(fonts);
                true
            }

            Title::Maximize => {
                let window = &mut self.workspaces[at].windows[index];
                if window.maximized {
                    window.rect = window.restored;
                    window.maximized = false;
                } else {
                    window.restored = window.rect;
                    window.rect = area.inset(6);
                    window.maximized = true;
                }
                self.reframe(fonts);
                true
            }

            // The cross asks, and that is the whole of it. Only the application
            // knows whether there is anything to lose, so only it can say what
            // closing means: it exits, or it puts a question up first and exits
            // when that is answered.
            //
            // There is no second press that closes the window regardless, and
            // nothing here can end a process. Every application on this machine
            // is written here, so one that hears this and does nothing is a bug
            // to fix in it, not a case for the compositor to carry machinery
            // about. That machinery cost more than it bought: a flag on every
            // window, a clock to tell one gesture from two, and the compositor
            // reaching into a client's tree looking for a dialog to work out
            // whether it had been answered.
            Title::Close => {
                let desk = self.workspaces[at].id;
                if let Some(client) = self.client_mut(fd) {
                    client.ask_to_close();
                }
                self.notes.push(format!("workspace {desk}: asked {fd} to close"));
                true
            }
        }
    }

    /// A press on a window's resize edges, if it is one.
    fn resize_hit(rect: Rect, x: i32, y: i32) -> Option<DragMode> {
        let band = resize_band();
        let right = x >= rect.x + rect.w - band;
        let bottom = y >= rect.y + rect.h - band;
        // The right band starts below the title bar, or it would fight the
        // close button for the corner nobody wants to lose.
        let right = right && y > rect.y + window_title_h();
        if right || bottom {
            Some(DragMode::Resize { right, bottom })
        } else {
            None
        }
    }

    /// Carry a window with the pointer: move it, or pull an edge.
    ///
    /// Movement is clamped so the title bar can always be reached again. A
    /// window dragged entirely off the bottom of its region is a window the
    /// human has lost.
    fn drag_to(&mut self, fonts: &Fonts, x: i32, y: i32) {
        let Some(drag) = &self.drag else { return };
        let fd = drag.fd;
        let at = self.current;
        let area = self.window_area(at);
        let (min_w, min_h) = min_window();

        let Some(window) = self
            .workspaces
            .get_mut(at)
            .and_then(|w| w.windows.iter_mut().find(|window| window.fd == fd))
        else {
            self.drag = None;
            return;
        };
        let before = window.rect;

        match self.drag.as_ref().map(|drag| &drag.mode) {
            Some(DragMode::Move { grab }) => {
                window.rect.x = (x - grab.0)
                    .clamp(area.x - window.rect.w + ui::sc(120), area.x + area.w - ui::sc(120));
                window.rect.y = (y - grab.1).clamp(area.y, area.y + area.h - window_title_h());
            }
            Some(DragMode::Resize { right, bottom }) => {
                if *right {
                    window.rect.w = (x - window.rect.x).clamp(min_w, area.x + area.w - window.rect.x);
                }
                if *bottom {
                    window.rect.h = (y - window.rect.y).clamp(min_h, area.y + area.h - window.rect.y);
                }
            }
            None => return,
        }

        // Dragging a maximized window, by the bar or by an edge, makes it a
        // normal one again, which is what grabbing hold of it ought to mean.
        window.maximized = false;

        // Where it was plus where it is, grown by the shadow's reach, so a
        // pointer-driven repaint can touch only this instead of the screen.
        let after = window.rect;
        let margin = ui::sc(28);
        let x0 = before.x.min(after.x) - margin;
        let y0 = before.y.min(after.y) - margin;
        let x1 = (before.x + before.w).max(after.x + after.w) + margin;
        let y1 = (before.y + before.h).max(after.y + after.h) + margin;
        let hit = Rect::new(x0, y0, x1 - x0, y1 - y0);
        self.drag_damage = Some(match self.drag_damage {
            Some(held) => {
                let x0 = held.x.min(hit.x);
                let y0 = held.y.min(hit.y);
                let x1 = (held.x + held.w).max(hit.x + hit.w);
                let y1 = (held.y + held.h).max(hit.y + hit.h);
                Rect::new(x0, y0, x1 - x0, y1 - y0)
            }
            None => hit,
        });
        self.reframe(fonts);
    }

    fn route_to(&mut self, fd: RawFd, fonts: &Fonts, event: Event) -> bool {
        // The clipboard and the clients are separate fields, so the borrow
        // checker lets one be handed to the other.
        let clipboard = &mut self.clipboard;
        self.clients
            .iter_mut()
            .find(|client| client.fd() == fd)
            .map(|client| client.handle(fonts, event, clipboard))
            .unwrap_or(false)
    }

    /// Remember what a press inside a client took hold of, so that pointer
    /// motion keeps reaching it once the pointer wanders off the thing it
    /// grabbed. Three kinds of drag, one rule.
    fn note_drags(&mut self, fd: RawFd) {
        // Asked and answered before anything is written, so the borrow on the
        // client ends before the fields it would conflict with are set.
        let Some((scrolling, selecting, sizing, cells, tab, sliding)) = self.client(fd).map(|client| {
            (
                client.scroll_dragging(),
                client.selecting(),
                client.sizing_column(),
                client.selecting_cells(),
                client.moving_tab(),
                client.slider_dragging(),
            )
        }) else {
            return;
        };
        if scrolling {
            self.scroll_drag = Some(fd);
        }
        if sliding {
            self.slider_drag = Some(fd);
        }
        if selecting {
            self.text_drag = Some(fd);
        }
        if sizing {
            self.column_drag = Some(fd);
        }
        if cells {
            self.cell_drag = Some(fd);
        }
        if tab {
            self.tabs_drag = Some(fd);
        }
    }

    fn route_desk(&mut self, fonts: &Fonts, event: Event) -> bool {
        let Some(fd) = self.workspaces.get(self.current).and_then(|w| w.desk) else {
            return false;
        };
        self.route_to(fd, fonts, event)
    }

    fn raise(&mut self, at: usize, fd: RawFd) {
        let Some(workspace) = self.workspaces.get_mut(at) else { return };
        let Some(index) = workspace.windows.iter().position(|w| w.fd == fd) else { return };
        // Raising a minimized window is what bringing it back means.
        workspace.windows[index].minimized = false;
        if index + 1 == workspace.windows.len() {
            return;
        }
        let window = workspace.windows.remove(index);
        workspace.windows.push(window);
        // No reframing: raising changes what paints last and what a point
        // resolves to, not where anything is.
    }

    fn agent_running(&self) -> bool {
        self.workspaces
            .get(self.current)
            .is_some_and(|workspace| workspace.agent.is_some())
    }

    /// Move to another workspace, or cycle if there is nowhere named.
    pub fn switch(&mut self, to: usize) -> bool {
        if to >= self.workspaces.len() || to == self.current {
            return false;
        }
        self.current = to;
        self.focus = Surface::Desk;
        self.nav = None;
        self.start = None;
        true
    }

    pub fn cycle(&mut self) -> bool {
        if self.workspaces.len() < 2 {
            return false;
        }
        self.switch((self.current + 1) % self.workspaces.len())
    }

    // ---- the agent surface -------------------------------------------------

    /// Answer whatever an agent asked for.
    ///
    /// Every answer is scoped to the workspace the supervisor said this
    /// connection belongs to. There is no workspace id in any of these messages
    /// and there is nowhere for the agent to put one.
    pub fn requests(&mut self, fonts: &Fonts, from: RawFd, requests: Vec<Vec<String>>) -> bool {
        let mut dirty = false;
        for fields in requests {
            dirty |= self.answer(fonts, from, &fields);
        }
        // Whether the screen changed, which for an agent's intent means
        // whether its workspace is the one being looked at. Queries never
        // change anything and say so themselves.
        let desk = self.client(from).map(|client| client.desk);
        dirty && desk.is_some_and(|desk| self.desk_on_screen(desk))
    }

    fn answer(&mut self, fonts: &Fonts, from: RawFd, fields: &[String]) -> bool {
        let field = |at: usize| fields.get(at).map(String::as_str).unwrap_or("");

        match (field(0), field(1)) {
            (agent::MSG_QUERY, agent::MSG_APPS) => {
                let markup = self.list_apps(from);
                self.reply(from, &[agent::MSG_APPS, &markup]);
                false
            }

            (agent::MSG_QUERY, agent::MSG_VIEW) => {
                let app = field(2).to_owned();
                let markup = self.view_of(from, &app);
                self.reply(from, &[agent::MSG_VIEW, &app, &markup]);
                false
            }

            // What the human last copied. Readable because it is context an
            // agent has no other way to get, and not writable because an
            // agent with something to say says it with `type-text` rather
            // than putting it down for itself to pick up.
            (agent::MSG_QUERY, agent::MSG_CLIPBOARD) => {
                let (kind, content) =
                    (self.clipboard.kind(), self.clipboard.content().to_owned());
                self.reply(from, &[agent::MSG_CLIPBOARD, kind, &content]);
                false
            }

            // Read part of a sheet. A query rather than an action because it
            // is a read, and a range rather than the whole thing because a
            // sheet is the one thing in the system too big to hand over: the
            // element in the view says how far it runs and where it is used,
            // and this is how an agent asks for the part it wants.
            (agent::MSG_QUERY, agent::MSG_CELLS) => {
                let (app, id, range) =
                    (field(2).to_owned(), field(3).to_owned(), field(4).to_owned());
                let block = self.read_cells(from, &app, &id, &range);
                self.reply(from, &[agent::MSG_CELLS, &app, &id, &range, &block]);
                false
            }

            // Applied and answered on arrival, whatever the cursor happens
            // to be doing. See `Flight`.
            (agent::MSG_INTENT, _) => self.begin(fonts, from, fields),

            (other, _) => {
                self.notes.push(format!("agent sent {other:?}, which is not a request"));
                false
            }
        }
    }

    /// A rectangle of one application's sheet, as rows of values.
    ///
    /// Rows of text rather than elements: a screenful of a grid is four
    /// hundred cells, and four hundred elements with a description and an
    /// action list each is forty kilobytes an agent has to read to learn
    /// twenty numbers. The actions are the same for every cell in a sheet and
    /// the description of one is its own name, so both are said once, on the
    /// element, and this carries what is actually in there.
    fn read_cells(&mut self, from: RawFd, app: &str, id: &str, range: &str) -> String {
        let Some(at) = self.agent_workspace(from) else { return String::new() };
        let Some(app_fd) = self.app_in(at, app) else { return String::new() };
        let Some(client) = self.client(app_fd) else { return String::new() };
        client.cells(id, range)
    }

    /// Which workspace an agent connection belongs to.    /// Which workspace an agent connection belongs to.
    fn agent_workspace(&self, from: RawFd) -> Option<usize> {
        self.workspaces.iter().position(|w| w.agent == Some(from))
    }

    /// What applications are open, in this agent's workspace and no other.
    ///
    /// The agentdesk is absent by construction rather than by being filtered:
    /// only windows are listed, and a desk connection is not a window. A useful
    /// consequence is that an agent cannot read the chat pane containing its own
    /// streamed thoughts, which would otherwise be a feedback loop.
    fn list_apps(&self, from: RawFd) -> String {
        let Some(at) = self.agent_workspace(from) else {
            return "<apps/>".to_owned();
        };
        let workspace = &self.workspaces[at];

        let mut out = format!("<apps desk=\"{}\">\n", workspace.id);
        for window in &workspace.windows {
            let Some(client) = self.client(window.fd) else { continue };
            out.push_str(&format!(
                "  <app name=\"{}\" title=\"{}\"/>\n",
                client.name,
                client.title()
            ));
        }
        out.push_str("</apps>");
        out
    }

    fn view_of(&self, from: RawFd, app: &str) -> String {
        let Some(at) = self.agent_workspace(from) else {
            return format!("<rejected target=\"{app}\" reason=\"{}\"/>", agent::REASON_NOT_ADDRESSABLE);
        };

        match self.app_in(at, app).and_then(|fd| self.client(fd)) {
            Some(client) => client.agent_view().unwrap_or_default(),
            None => format!(
                "<rejected target=\"{app}\" reason=\"{}\"/>",
                self.why_not(at, app)
            ),
        }
    }

    fn app_in(&self, at: usize, name: &str) -> Option<RawFd> {
        let workspace = self.workspaces.get(at)?;
        workspace
            .windows
            .iter()
            .find(|window| self.client(window.fd).is_some_and(|c| c.name == name))
            .map(|window| window.fd)
    }

    /// Why an application the agent named is not one it may have.
    ///
    /// The distinction is worth making. Something open in another workspace, or
    /// the workspace's own chrome, is not missing: it exists and is forbidden,
    /// and telling an agent it does not exist would send it looking for it.
    fn why_not(&self, at: usize, name: &str) -> &'static str {
        let elsewhere = self
            .workspaces
            .iter()
            .enumerate()
            .any(|(other, workspace)| {
                other != at
                    && workspace
                        .windows
                        .iter()
                        .any(|w| self.client(w.fd).is_some_and(|c| c.name == name))
            });

        if elsewhere || name == "workspace" {
            agent::REASON_NOT_ADDRESSABLE
        } else {
            agent::REASON_NO_SUCH_APP
        }
    }

    /// The topmost window at a point within one workspace.
    ///
    /// Used both for routing the human's clicks and for deciding whether an
    /// agent's target is covered, which is deliberate: they are the same
    /// question, and answering it twice would let them drift.
    fn topmost_at(&self, at: usize, x: i32, y: i32) -> Option<RawFd> {
        let workspace = self.workspaces.get(at)?;
        workspace
            .windows
            .iter()
            .rev()
            .find(|window| !window.minimized && window.rect.contains(x, y))
            .map(|window| window.fd)
    }

    fn window(&self, at: usize, fd: RawFd) -> Option<&Window> {
        self.workspaces.get(at)?.windows.iter().find(|w| w.fd == fd)
    }

    /// Set the stage for an agent's intent: its target fills the apps region
    /// and every other window in the workspace is put away.
    ///
    /// This is the translation the design promises. An agent names a control,
    /// never a window: whether its target was covered, minimized, or shuffled
    /// behind something is not the agent's problem and not something it can ask
    /// about, so the compositor makes the question impossible instead of
    /// answering it with rejections. The human watching gets the clearest view
    /// of the one app being driven, and the dock shows where the rest went.
    /// The reverse can never happen: there is no resize or arrange in the
    /// intent vocabulary for an agent to ask with.
    fn arrange_for_agent(&mut self, fonts: &Fonts, at: usize, fd: RawFd) {
        let area = self.window_area(at).inset(ui::sc(6));
        let Some(workspace) = self.workspaces.get_mut(at) else { return };

        let mut changed = false;
        for window in &mut workspace.windows {
            if window.fd == fd {
                if window.minimized {
                    window.minimized = false;
                    changed = true;
                }
                if !window.maximized {
                    window.restored = window.rect;
                    window.maximized = true;
                    changed = true;
                }
                if window.rect != area {
                    window.rect = area;
                    changed = true;
                }
            } else if !window.minimized {
                window.minimized = true;
                changed = true;
            }
        }

        self.raise(at, fd);
        if changed {
            self.reframe(fonts);
        }
    }

    /// Check an intent and start the cursor moving, or say why not.
    fn begin(&mut self, fonts: &Fonts, from: RawFd, fields: &[String]) -> bool {
        let field = |at: usize| fields.get(at).map(String::as_str).unwrap_or("");
        let (app, action, target, value) =
            (field(1).to_owned(), field(2).to_owned(), field(3).to_owned(), field(4).to_owned());

        let Some(at) = self.agent_workspace(from) else {
            self.refuse(from, &app, &target, agent::REASON_NOT_ADDRESSABLE);
            return false;
        };

        let Some(app_fd) = self.app_in(at, &app) else {
            let reason = self.why_not(at, &app);
            self.refuse(from, &app, &target, reason);
            return false;
        };

        // The stage is set before anything is measured, so every check below
        // runs against the geometry the action will actually happen in.
        self.arrange_for_agent(fonts, at, app_fd);

        let Some(client) = self.client(app_fd) else {
            self.refuse(from, &app, &target, agent::REASON_NO_SUCH_APP);
            return false;
        };
        // A cell of a spreadsheet is named after its element: `sheet!B7`.
        // Cells are not nodes, so there is nothing else for an intent to
        // name; what is left of the target is the element, and the cell
        // travels beside the action the way it does on the wire.
        let (target, cell) = match target.split_once('!') {
            Some((element, at)) => (element.to_owned(), at.to_owned()),
            None => (target, String::new()),
        };
        let Some(index) = client.node_by_id(&target) else {
            self.refuse(from, &app, &target, agent::REASON_NO_SUCH_NODE);
            return false;
        };
        if !cell.is_empty() && sheet::parse(&cell).is_none() {
            self.refuse(from, &app, &target, agent::REASON_NO_SUCH_NODE);
            return false;
        }

        // Everything the action needs to be legitimate, in the order that
        // makes each answer say what it means.
        //
        // The action list first, because it is derived from the element and
        // its state and is the same list the agent was shown: a disabled
        // control, or an option of a closed dropdown, offers nothing, and
        // saying so is more use than any remark about where it is on screen.
        if client.is_disabled(index) {
            self.refuse(from, &app, &target, agent::REASON_DISABLED);
            return false;
        }
        // Behind a dialog. The view said so too: its actions were empty and
        // it was marked, so an agent that reads before it acts never gets
        // here, and one that does not is told what to deal with first.
        if client.is_blocked(index) {
            self.refuse(from, &app, &target, agent::REASON_BLOCKED);
            return false;
        }
        if !client.offers(index, &action) {
            self.refuse(from, &app, &target, agent::REASON_UNSUPPORTED);
            return false;
        }
        if client.needs_approval(index) {
            self.refuse(from, &app, &target, agent::REASON_NEEDS_APPROVAL);
            return false;
        }

        // Then make it reachable. Scrolling is not something an agent asks
        // for and not something it is told about: acting on a node moves
        // whatever has to move, exactly as acting on an application arranges
        // its window. What is left after this is a node the compositor tried
        // to reveal and could not, which is a fault here rather than a step
        // the agent missed.
        let at_cell = sheet::parse(&cell);
        let reachable = self
            .client_mut(app_fd)
            .map(|client| match at_cell {
                // A cell is brought into view by scrolling the grid it is in,
                // which the compositor can do itself: it holds the sheet.
                Some(at) => client.reveal(fonts, index) && client.reveal_cell(fonts, index, at),
                None => client.reveal(fonts, index),
            })
            .unwrap_or(false);
        if !reachable {
            self.notes.push(format!(
                "BUG: could not bring {target} in {app} into view; the agent was told so"
            ));
            self.refuse(from, &app, &target, agent::REASON_UNREACHABLE);
            return false;
        }

        let Some(client) = self.client(app_fd) else {
            self.refuse(from, &app, &target, agent::REASON_NO_SUCH_APP);
            return false;
        };
        // The cursor flies to the cell rather than to the middle of the whole
        // grid, so a human watching sees it land on the cell it is about to
        // act on.
        let rect = at_cell
            .and_then(|at| client.cell_rect(index, at))
            .unwrap_or_else(|| client.rect_of(index));
        let to = (rect.x + rect.w / 2, rect.y + rect.h / 2);

        // Everything that can refuse this has refused it by now, so perform
        // it, here, before anything is drawn. The agent is answered from this
        // line rather than from the end of an animation, which is the whole
        // point: the model is free to think about its next step while the
        // cursor is still on its way to the last one.
        let outcome = self.perform(fonts, app_fd, &target, &cell, &action, &value);
        match outcome {
            Some(reason) => {
                self.refuse(from, &app, &target, reason);
                return false;
            }
            None => {
                self.notes
                    .push(format!("agent performed {action} on {target} in {app}"));
                self.confirm(from, &app, &target, &action);
            }
        }

        // And only now the picture of it. A flight decides nothing and is
        // owed nothing; if one is already running this joins the back of the
        // trail and waits its turn.
        //
        // Nothing is ever dropped from that queue and it is never reordered.
        // Every action the agent performed is shown, in the order it was
        // performed in, however far behind the machine that leaves the
        // cursor: a human watching a workspace has to be able to trust that
        // what they saw happen is what happened, and a queue that skips is a
        // queue that quietly hides steps.
        let desk = self.workspaces[at].id;
        self.watch_on_screen();
        self.trail.push(desk, to);
        true
    }

    /// Synthesize the events one action is worth, at once.
    ///
    /// Typing is still one event per character, because that is a guarantee
    /// and not a decoration: an application sees a value growing exactly as
    /// it does under a human's hands, and nothing an agent does arrives as a
    /// value that appeared in one step. What has gone is the waiting between
    /// them. The keystrokes are painted by the trail afterwards, at whatever
    /// pace reads well, and the application has them all already.
    fn perform(
        &mut self,
        fonts: &Fonts,
        app: RawFd,
        target: &str,
        cell: &str,
        action: &str,
        value: &str,
    ) -> Option<&'static str> {
        if action != display::ACTION_TYPE_TEXT || value.is_empty() {
            return self.apply(fonts, app, target, cell, action, value);
        }
        self.blink_epoch = Instant::now();
        let total = value.chars().count();
        for done in 1..=total {
            let prefix: String = value.chars().take(done).collect();
            if let Some(reason) = self.apply(fonts, app, target, cell, action, &prefix) {
                return Some(reason);
            }
        }
        None
    }

    /// True while something is mid-animation, so the loop should wake for
    /// frames rather than sleeping until the next event.
    pub fn wants_frame(&self) -> bool {
        self.trail.busy()
            || self.panes_moving()
            // Only what is on screen: a control sinking for a fifth of a
            // second in a workspace nobody is looking at is not a reason to
            // run the compositor at sixty frames a second and repaint the
            // desk in front of the human for every one of them. An agent
            // working in another agentdesk presses a control every few
            // hundred milliseconds, so this was very nearly continuous.
            || self
                .clients
                .iter()
                .any(|client| client.animating() && self.on_screen(client.fd()))
    }

    /// Advance whatever is moving.
    pub fn tick(&mut self, fonts: &Fonts) -> bool {
        let busy = self.wants_frame();
        // One frame after the last animation ends, so a pressed control is
        // repainted unpressed rather than staying that way until the next time
        // something happens to redraw the screen.
        let settling = self.animated && !busy;
        self.animated = busy;

        let panes = self.advance_panes(fonts);

        // A blink transition is worth one frame, and only when a caret is
        // actually on screen to blink.
        let mut blinked = false;
        if self.until_blink().is_some() {
            let phase = self.caret_phase();
            if phase != self.blink_shown {
                self.blink_shown = phase;
                blinked = true;
            }
        }

        self.watch_on_screen();

        // The cursor, which draws what already happened and decides
        // nothing. An agent working in an agentdesk nobody is looking at
        // queues no stops at all, so this is silent for it.
        let before = self.agent_cursor;
        self.agent_cursor = self.trail.tick(Instant::now()).map(|(desk, (x, y))| (desk, x, y));
        if self.agent_cursor != before {
            let showing = self
                .agent_cursor
                .or(before)
                .is_some_and(|(desk, _, _)| self.desk_on_screen(desk));
            self.overlay_dirty |= showing;
            return showing;
        }
        busy || settling || panes || blinked
    }

    /// Apply one action, re-resolving the target first.
    ///
    /// The tree may have changed while the cursor was travelling or while the
    /// text was going in. A human takes the same risk, and the difference is
    /// that the compositor can notice: refusing beats acting on whatever moved
    /// into that place.
    fn apply(
        &mut self,
        fonts: &Fonts,
        app: RawFd,
        target: &str,
        cell: &str,
        action: &str,
        value: &str,
    ) -> Option<&'static str> {
        match self.client_mut(app) {
            Some(client) => match client.node_by_id(target) {
                Some(index) => client.act_cell(fonts, index, action, value, cell).err(),
                None => Some(agent::REASON_NO_SUCH_NODE),
            },
            None => Some(agent::REASON_NO_SUCH_APP),
        }
    }

    fn confirm(&mut self, to: RawFd, app: &str, target: &str, action: &str) {
        self.reply(to, &[agent::MSG_DONE, app, target, action]);
    }

    /// Rejections are answers, not silence. An agent that cannot be told no acts
    /// blind and retries forever.
    fn refuse(&mut self, to: RawFd, app: &str, target: &str, reason: &str) {
        self.notes.push(format!("agent refused {target} in {app}: {reason}"));
        self.reply(to, &[agent::MSG_REJECTED, app, target, reason]);
    }

    fn reply(&mut self, to: RawFd, fields: &[&str]) {
        if let Some(client) = self.client_mut(to) {
            client.send(fields);
        }
    }

    // ---- the navigation bar ------------------------------------------------

    /// The bar as AWML, so it goes through the same layout and painting as
    /// everything else rather than being a second rendering path.
    /// The bar as AWML, using the same `tabs` and `tab` elements an
    /// application uses.
    ///
    /// There used to be two implementations of a row of tabs: this one, built
    /// from buttons with trailing spaces reserving room for a close glyph
    /// painted over the top, and the element. Keeping the two looking alike
    /// meant matching them by hand and checking the result a pixel at a time,
    /// which is exactly as reliable as it sounds. There is one now. The bar
    /// is the element's first user, so whatever it does an application gets,
    /// and neither can drift from the other because there is nothing to
    /// drift from.
    fn nav_markup(&self) -> String {
        let mut out = String::from(
            "<window font=\"sans\" pad=\"none\" size=\"sm\">\n  <tabs gap=\"sm\" grow=\"true\">\n",
        );

        for (at, workspace) in self.workspaces.iter().enumerate() {
            if let Some((renaming, buffer, width)) = &self.renaming
                && *renaming == at
            {
                out.push_str(&format!(
                    "    <field id=\"nav-rename\" value=\"{value}\" width=\"{width}\" \
                     description=\"The name being typed for this agentdesk\"/>\n",
                    value = awproto::display::escape(&buffer.value),
                ));
                continue;
            }

            // The element reserves the room for the cross and draws it, so
            // there are no trailing spaces to leave it a landing zone.
            let busy = if workspace.agent.is_some() { " *" } else { "" };
            out.push_str(&format!(
                "    <tab id=\"nav-desk-{id}\" label=\"{name}{busy}\" closable=\"true\"{chosen} \
                 description=\"Switches to this agentdesk\"/>\n",
                id = workspace.id,
                name = awproto::display::escape(&self.tab_name(workspace)),
                chosen = if at == self.current { " selected=\"true\"" } else { "" },
            ));
        }

        // The plus at the end of the row: a new agentdesk with no prompt. It
        // is chrome rather than a workspace's control for the same reason the
        // tabs are: it is how the human gets somewhere else, and it must work
        // when no workspace does. A workspace's taskbar holds the other way,
        // a new agentdesk with a prompt.
        out.push_str(
            "    <button id=\"nav-new\" label=\"+\" \
             description=\"Creates a new agentdesk with nothing to do yet\"/>\n",
        );
        out.push_str("    <text grow=\"true\"/>\n");

        // Drawn only while there is something to stop, so the button never
        // implies a turn is running when none is.
        if self.agent_running() {
            out.push_str(
                "    <button id=\"nav-stop\" label=\"Stop\" emphasis=\"danger\" \
                 description=\"Interrupts the agent running in this workspace\"/>\n",
            );
        }

        out.push_str("  </tabs>\n</window>\n");
        out
    }

    fn build_nav(&mut self, fonts: &Fonts) {
        let markup = self.nav_markup();
        let Ok(doc) = Document::parse(&markup, 0) else { return };

        // The whole bar. The strip is what insets its tabs, caps and foots
        // itself and draws its own hairline, so there is nothing left here to
        // compute: an earlier version worked the margins out by hand and the
        // element now owns that arithmetic for the bar and for applications
        // alike.
        let bar = self.nav_rect();
        self.nav_layout = ui::layout_chrome(fonts, &doc, &Frame::Whole(bar), &mut self.nav_scroll);
        self.nav = Some(doc);
    }

    /// An agentdesk tab's display name.
    fn tab_name(&self, workspace: &Workspace) -> String {
        workspace
            .name
            .clone()
            .unwrap_or_else(|| format!("Agentdesk {}", workspace.id))
    }

    /// The tabs as laid out, in display order.
    fn nav_tab_rects(&self) -> Vec<(u32, Rect)> {
        let Some(doc) = &self.nav else { return Vec::new() };
        (0..doc.tree.nodes.len())
            .filter_map(|index| {
                let id = doc.tree.node(index).id()?;
                let number = id.strip_prefix("nav-desk-")?.parse().ok()?;
                Some((number, self.nav_layout.rect_of(index)))
            })
            .collect()
    }

    /// A press in the navigation bar. Tabs defer their action to the release,
    /// because until then a press does not know whether it is a click or the
    /// start of a drag; everything else acts immediately.
    fn click_nav(&mut self, fonts: &Fonts, x: i32, y: i32) -> bool {
        if self.nav.is_none() {
            self.build_nav(fonts);
        }
        let Some(doc) = &self.nav else { return false };
        let Some(index) = self.nav_layout.hit(&doc.tree, x, y) else {
            self.commit_rename(fonts);
            return true;
        };
        let rect = self.nav_layout.rect_of(index);
        let Some(id) = doc.tree.node(index).id().map(str::to_owned) else {
            self.commit_rename(fonts);
            return true;
        };

        // A click anywhere but the rename field settles the rename first.
        if id != "nav-rename" {
            self.commit_rename(fonts);
        } else {
            // In it, the press puts the caret down and anchors a run, exactly
            // as it does in an application's field: the rename field is the
            // same text box.
            let at = self.rename_caret(fonts, x, y);
            let count = self.presses.press(x, y);
            if let Some((_, buffer, _)) = &mut self.renaming {
                if count > 1 {
                    buffer.press_again(at, count);
                } else {
                    buffer.press(at);
                }
            }
            self.rename_drag = true;
            return true;
        }

        if id == "nav-stop" {
            // Straight to PID 1. This is the one control that must work when the
            // agentdesk cannot.
            let desk = self.workspaces[self.current].id;
            self.requests.push(vec!["interrupt".into(), desk.to_string()]);
            self.notes.push(format!("workspace {desk}: stop requested"));
            return true;
        }

        if id == "nav-new" {
            // Straight to PID 1, like the stop button and the tab's close: a
            // workspace comes from the broker and the compositor is already
            // its client. Creating one starts no turn; the desk that appears
            // is empty until someone types into it.
            self.requests.push(vec!["create-desk".into()]);
            self.notes.push("new agentdesk requested".into());
            return true;
        }

        if let Some(number) = id.strip_prefix("nav-desk-")
            && let Ok(wanted) = number.parse::<u32>()
        {
            // The close glyph occupies the tab's right end.
            if x >= rect.x + rect.w - ui::tab_close_w() {
                self.requests.push(vec!["close-desk".into(), wanted.to_string()]);
                self.notes.push(format!("workspace {wanted}: close requested from its tab"));
                return true;
            }
            self.tab_drag = Some(TabDrag {
                id: wanted,
                start_x: x,
                grab_dx: x - rect.x,
                at_x: x,
                moved: false,
            });
            return true;
        }

        true
    }

    /// A release over the bar: a tab that never moved was a click, and a click
    /// on the tab already in front means the human wants to rename it.
    fn nav_release(&mut self, _fonts: &Fonts) -> bool {
        let Some(held) = self.tab_drag.take() else { return false };
        if held.moved {
            return true;
        }
        let Some(at) = self.workspaces.iter().position(|w| w.id == held.id) else {
            return true;
        };

        if at == self.current {
            let name = self.tab_name(&self.workspaces[at]);
            // The editor takes over the tab's exact footprint.
            let width = self
                .nav_tab_rects()
                .iter()
                .find(|(id, _)| *id == held.id)
                .map(|(_, rect)| rect.w)
                .unwrap_or(ui::sc(160));
            self.renaming = Some((at, Editing::new(name), width));
            self.focus = Surface::Nav;
            self.nav = None;
            return true;
        }
        self.switch(at) || true
    }

    /// Carry a held tab into a new position.
    ///
    /// The reorder is applied live rather than shown as a ghost: the tabs are
    /// cheap to rebuild, and the row rearranging under the hand is its own
    /// feedback.
    fn nav_drag_motion(&mut self, fonts: &Fonts, x: i32) {
        let Some(held) = &mut self.tab_drag else { return };
        held.at_x = x;
        if !held.moved && (x - held.start_x).abs() < ui::sc(4) {
            return;
        }
        held.moved = true;
        let id = held.id;

        // The layout must exist before the slot arithmetic runs. Input arrives
        // in bursts, several motions to one repaint, and the first version left
        // the bar torn down after a reorder: every further motion in the same
        // burst then saw no tabs at all, computed slot zero, and hauled the tab
        // back to the front. A drag that works one motion at a time and fails
        // at mouse speed is exactly the kind of bug a paced test misses.
        if self.nav.is_none() {
            self.build_nav(fonts);
        }
        let tabs = self.nav_tab_rects();
        // Where the pointer falls among the other tabs' middles is the slot
        // this one belongs in. The arithmetic is `ui::tab_slot`, which is
        // also what an application's strip uses: the rule is the same one
        // and there is no reason for two of it.
        let held = tabs.iter().position(|(tab, _)| *tab == id).unwrap_or(0);
        let rects: Vec<Rect> = tabs.iter().map(|(_, rect)| *rect).collect();
        let slot = ui::tab_slot(&rects, held, x);

        let Some(from) = self.workspaces.iter().position(|w| w.id == id) else { return };
        if slot == from {
            return;
        }
        let current_id = self.workspaces[self.current].id;
        let workspace = self.workspaces.remove(from);
        self.workspaces.insert(slot.min(self.workspaces.len()), workspace);
        self.current = self
            .workspaces
            .iter()
            .position(|w| w.id == current_id)
            .unwrap_or(0);
        // Rebuilt on the spot rather than left for the next repaint, so the
        // rest of this burst measures against the new order.
        self.nav = None;
        self.build_nav(fonts);
    }

    /// A keystroke while a tab is being renamed.
    fn nav_key(&mut self, fonts: &Fonts, key: Key) -> bool {
        // Enter and Escape are the bar's: one commits the name, the other
        // abandons it. Everything about the words is the text box's, which is
        // the same text box the start menu's prompt and an application's
        // field are.
        match key {
            Key::Enter | Key::ShiftEnter => {
                self.commit_rename(fonts);
                return true;
            }
            Key::Escape => {
                self.renaming = None;
                self.nav = None;
                return true;
            }
            _ => {}
        }
        let clipboard = &mut self.clipboard;
        let Some((_, buffer, _)) = &mut self.renaming else { return false };
        if buffer.key(key, clipboard, false) == Edit::Ignored {
            return false;
        }
        self.nav = None;
        true
    }

    /// Which character of the rename field a point lands on.
    fn rename_caret(&self, fonts: &Fonts, x: i32, y: i32) -> usize {
        let Some(doc) = &self.nav else { return 0 };
        let Some(index) = doc.index_of("#nav-rename") else { return 0 };
        let Some((_, buffer, _)) = &self.renaming else { return 0 };
        ui::caret_at_point(
            fonts,
            &buffer.value,
            &ui::style_at(&doc.tree, index),
            doc.tree.node(index).tag,
            self.nav_layout.rect_of(index),
            buffer.caret,
            (x, y),
        )
    }

    /// Carry the far end of the rename field's selection to here.
    fn drag_rename(&mut self, fonts: &Fonts, x: i32, y: i32) -> bool {
        if !self.rename_drag {
            return false;
        }
        let at = self.rename_caret(fonts, x, y);
        let Some((_, buffer, _)) = &mut self.renaming else { return false };
        buffer.drag(at)
    }

    /// Settle a rename in progress: the trimmed text becomes the name, and an
    /// emptied field falls back to the default rather than keeping a blank tab.
    fn commit_rename(&mut self, fonts: &Fonts) {
        let Some((at, buffer, _)) = self.renaming.take() else { return };
        if let Some(workspace) = self.workspaces.get_mut(at) {
            let trimmed = buffer.value.trim();
            workspace.name = if trimmed.is_empty() { None } else { Some(trimmed.to_owned()) };
            let (id, name) = (workspace.id, self.tab_name(&self.workspaces[at]));
            self.notes.push(format!("workspace {id} is now named {name:?}"));
        }
        self.nav = None;
        let _ = fonts;
    }

    // ---- the start menu ----------------------------------------------------

    /// The Agentware mark at the left end of the taskbar band.
    fn start_button_rect(&self, at: usize) -> Rect {
        let band = self.regions_for(at).taskbar;
        Rect::new(
            band.x + ui::sc(6),
            band.y + (band.h - dock_tile()) / 2,
            start_button_w(),
            dock_tile(),
        )
    }

    /// Where the panel goes: the workspace area above the taskbar. The menu
    /// centres itself in it and sizes itself to its content.
    fn start_area(&self) -> Rect {
        let area = self.workspace_area();
        Rect::new(area.x, area.y, area.w, area.h - self.taskbar_h)
    }

    /// The open panel's rectangle, or an empty one when it is closed.
    fn start_rect(&self) -> Rect {
        self.start.as_ref().map(StartMenu::panel).unwrap_or(Rect::new(0, 0, 0, 0))
    }

    fn open_start(&mut self, fonts: &Fonts) {
        self.commit_rename(fonts);
        let mut menu = StartMenu::open();
        let area = self.start_area();
        menu.build(fonts, &mut self.images, area);
        self.start = Some(menu);
        self.focus = Surface::Start;
        self.blink_epoch = Instant::now();
    }

    fn close_start(&mut self) {
        if self.start.take().is_some() && self.focus == Surface::Start {
            self.focus = Surface::Desk;
        }
    }

    /// Carry out what the menu decided. Requests go to PID 1 the way the stop
    /// button's does; the menu itself never touches the control connection.
    fn start_outcome(&mut self, fonts: &Fonts, outcome: StartOutcome) -> bool {
        match outcome {
            StartOutcome::Nothing => false,
            StartOutcome::Changed => {
                let area = self.start_area();
                if let Some(menu) = &mut self.start {
                    menu.build(fonts, &mut self.images, area);
                }
                true
            }
            StartOutcome::Close => {
                self.close_start();
                true
            }
            StartOutcome::CreateDesk { prompt, backend } => {
                // The prompt and the model travel the same way: as arguments
                // PID 1 hands to the new agentdesk without reading them. The
                // empty prompt is still sent when a model is named, so the
                // two are never confused for one another.
                self.requests.push(vec![
                    "create-desk".into(),
                    prompt.unwrap_or_default(),
                    backend.to_owned(),
                ]);
                self.notes
                    .push(format!("new agentdesk requested from the start menu, as {backend}"));
                self.close_start();
                true
            }
            StartOutcome::Open(app) => {
                let desk = self.workspaces[self.current].id;
                self.notes.push(format!("workspace {desk}: opening {app} from the start menu"));
                self.requests.push(vec!["open-app".into(), desk.to_string(), app]);
                self.close_start();
                true
            }
            // Real power, by the same path as everything else the chrome
            // asks for: a request to PID 1, which runs the orderly shutdown.
            // The menu still closes, because the request travels on the next
            // loop pass and a menu frozen on screen would read as a hang.
            StartOutcome::PowerOff => {
                self.requests.push(vec!["poweroff".into()]);
                self.notes.push("power off requested from the start menu".into());
                self.close_start();
                true
            }
            StartOutcome::Restart => {
                self.requests.push(vec!["reboot".into()]);
                self.notes.push("restart requested from the start menu".into());
                self.close_start();
                true
            }
        }
    }

    fn click_start(&mut self, fonts: &Fonts, x: i32, y: i32) -> bool {
        let Some(menu) = &mut self.start else { return false };
        let outcome = menu.click(fonts, x, y);
        self.start_outcome(fonts, outcome)
    }

    fn start_key(&mut self, fonts: &Fonts, key: Key) -> bool {
        let clipboard = &mut self.clipboard;
        let Some(menu) = &mut self.start else { return false };
        let outcome = menu.key(key, clipboard);
        self.start_outcome(fonts, outcome)
    }

    fn start_wheel(&mut self, fonts: &Fonts, delta: i32, x: i32, y: i32) -> bool {
        let Some(menu) = &mut self.start else { return false };
        let outcome = menu.wheel(delta, x, y);
        self.start_outcome(fonts, outcome)
    }

    /// The button on the band: the mark, pressed-looking while the menu is up.
    fn draw_start_button(&self, canvas: &mut Canvas) {
        let button = self.start_button_rect(self.current);
        if self.start.is_some() {
            canvas.fill_round_rect(button, ui::radius_control(), ui::pressed());
            canvas.stroke_round_rect(button, ui::radius_control(), 1, ui::border());
        }
        if let Some(icon) = self.images.icons.get(AGENTWARE_ICON, start_icon()) {
            canvas.blend_pixmap(
                icon.as_ref(),
                button.x + (button.w - start_icon()) / 2,
                button.y + (button.h - start_icon()) / 2,
            );
        }
    }

    // ---- painting ----------------------------------------------------------

    pub fn draw(&mut self, canvas: &mut Canvas, fonts: &Fonts, pointer: (i32, i32)) {
        if self.nav.is_none() {
            self.build_nav(fonts);
        }

        // The caret belongs to wherever keystrokes go, and nowhere else. Other
        // windows keep their remembered focus ring, but a bar that blinks in a
        // window that cannot hear the keyboard would be a lie.
        let keyboard = self.keyboard_client();
        let phase = self.caret_phase();
        self.blink_shown = phase;
        for client in &mut self.clients {
            client.caret_on = keyboard == Some(client.fd()) && phase;
        }

        canvas.fill_rect(canvas.bounds(), ui::background());

        match self.workspaces.get(self.current) {
            Some(_) => self.draw_workspace(canvas, fonts, pointer),
            None => self.draw_empty(canvas, fonts),
        }

        if let Some(menu) = &self.start {
            menu.draw(canvas, fonts, &self.images, self.start_rect(), self.caret_phase());
        }
        if let Some(menu) = &self.edit {
            menu.draw(canvas, fonts, &self.images);
        }
        self.draw_nav(canvas, fonts, pointer);

        if self.debug {
            self.draw_debug(canvas, fonts);
        }

        // Cursors are deliberately absent. They live in an overlay the event
        // loop stamps as small patches over this scene, because a pointer that
        // forces the whole screen through the rasterizer and the host encoder
        // on every twitch is most of what "sluggish" is made of.
    }

    /// Wallpaper, then windows, then the pane and the taskbar over them.
    ///
    /// The order is the reason painting has to be able to start partway down a
    /// tree: the workspace's regions are branches of one document that do not
    /// paint consecutively, because the application windows go between them.
    fn draw_workspace(&self, canvas: &mut Canvas, fonts: &Fonts, pointer: (i32, i32)) {
        let regions = self.regions();
        let workspace = &self.workspaces[self.current];
        let desk = workspace.desk.and_then(|fd| self.client(fd));

        canvas.fill_rect(regions.background, ui::background());
        if let Some(desk) = desk {
            canvas.clipped(regions.background, |canvas| {
                desk.draw_region(canvas, fonts, &self.images, "background")
            });
        }

        // No fill for the apps region: the wallpaper behind it is the
        // workspace's, and painting over it would mean an agentdesk could never
        // put anything behind its own windows.
        for window in &workspace.windows {
            if window.minimized {
                continue;
            }
            // A window the repaint does not reach costs nothing. Worth the
            // test because a window is not cheap: four spreadsheets open at
            // once measured 11.8ms of paint against 4.3ms for one, about two
            // milliseconds each, and a repaint of the region a small window
            // swept used to pay all of it.
            if window.rect.inset(-window_reach()).intersect(&canvas.clip()).is_none() {
                continue;
            }
            let focused = self.focus == Surface::App(window.fd);
            canvas.clipped(regions.apps, |canvas| {
                self.draw_window(canvas, fonts, window, focused, pointer)
            });
        }

        // The taskbar band: the agentdesk's row of controls on a raised
        // surface, with the compositor's dock centred over the middle of it.
        let band = regions.taskbar;
        canvas.fill_round_rect_vgrad(band, 0, ui::lift(ui::surface(), 6), ui::surface());
        canvas.fill_rect(Rect::new(band.x, band.y, band.w, 1), ui::border());
        if let Some(desk) = desk {
            canvas.clipped(band, |canvas| desk.draw_region(canvas, fonts, &self.images, "taskbar"));
        }
        self.draw_dock(canvas, fonts);
        self.draw_start_button(canvas);

        let rect = regions.pane;
        if rect.w > 0 {
            canvas.fill_rect(rect, ui::surface());
            canvas.fill_rect(Rect::new(rect.x, rect.y, 1, rect.h), ui::border());
            if let Some(desk) = desk {
                canvas.clipped(rect, |canvas| desk.draw_region(canvas, fonts, &self.images, "pane"));
            }
        }

        self.draw_pane_handle(canvas);
    }

    /// The dock: one icon tile per open window, on the taskbar band.
    fn draw_dock(&self, canvas: &mut Canvas, fonts: &Fonts) {
        let pills = self.dock_pills(self.current);
        if pills.is_empty() {
            return;
        }

        // The fallback for an app without an icon: its initial, drawn large.
        // A letter is not a picture, but it is stable, unique-ish, and honest
        // about which window the tile is.
        let style = Style { size: 15.0 * ui::scale(), ..Style::default() };
        for (fd, pill) in pills {
            let Some(client) = self.client(fd) else { continue };
            let minimized = self
                .window(self.current, fd)
                .is_some_and(|window| window.minimized);
            let focused = self.focus == Surface::App(fd);

            if focused {
                canvas.fill_round_rect(pill, ui::radius_control(), ui::raised());
                canvas.stroke_round_rect(pill, ui::radius_control(), 1, ui::border());
            }

            match self.images.icons.get(&client.name, dock_icon()) {
                Some(icon) => {
                    canvas.blend_pixmap(
                        icon.as_ref(),
                        pill.x + (pill.w - dock_icon()) / 2,
                        pill.y + ui::sc(3),
                    );
                }
                None => {
                    let initial = client.title().chars().next().unwrap_or('?').to_string();
                    let ink = if minimized { ui::muted() } else { ui::text() };
                    let x = pill.x + (pill.w - fonts.measure(&initial, &style)) / 2;
                    canvas.draw_text(
                        fonts,
                        &initial,
                        x,
                        pill.y + ui::sc(3),
                        &style,
                        ink,
                    );
                }
            }

            // A dot under a window that is on screen, the way a dock marks a
            // running application. Absent for one that is put away.
            if !minimized {
                let dot = ui::sc(3).max(3);
                canvas.fill_round_rect(
                    Rect::new(
                        pill.x + (pill.w - dot) / 2,
                        pill.y + pill.h - dot - ui::sc(2),
                        dot,
                        dot,
                    ),
                    dot / 2,
                    ui::accent(),
                );
            }
        }
    }

    /// The grip that folds the conversation pane away.
    fn draw_pane_handle(&self, canvas: &mut Canvas) {
        let grip = self.pane_handle(self.current);
        canvas.fill_round_rect(grip, pane_handle_w() / 2, ui::raised());
        canvas.stroke_round_rect(grip, pane_handle_w() / 2, 1, ui::border());
        canvas.fill_rect(
            Rect::new(grip.x + grip.w / 2 - 1, grip.y + 14, 2, grip.h - 28),
            ui::muted(),
        );
    }

    fn draw_empty(&self, canvas: &mut Canvas, fonts: &Fonts) {
        let style = Style { family: Family::Mono, size: 18.0 * ui::scale(), ..Style::default() };
        let message = "no workspace open";
        let width = fonts.measure(message, &style);
        canvas.draw_text(
            fonts,
            message,
            (canvas.width() - width) / 2,
            canvas.height() / 2,
            &style,
            ui::muted(),
        );
    }

    fn draw_nav(&self, canvas: &mut Canvas, fonts: &Fonts, _pointer: (i32, i32)) {
        let rect = self.nav_rect();
        // No fill and no hairline here: the strip paints the band, the tabs
        // and their crosses, exactly as it does inside an application.
        let Some(doc) = &self.nav else { return };

        // While a tab is being renamed its field carries the caret, on the same
        // blink clock as every other caret: solid while keys arrive, blinking
        // while the field waits.
        let focus = match &self.renaming {
            Some((_, buffer, _)) => Focus {
                node: doc.index_of("#nav-rename"),
                caret: buffer.caret,
                anchor: buffer.anchor,
                caret_visible: self.caret_phase(),
                ..Focus::default()
            },
            None => Focus::default(),
        };

        canvas.clipped(rect, |canvas| {
            ui::paint_subtree(canvas, fonts, &ui::Content { images: &self.images, sheets: &crate::sheet::Sheets::default() }, &doc.tree, &self.nav_layout, Document::ROOT, &focus)
        });

        // The crosses are the strip's, drawn with the tabs. What is left
        // here is the one thing that is in no tree: the tab being carried.
        let current_id = self.workspaces.get(self.current).map(|w| w.id);
        let dragging = self
            .tab_drag
            .as_ref()
            .filter(|held| held.moved)
            .map(|held| (held.id, held.at_x - held.grab_dx));

        // The held tab is lifted out of the row and rides under the pointer.
        // An application's strip does the same, from the same function.
        if let Some((id, ghost_x)) = dragging
            && let Some((_, home)) = self.nav_tab_rects().into_iter().find(|(tab, _)| *tab == id)
            && let Some(workspace) = self.workspaces.iter().find(|w| w.id == id)
        {
            ui::draw_tab_ghost(
                canvas,
                fonts,
                home,
                Rect::new(ghost_x, home.y, home.w, home.h),
                &self.tab_name(workspace),
                current_id == Some(id),
                true,
            );
        }

        let _ = rect;
    }

    /// A readout of what the compositor is holding, for screenshots.
    fn draw_debug(&self, canvas: &mut Canvas, fonts: &Fonts) {
        let style = Style { family: Family::Mono, size: 13.0 * ui::scale(), ..Style::default() };
        let height = 20 * (self.clients.len() as i32 + 4);
        let panel = Rect::new(8, self.nav_rect().h + 8, 620, height);
        canvas.fill_rect(panel, ui::surface());
        canvas.stroke_rect(panel, 1, ui::accent());

        let mut y = panel.y + 6;
        let workspace = self
            .workspaces
            .get(self.current)
            .map(|w| format!("workspace {} ({} windows)", w.id, w.windows.len()))
            .unwrap_or_else(|| "no workspace".into());
        canvas.draw_text(fonts, &workspace, panel.x + 8, y, &style, ui::accent());
        y += 20;

        for client in &self.clients {
            let front = match self.focus {
                Surface::App(fd) if fd == client.fd() => "*",
                Surface::Desk if Some(client.fd()) == self.workspaces.get(self.current).and_then(|w| w.desk) => "*",
                _ => " ",
            };
            canvas.draw_text(
                fonts,
                &format!("{front}{} v{} {}", client.label(), client.version(), client.note),
                panel.x + 8,
                y,
                &style,
                ui::text(),
            );
            y += 20;
            if front == "*" {
                canvas.draw_text(fonts, &client.focus_summary(), panel.x + 20, y, &style, ui::muted());
                y += 20;
            }
        }
    }
}

/// Where an application's own tree goes inside its window.
fn content_of(rect: Rect) -> Rect {
    Rect::new(rect.x, rect.y + window_title_h(), rect.w, rect.h - window_title_h())
}

/// The chrome around an application window.
///
/// Drawn by the compositor from the `title` the application declared. An app
/// names its window; it does not draw one, does not know where it is, and
/// cannot draw outside it.
///
/// The three dots are the only controls in Agentware that are not AWML. They
/// are compositor affordances over a client rather than part of any client's
/// interface, and putting them in the tree would mean every application could
/// decide whether it was closable.
impl Screen {
fn draw_window(
    &self,
    canvas: &mut Canvas,
    fonts: &Fonts,
    window: &Window,
    focused: bool,
    pointer: (i32, i32),
) {
    let Some(client) = self.client(window.fd) else { return };
    let icon = self.images.icons.get(&client.name, title_icon());
    let rect = window.rect;
    let maximized = window.maximized;
    let bar = Rect::new(rect.x, rect.y, rect.w, window_title_h());

    // Depth rather than a heavy outline. A focused window sits higher.
    canvas.shadow(rect, ui::radius_window(), ui::sc(if focused { 22 } else { 12 }), 130);
    canvas.fill_round_rect(rect, ui::radius_window(), ui::background());

    // The bar is the top of the same rounded shape, clipped to its own height so
    // the two lower corners stay square against the content below. Lit faintly
    // from above like every other raised surface.
    canvas.clipped(bar, |canvas| {
        let base = if focused { ui::raised() } else { ui::surface() };
        canvas.fill_round_rect_vgrad(
            Rect::new(bar.x, bar.y, bar.w, bar.h + ui::radius_window()),
            ui::radius_window(),
            ui::lift(base, 8),
            base,
        );
    });

    let cy = bar.y + bar.h / 2;

    // The window controls: stroke glyphs at the bar's right end rather than
    // anyone else's coloured dots. Each is quiet until the pointer is over it,
    // which the compositor can afford because it repaints on pointer motion
    // anyway; the chip under the hovered one is what says "this is a button"
    // without three glyphs shouting it all the time.
    for (action, button) in Screen::title_buttons(rect) {
        let hovered = button.contains(pointer.0, pointer.1);
        if hovered {
            let chip = if action == Title::Close { ui::danger() } else { ui::pressed() };
            canvas.fill_round_rect(button.inset(ui::sc(4)), ui::radius_small(), chip);
        }

        let ink = match (hovered, focused) {
            (true, _) => ui::text(),
            (false, true) => ui::muted(),
            (false, false) => ui::border(),
        };
        let (cx, cyf) = (
            (button.x + button.w / 2) as f32,
            cy as f32,
        );
        let r = ui::sc(4) as f32;
        let t = ui::sc(1).max(1);
        match action {
            // Points at the dock the window is about to join.
            Title::Minimize => {
                canvas.stroke_line(cx - r, cyf - r * 0.4, cx, cyf + r * 0.5, t, ink);
                canvas.stroke_line(cx, cyf + r * 0.5, cx + r, cyf - r * 0.4, t, ink);
            }
            // Corner brackets pushing outward; inward when there is nowhere
            // further out to go.
            Title::Maximize => {
                let d = if maximized { -r * 0.35 } else { 0.0 };
                canvas.stroke_line(cx - r + d, cyf - r * 0.3 + d, cx - r + d, cyf - r + d, t, ink);
                canvas.stroke_line(cx - r + d, cyf - r + d, cx - r * 0.3 + d, cyf - r + d, t, ink);
                canvas.stroke_line(cx + r - d, cyf + r * 0.3 - d, cx + r - d, cyf + r - d, t, ink);
                canvas.stroke_line(cx + r - d, cyf + r - d, cx + r * 0.3 - d, cyf + r - d, t, ink);
            }
            Title::Close => {
                canvas.stroke_line(cx - r * 0.9, cyf - r * 0.9, cx + r * 0.9, cyf + r * 0.9, t, ink);
                canvas.stroke_line(cx - r * 0.9, cyf + r * 0.9, cx + r * 0.9, cyf - r * 0.9, t, ink);
            }
            Title::Drag => {}
        }
    }

    let style = Style { size: 13.0 * ui::scale(), ..Style::default() };
    let ink = if focused { ui::text() } else { ui::muted() };
    let title = client.title();
    // The icon leads and the title follows it. The title starts at the bar's
    // text inset rather than being centred: centred text between asymmetric
    // furniture never quite looks centred, and a left-anchored title is where
    // the drag region unambiguously begins.
    let mut x = bar.x + ui::sc(10);
    if let Some(icon) = icon {
        canvas.blend_pixmap(icon.as_ref(), x, bar.y + (bar.h - title_icon()) / 2);
        x += title_icon() + ui::sc(6);
    }
    let clip = Rect::new(bar.x, bar.y, bar.w - title_button_w() * 3 - ui::sc(6), bar.h);
    canvas.clipped(clip, |canvas| {
        canvas.draw_text(fonts, title, x, cy - fonts.line_height(&style) / 2, &style, ink);
    });

    let content = content_of(rect);
    canvas.clipped(content, |canvas| client.draw(canvas, fonts, &self.images));

    canvas.fill_rect(Rect::new(bar.x, bar.y + bar.h - 1, bar.w, 1), ui::border());
    canvas.stroke_round_rect(rect, ui::radius_window(), 1, ui::border());
}
}

/// Keys the compositor keeps for itself, before anything is routed.
pub fn compositor_key(key: Key) -> Option<CompositorKey> {
    match key {
        Key::Other(59) => Some(CompositorKey::CycleWorkspace),
        Key::Other(60) => Some(CompositorKey::ToggleDebug),
        Key::Other(61) => Some(CompositorKey::TogglePane),
        _ => None,
    }
}

pub enum CompositorKey {
    /// F1. Cycles workspaces from the keyboard, the way the tabs do by pointer.
    CycleWorkspace,
    /// F2. The diagnostic overlay.
    ToggleDebug,
    /// F3. Fold the conversation pane away, for when the window needs the room.
    TogglePane,
}

#[cfg(test)]
mod tests {
    use super::*;
    use awproto::{Decoder, encode};
    use std::io::{self, Read, Write};
    use std::os::fd::{AsRawFd, OwnedFd};

    /// One frame off a socket, blocking. The peers here are stand-ins for an
    /// application and an agent, and both are being driven a step at a time,
    /// so there is always either something to read or a bug.
    fn frame(stream: &mut UnixStream) -> Vec<String> {
        let mut decoder = Decoder::with_limit(1024 * 1024);
        let mut buf = [0u8; 8192];
        loop {
            if let Some(fields) = decoder.next_frame().expect("a frame") {
                return fields;
            }
            let read = stream.read(&mut buf).expect("the peer is still there");
            assert!(read > 0, "the connection closed with nothing said");
            decoder.feed(&buf[..read]);
        }
    }

    const SHEET: &str = "<window title=\"Sheet\" pad=\"none\">\
         <spreadsheet id=\"sheet\" grow=\"true\" source=\"book\" version=\"2\" \
         rows=\"1000\" columns=\"26\" cursor=\"A1\" description=\"The grid\"/></window>";

    /// The whole of `query cells`: an agent asks for a rectangle and is
    /// answered out of the sheet the compositor holds, without the
    /// application being asked anything at all.
    ///
    /// That is the change. It used to be a round trip — the compositor asked
    /// the application to describe a different window, held the agent's reply
    /// until a fresh tree arrived, and answered with a view of four hundred
    /// cell elements. The cells are the compositor's now, so a read is a
    /// lookup, and what comes back is the values rather than the markup.
    #[test]
    fn an_agent_reads_cells_without_asking_the_application() {
        ui::set_scale(1.0);
        let fonts = Fonts::load().expect("the faces are compiled in");
        let mut screen = Screen::new(Rect::new(0, 0, 1200, 800), &fonts);

        let (mut app, app_end) = UnixStream::pair().expect("a socketpair");
        let app_fd = app_end.as_raw_fd();
        screen
            .attach(
                &fonts,
                &["app-attached".into(), "1".into(), "awsheet".into(), "0".into()],
                OwnedFd::from(app_end),
            )
            .expect("the application attached");

        let (mut agent, agent_end) = UnixStream::pair().expect("a socketpair");
        let agent_fd = agent_end.as_raw_fd();
        screen
            .attach(
                &fonts,
                &["agent-attached".into(), "1".into(), "0".into()],
                OwnedFd::from(agent_end),
            )
            .expect("the agent attached");

        // The application publishes two rows, one of them five hundred rows
        // down, and then the tree that claims their version. Cells first: one
        // connection, so the compositor can never hold a tree ahead of its
        // data.
        app.write_all(&encode(&["sheet", "book", "1", "0", "A1", "Region", "Q1"])).unwrap();
        app.write_all(&encode(&["sheet", "book", "2", "1", "A500", "far down"])).unwrap();
        app.write_all(&encode(&["render", "1", SHEET])).unwrap();
        screen.readable(app_fd, &fonts);

        // The agent asks for a rectangle nowhere near the top of the sheet.
        agent
            .write_all(&encode(&["query", "cells", "awsheet", "sheet", "A500:B500"]))
            .unwrap();
        let progress = screen.readable(agent_fd, &fonts).expect("the agent was read");
        screen.requests(&fonts, agent_fd, progress.requests);

        let reply = frame(&mut agent);
        assert_eq!(reply[0], agent::MSG_CELLS, "not a block of cells: {reply:?}");
        assert_eq!(reply[1], "awsheet");
        assert_eq!(reply[3], "A500:B500");
        assert_eq!(reply[4], "far down\t\n", "the wrong cells came back: {:?}", reply[4]);

        // And the application was asked nothing. There is nothing to ask.
        app.set_nonblocking(true).unwrap();
        let mut spare = [0u8; 64];
        assert!(
            matches!(app.read(&mut spare), Err(err) if err.kind() == io::ErrorKind::WouldBlock),
            "the application was sent something"
        );

        // The view carries the shape and where the sheet is used, so an agent
        // knows what to ask for; it carries no cells at all.
        agent.write_all(&encode(&["query", "view", "awsheet"])).unwrap();
        let progress = screen.readable(agent_fd, &fonts).expect("the agent was read");
        screen.requests(&fonts, agent_fd, progress.requests);
        let reply = frame(&mut agent);
        assert_eq!(reply[0], agent::MSG_VIEW);
        assert!(reply[2].contains("rows=\"1000\""), "{}", reply[2]);
        // The bounding box of everything published, which is what tells an
        // agent where to look without reading a screenful to find out.
        assert!(reply[2].contains("used=\"A1:B500\""), "{}", reply[2]);
        assert!(!reply[2].contains("<cell"), "a cell reached the agent: {}", reply[2]);
    }

    /// The cross asks the application, and does nothing else.
    ///
    /// It used to be a SIGTERM through PID 1 with no warning, which is why this
    /// exists: an application with unsaved work had no moment in which to say
    /// so. What it must not do is end anything itself, so the assertion that
    /// matters as much as the event is the one that nothing was asked of PID 1.
    #[test]
    fn the_cross_asks_the_application_and_does_nothing_else() {
        ui::set_scale(1.0);
        let fonts = Fonts::load().expect("the faces are compiled in");
        let mut screen = Screen::new(Rect::new(0, 0, 1200, 800), &fonts);

        let (mut app, app_end) = UnixStream::pair().expect("a socketpair");
        let app_fd = app_end.as_raw_fd();
        screen
            .attach(
                &fonts,
                &["app-attached".into(), "1".into(), "awsheet".into(), "0".into()],
                OwnedFd::from(app_end),
            )
            .expect("the application attached");
        app.write_all(&encode(&["render", "1", "<window><text>x</text></window>"])).unwrap();
        screen.readable(app_fd, &fonts);

        // The press reaches the application, and reaches nobody else.
        assert!(screen.title_action(&fonts, app_fd, Title::Close, 0, 0));
        let said = frame(&mut app);
        assert_eq!(said[0], awproto::display::MSG_EVENT);
        assert_eq!(said[2], "", "the window is the one thing with no node to name");
        assert_eq!(said[3], awproto::display::ACTION_CLOSE);
        assert!(screen.requests.is_empty(), "the compositor asked PID 1 to end something");

        // Pressing it again asks again. There is no second press that closes
        // the window regardless, and nothing here can end a process: the
        // application exits when it is ready, and the window goes when its
        // connection does.
        assert!(screen.title_action(&fonts, app_fd, Title::Close, 0, 0));
        assert_eq!(frame(&mut app)[3], awproto::display::ACTION_CLOSE);
        assert!(screen.requests.is_empty(), "a second press forced the window shut");
    }
}
