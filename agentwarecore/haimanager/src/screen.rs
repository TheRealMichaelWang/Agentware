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

use std::collections::{HashMap, VecDeque};
use std::os::fd::{OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use awproto::agent;

use crate::client::{Client, Kind, Progress};
use crate::clipboard::Clipboard;
use crate::cursor;
use crate::document::Document;
use crate::images::Images;
use crate::startmenu::{AGENTWARE_ICON, AGENTWARE_SVG, StartMenu, StartOutcome};
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
fn tab_close_w() -> i32 { ui::sc(20) }

/// How close to an edge a press counts as taking hold of it.
fn resize_band() -> i32 { ui::sc(7) }
/// Smaller than this and a window is all chrome.
fn min_window() -> (i32, i32) { (ui::sc(320), ui::sc(200)) }

/// How long the fake cursor takes to travel to what an agent named.
///
/// Long enough for a human to follow, which is the entire point of it. An agent
/// that acted instantly would be indistinguishable from one that had never
/// shown its work, and the visible embodiment VISION.md promises would be a
/// claim rather than something on screen.
const FLIGHT: Duration = Duration::from_millis(600);

/// How long an agent's `rows` query waits for the application to describe a
/// different window before it is answered with whatever is on screen.
///
/// Long enough for a re-render, short enough that an application which
/// ignores the request costs the agent a pause rather than a hang. The reply
/// carries `first-row`, so an agent can always see whether it moved.
const ROWS_WAIT: Duration = Duration::from_millis(400);

/// How long the conversation pane takes to fold away or return.
const PANE_FOLD: Duration = Duration::from_millis(200);

/// Half a caret blink: lit for this long, dark for this long.
///
/// Restarted by every keystroke, so the caret is solid while someone is typing
/// and only blinks while the field is waiting.
const BLINK: Duration = Duration::from_millis(530);

/// Time between characters when an agent enters text.
///
/// An agent that set a field's value in one step would produce something no
/// human could have produced, and the application would receive one event where
/// a person types seventeen. Typing it out is both the honest synthesis and the
/// only way a human watching can read what is being entered.
const KEYSTROKE: Duration = Duration::from_millis(45);

/// How far an accepted intent has got.
enum Stage {
    /// The cursor is on its way to the target.
    Travelling,
    /// Characters are going in one at a time.
    Typing { done: usize, next: Instant },
}

/// An intent that has been accepted and is being performed.
///
/// It exists as state rather than as a blocking call because the compositor must
/// keep answering the human while it runs. The stop button and the navigation
/// bar stay live through the whole of it.
struct Flight {
    agent: RawFd,
    app: RawFd,
    app_name: String,
    desk: u32,
    target: String,
    action: String,
    value: String,
    from: (i32, i32),
    to: (i32, i32),
    started: Instant,
    stage: Stage,
}

/// An agent's `rows` query, waiting for the application to answer.
///
/// The compositor cannot describe rows nobody sent, so this is one of the
/// two places it asks an application for something and waits: it holds the
/// agent's reply until a fresh tree arrives or the wait runs out. The
/// version asked at is what tells a genuinely new tree from the one already
/// held.
struct PendingRows {
    agent: RawFd,
    app: RawFd,
    app_name: String,
    asked_at: u64,
    until: Instant,
}

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
    /// The client the pointer is dragging a text selection out of, on the
    /// same terms: a selection that stopped extending the moment the pointer
    /// left the field would be a selection nobody could finish.
    text_drag: Option<RawFd>,
    /// The client whose column edge is being dragged.
    column_drag: Option<RawFd>,
    /// What the human last copied. One per machine, held here because the
    /// compositor is the only process that sees both ends of a copy: it owns
    /// the keyboard the chord arrives on and the text being edited.
    clipboard: Clipboard,
    /// A tab held by the pointer. Whether it becomes a drag or a click is
    /// decided by whether it moves before it is released.
    tab_drag: Option<TabDrag>,
    /// The agentdesk tab being renamed: its workspace index, the text so far,
    /// and the width of the tab it replaced, so entering the editor does not
    /// change the tab's size under the click that opened it.
    renaming: Option<(usize, String, i32)>,
    /// The start menu, while it is open.
    start: Option<StartMenu>,

    /// The intent being performed, if any.
    flight: Option<Flight>,
    /// Agents waiting to be told what a table's new window holds.
    pending_rows: Vec<PendingRows>,
    /// Intents that arrived while one was in flight. An agent waits for its
    /// answer before sending the next, so this is a safety net rather than a
    /// pipeline.
    queued: VecDeque<(RawFd, Vec<String>)>,
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
            text_drag: None,
            column_drag: None,
            clipboard: Clipboard::default(),
            tab_drag: None,
            renaming: None,
            start: None,
            flight: None,
            pending_rows: Vec::new(),
            queued: VecDeque::new(),
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

        let pid: i32 = match kind {
            Kind::App => field(3).parse().unwrap_or(0),
            _ => field(2).parse().unwrap_or(0),
        };

        // The name arrived in the supervisor's handoff tag, which is the only
        // identity an app has; its icon is read from the package under that
        // name, so an app cannot wear another's.
        if kind == Kind::App {
            self.images.icons.prepare(&name, &[title_icon(), dock_icon()]);
        }

        let client = Client::adopt(kind, desk, name, pid, UnixStream::from(fd))
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
        if self.flight.as_ref().is_some_and(|f| f.agent == fd || f.app == fd) {
            self.flight = None;
        }
        self.queued.retain(|(from, _)| *from != fd);
        // An agent or an application that has gone leaves nobody to answer
        // and nothing to answer with.
        self.pending_rows.retain(|pending| pending.agent != fd && pending.app != fd);
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
        let progress = self.client_mut(fd)?.readable(fonts);
        if progress.first {
            self.fit_window(fd, fonts);
        }
        // A workspace's title can change with its tree, and the navigation bar
        // shows it.
        if progress.dirty {
            self.nav = None;
        }
        // A fresh tree may be the window an agent is waiting on.
        self.settle_rows();
        Some(progress)
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
                self.nav_release(fonts)
            }

            Event::ButtonPressed { button: Button::Left, x, y } => {
                self.blink_epoch = Instant::now();
                self.click(fonts, x, y)
            }

            Event::Scrolled { delta, x, y } => match self.surface_at(x, y) {
                Some(Surface::App(fd)) => self.route_to(fd, fonts, event),
                Some(Surface::Desk) => self.route_desk(fonts, event),
                Some(Surface::Start) => self.start_wheel(fonts, delta, x, y),
                _ => false,
            },

            Event::KeyPressed(key) => {
                self.blink_epoch = Instant::now();
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

            // Lifetime belongs to PID 1, so closing a window is a request rather
            // than a socket the compositor drops. Dropping it would leave a
            // process alive with nothing to draw on and nobody tracking it.
            Title::Close => {
                let desk = self.workspaces[at].id;
                let pid = self.client(fd).map(|client| client.pid).unwrap_or(0);
                self.requests
                    .push(vec!["close-app".into(), desk.to_string(), pid.to_string()]);
                self.notes.push(format!("workspace {desk}: closing pid {pid}"));
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
        let Some((scrolling, selecting, sizing)) = self
            .client(fd)
            .map(|client| (client.scroll_dragging(), client.selecting(), client.sizing_column()))
        else {
            return;
        };
        if scrolling {
            self.scroll_drag = Some(fd);
        }
        if selecting {
            self.text_drag = Some(fd);
        }
        if sizing {
            self.column_drag = Some(fd);
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
        dirty
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

            // Read a different part of a table. A query rather than an
            // action because it is a read, and because there is nothing to
            // act on: a cell outside the window is not in the tree, so an
            // intent naming one has no target to resolve.
            (agent::MSG_QUERY, agent::MSG_ROWS) => {
                let (app, table, row) =
                    (field(2).to_owned(), field(3).to_owned(), field(4).to_owned());
                self.read_rows(from, &app, &table, &row)
            }

            (agent::MSG_INTENT, _) => {
                if self.flight.is_some() {
                    self.queued.push_back((from, fields.to_vec()));
                    return false;
                }
                self.begin(fonts, from, fields)
            }

            (other, _) => {
                self.notes.push(format!("agent sent {other:?}, which is not a request"));
                false
            }
        }
    }

    /// Ask an application for a different window of one of its tables, and
    /// answer the agent once it has described it.
    fn read_rows(&mut self, from: RawFd, app: &str, table: &str, row: &str) -> bool {
        let refuse = |screen: &mut Screen, reason: &str| {
            let markup = format!("<rejected target=\"{table}\" reason=\"{reason}\"/>");
            screen.reply(from, &[agent::MSG_VIEW, app, &markup]);
        };

        let Some(at) = self.agent_workspace(from) else {
            refuse(self, agent::REASON_NOT_ADDRESSABLE);
            return false;
        };
        let Some(app_fd) = self.app_in(at, app) else {
            let reason = self.why_not(at, app);
            refuse(self, reason);
            return false;
        };
        let Ok(row) = row.parse::<i32>() else {
            refuse(self, agent::REASON_UNSUPPORTED);
            return false;
        };
        let Some(index) = self.client(app_fd).and_then(|client| client.node_by_id(table)) else {
            refuse(self, agent::REASON_NO_SUCH_NODE);
            return false;
        };

        // Already showing it: answer at once rather than asking for a window
        // the application is already describing and waiting to be told so.
        let showing = self.client(app_fd).and_then(|client| client.number(index, "first-row"));
        if showing == Some(row.max(0)) {
            self.answer_view(from, app_fd, app);
            return false;
        }

        let version = self.client(app_fd).map_or(0, Client::version);
        let asked = self
            .client_mut(app_fd)
            .map(|client| client.ask_for_row(index, row))
            .unwrap_or(false);
        if !asked {
            // Not a table, or one with no id to address. What is on screen is
            // still a true answer to what the agent asked to see.
            self.answer_view(from, app_fd, app);
            return false;
        }

        self.pending_rows.push(PendingRows {
            agent: from,
            app: app_fd,
            app_name: app.to_owned(),
            asked_at: version,
            until: Instant::now() + ROWS_WAIT,
        });
        self.notes.push(format!("agent asked {app} for row {row} of {table}"));
        false
    }

    fn answer_view(&mut self, to: RawFd, app_fd: RawFd, app: &str) {
        let markup = self
            .client(app_fd)
            .and_then(Client::agent_view)
            .unwrap_or_default();
        self.reply(to, &[agent::MSG_VIEW, app, &markup]);
    }

    /// Answer every waiting `rows` query whose application has re-rendered,
    /// or whose wait has run out.
    fn settle_rows(&mut self) {
        if self.pending_rows.is_empty() {
            return;
        }
        let now = Instant::now();
        let ready: Vec<usize> = self
            .pending_rows
            .iter()
            .enumerate()
            .filter(|(_, pending)| {
                let answered = self
                    .client(pending.app)
                    .map(|client| client.version() > pending.asked_at);
                // An application that has gone is answered with nothing
                // rather than left holding the agent forever.
                answered.unwrap_or(true) || now >= pending.until
            })
            .map(|(at, _)| at)
            .collect();

        for at in ready.into_iter().rev() {
            let pending = self.pending_rows.remove(at);
            let app_name = pending.app_name.clone();
            self.answer_view(pending.agent, pending.app, &app_name);
        }
    }

    /// Which workspace an agent connection belongs to.
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
        let Some(index) = client.node_by_id(&target) else {
            self.refuse(from, &app, &target, agent::REASON_NO_SUCH_NODE);
            return false;
        };

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
        let reachable = self
            .client_mut(app_fd)
            .map(|client| client.reveal(fonts, index))
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
        let rect = client.rect_of(index);
        let to = (rect.x + rect.w / 2, rect.y + rect.h / 2);

        let desk = self.workspaces[at].id;
        let from_point = self.agent_cursor.filter(|(d, _, _)| *d == desk).map_or(
            (self.regions().apps.x + 20, self.regions().apps.y + 20),
            |(_, x, y)| (x, y),
        );

        self.flight = Some(Flight {
            agent: from,
            app: app_fd,
            app_name: app,
            desk,
            target,
            action,
            value,
            from: from_point,
            to,
            started: Instant::now(),
            stage: Stage::Travelling,
        });
        true
    }

    /// True while something is mid-animation, so the loop should wake for
    /// frames rather than sleeping until the next event.
    pub fn wants_frame(&self) -> bool {
        self.flight.is_some()
            || self.panes_moving()
            || self.clients.iter().any(Client::animating)
    }

    /// Advance whatever is moving.
    pub fn tick(&mut self, fonts: &Fonts) -> bool {
        self.settle_rows();
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

        let Some(flight) = &self.flight else { return busy || settling || panes || blinked };

        match flight.stage {
            Stage::Travelling => {
                let elapsed = flight.started.elapsed();
                if elapsed >= FLIGHT {
                    self.land(fonts);
                    return true;
                }

                // Eased, because a pointer that moves at a constant speed and
                // stops dead does not read as a pointer. Travel only moves the
                // overlay: the scene under the flying cursor is not changing.
                let t = elapsed.as_secs_f32() / FLIGHT.as_secs_f32();
                let eased = 1.0 - (1.0 - t).powi(3);
                let x = flight.from.0 + ((flight.to.0 - flight.from.0) as f32 * eased) as i32;
                let y = flight.from.1 + ((flight.to.1 - flight.from.1) as f32 * eased) as i32;
                self.agent_cursor = Some((flight.desk, x, y));
                self.overlay_dirty = true;
                false
            }

            Stage::Typing { done, next } => {
                if Instant::now() >= next {
                    self.type_one(fonts, done);
                }
                true
            }
        }
    }

    /// The cursor has arrived. Start typing, or synthesize the event.
    fn land(&mut self, fonts: &Fonts) -> bool {
        let Some(flight) = &mut self.flight else { return false };
        self.agent_cursor = Some((flight.desk, flight.to.0, flight.to.1));

        // Text is entered a character at a time, from here on. Every other
        // action happens at the moment the cursor arrives, as a click does.
        if flight.action == "type-text" && !flight.value.is_empty() {
            flight.stage = Stage::Typing { done: 0, next: Instant::now() };
            return true;
        }

        let Some(flight) = self.flight.take() else { return false };
        let outcome = self.apply(fonts, flight.app, &flight.target, &flight.action, &flight.value);
        self.settle(fonts, flight, outcome);
        true
    }

    /// Put in one more character.
    fn type_one(&mut self, fonts: &Fonts, done: usize) -> bool {
        let (app, target, action, total, prefix) = {
            let Some(flight) = &self.flight else { return false };
            (
                flight.app,
                flight.target.clone(),
                flight.action.clone(),
                flight.value.chars().count(),
                flight.value.chars().take(done + 1).collect::<String>(),
            )
        };

        // Every keystroke is its own event, exactly as a human's would be, so an
        // application sees a value growing rather than one appearing, and the
        // caret stays solid exactly as it does under a human's typing.
        self.blink_epoch = Instant::now();
        let outcome = self.apply(fonts, app, &target, &action, &prefix);
        if outcome.is_some() || done + 1 >= total {
            let Some(flight) = self.flight.take() else { return false };
            self.settle(fonts, flight, outcome);
            return true;
        }

        if let Some(flight) = &mut self.flight {
            flight.stage = Stage::Typing { done: done + 1, next: Instant::now() + KEYSTROKE };
        }
        true
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
        action: &str,
        value: &str,
    ) -> Option<&'static str> {
        match self.client_mut(app) {
            Some(client) => match client.node_by_id(target) {
                Some(index) => client.act(fonts, index, action, value).err(),
                None => Some(agent::REASON_NO_SUCH_NODE),
            },
            None => Some(agent::REASON_NO_SUCH_APP),
        }
    }

    /// Answer the agent, and start whatever was waiting behind this.
    fn settle(&mut self, fonts: &Fonts, flight: Flight, outcome: Option<&'static str>) {
        match outcome {
            None => {
                self.notes.push(format!(
                    "agent performed {} on {} in {}",
                    flight.action, flight.target, flight.app_name
                ));
                self.confirm(flight.agent, &flight.app_name, &flight.target, &flight.action);
            }
            Some(reason) => {
                self.refuse(flight.agent, &flight.app_name, &flight.target, reason)
            }
        }

        if let Some((from, fields)) = self.queued.pop_front() {
            self.begin(fonts, from, &fields);
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
    fn nav_markup(&self) -> String {
        let mut out = String::from(
            "<window font=\"sans\" pad=\"none\" size=\"sm\">\n  <hstack gap=\"sm\">\n",
        );

        for (at, workspace) in self.workspaces.iter().enumerate() {
            if let Some((renaming, buffer, width)) = &self.renaming
                && *renaming == at
            {
                out.push_str(&format!(
                    "    <field id=\"nav-rename\" value=\"{value}\" width=\"{width}\" \
                     description=\"The name being typed for this agentdesk\"/>\n",
                    value = awproto::display::escape(buffer),
                ));
                continue;
            }

            // Trailing spaces reserve the room the close glyph is drawn into.
            // The glyph is compositor paint over the button, the way window
            // controls are, so the label must leave it a landing zone.
            let busy = if workspace.agent.is_some() { " *" } else { "" };
            out.push_str(&format!(
                "    <button id=\"nav-desk-{id}\" label=\"{name}{busy}    \"{emphasis} \
                 description=\"Switches to this agentdesk\"/>\n",
                id = workspace.id,
                name = awproto::display::escape(&self.tab_name(workspace)),
                emphasis = if at == self.current { " emphasis=\"primary\"" } else { "" },
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

        out.push_str("  </hstack>\n</window>\n");
        out
    }

    fn build_nav(&mut self, fonts: &Fonts) {
        let markup = self.nav_markup();
        let Ok(doc) = Document::parse(&markup, 0) else { return };

        // The frame is the bar minus a margin that centres one row of small
        // controls, computed rather than hoped. The earlier version handed the
        // whole bar to a padded window, and the padding pushed the buttons past
        // the bottom edge, which is why the tabs looked like they were bleeding
        // off the screen.
        let bar = self.nav_rect();
        let row = fonts.line_height(&Style { size: 11.0 * ui::scale(), ..Style::default() }) + 12;
        let inset_y = ((bar.h - row) / 2).max(2);
        let frame = Rect::new(bar.x + 10, bar.y + inset_y, bar.w - 20, bar.h - inset_y * 2);
        self.nav_layout = ui::layout_chrome(fonts, &doc, &Frame::Whole(frame), &mut self.nav_scroll);
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
            if x >= rect.x + rect.w - tab_close_w() {
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
            self.renaming = Some((at, name, width));
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
        // Where the pointer falls among the other tabs' centres is the slot
        // this one belongs in.
        let slot = tabs
            .iter()
            .filter(|(tab, _)| *tab != id)
            .filter(|(_, rect)| x > rect.x + rect.w / 2)
            .count();

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
        let Some((_, buffer, _)) = &mut self.renaming else { return false };
        match key {
            Key::Char(c) => buffer.push(c),
            Key::Backspace => {
                buffer.pop();
            }
            Key::Enter => {
                self.commit_rename(fonts);
                return true;
            }
            Key::Escape => {
                self.renaming = None;
            }
            _ => return false,
        }
        self.nav = None;
        true
    }

    /// Settle a rename in progress: the trimmed text becomes the name, and an
    /// emptied field falls back to the default rather than keeping a blank tab.
    fn commit_rename(&mut self, fonts: &Fonts) {
        let Some((at, buffer, _)) = self.renaming.take() else { return };
        if let Some(workspace) = self.workspaces.get_mut(at) {
            let trimmed = buffer.trim();
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
            StartOutcome::CreateDesk(prompt) => {
                match prompt {
                    Some(text) => self.requests.push(vec!["create-desk".into(), text]),
                    None => self.requests.push(vec!["create-desk".into()]),
                }
                self.notes.push("new agentdesk requested from the start menu".into());
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
        let outcome = menu.click(x, y);
        self.start_outcome(fonts, outcome)
    }

    fn start_key(&mut self, fonts: &Fonts, key: Key) -> bool {
        let Some(menu) = &mut self.start else { return false };
        let outcome = menu.key(key);
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
        canvas.fill_rect(rect, ui::raised());
        let Some(doc) = &self.nav else { return };

        // While a tab is being renamed its field carries the caret, on the same
        // blink clock as every other caret: solid while keys arrive, blinking
        // while the field waits.
        let focus = match &self.renaming {
            Some((_, buffer, _)) => Focus {
                node: doc.index_of("#nav-rename"),
                caret: buffer.chars().count(),
                caret_visible: self.caret_phase(),
                ..Focus::default()
            },
            None => Focus::default(),
        };

        canvas.clipped(rect, |canvas| {
            ui::paint_subtree(canvas, fonts, &self.images, &doc.tree, &self.nav_layout, Document::ROOT, &focus)
        });

        // Close glyphs, drawn over each tab's reserved right end the way window
        // controls are drawn over windows: chrome on chrome, not markup.
        let current_id = self.workspaces.get(self.current).map(|w| w.id);
        let dragging = self
            .tab_drag
            .as_ref()
            .filter(|held| held.moved)
            .map(|held| (held.id, held.at_x - held.grab_dx));
        for (id, tab) in self.nav_tab_rects() {
            if dragging.is_some_and(|(dragged, _)| dragged == id) {
                continue;
            }
            Self::draw_tab_close(canvas, tab, current_id == Some(id));
        }

        // The held tab is lifted out of the row and rides under the pointer.
        // Without this the only feedback was the row rearranging once the
        // pointer crossed a neighbour's midpoint, which for the first half of
        // any drag is no feedback at all: the mechanism worked and the
        // interaction still read as dead.
        if let Some((id, ghost_x)) = dragging
            && let Some((_, home)) = self.nav_tab_rects().into_iter().find(|(tab, _)| *tab == id)
            && let Some(workspace) = self.workspaces.iter().find(|w| w.id == id)
        {
            // Blank the tab's resting place so it reads as picked up. The nav
            // document's own window paints the background colour across the
            // bar, so that is what the empty slot has to be; the raised
            // colour here left a grey patch
            // over the black.
            canvas.fill_rect(home, ui::background());

            let ghost = Rect::new(ghost_x, home.y, home.w, home.h);
            let active = current_id == Some(id);
            canvas.shadow(ghost, ui::radius_control(), ui::sc(8), 110);
            canvas.fill_round_rect(ghost, ui::radius_control(), if active { ui::accent() } else { ui::pressed() });
            let style = Style { size: 11.0 * ui::scale(), ..Style::default() };
            let label = self.tab_name(workspace);
            canvas.clipped(ghost.inset(2), |canvas| {
                canvas.draw_text(
                    fonts,
                    &label,
                    ghost.x + ui::sc(12),
                    ghost.y + (ghost.h - fonts.line_height(&style)) / 2,
                    &style,
                    ui::text(),
                );
            });
            Self::draw_tab_close(canvas, ghost, active);
        }

        canvas.fill_rect(Rect::new(rect.x, rect.y + rect.h - 1, rect.w, 1), ui::border());
    }

    /// The close glyph at a tab's right end.
    fn draw_tab_close(canvas: &mut Canvas, tab: Rect, active: bool) {
        let cx = (tab.x + tab.w - tab_close_w() / 2 - ui::sc(2)) as f32;
        let cy = (tab.y + tab.h / 2) as f32;
        let r = ui::sc(3) as f32;
        let ink = if active { ui::text() } else { ui::muted() };
        let t = ui::sc(1).max(1);
        canvas.stroke_line(cx - r, cy - r, cx + r, cy + r, t, ink);
        canvas.stroke_line(cx - r, cy + r, cx + r, cy - r, t, ink);
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
    use awproto::display::ACTION_SCROLL;
    use awproto::{Decoder, encode};
    use std::io::{Read, Write};
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

    fn sheet(first: i32) -> String {
        let mut out = format!(
            "<window title=\"Sheet\" pad=\"none\"><table id=\"sheet\" grow=\"true\" \
             rows=\"1000\" first-row=\"{first}\" description=\"The grid\">\
             <column label=\"A\" chars=\"8\"/>"
        );
        for row in first..first + 3 {
            out.push_str(&format!(
                "<row label=\"{}\"><cell id=\"A{}\" value=\"r{}\"/></row>",
                row + 1,
                row + 1,
                row
            ));
        }
        out.push_str("</table></window>");
        out
    }

    /// The whole of `query rows`: an agent asks for a window it cannot see,
    /// the application is asked to describe it, and the agent is answered
    /// with the fresh view once it has.
    ///
    /// Worth an integration test rather than a unit one, because every part
    /// of it is the seam between two processes: the compositor asking, the
    /// application answering, and the agent's reply held in between.
    #[test]
    fn an_agent_reads_a_window_the_application_had_not_sent() {
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

        // The application describes rows 1 to 3 of a thousand.
        app.write_all(&encode(&["render", "1", &sheet(0)])).unwrap();
        screen.readable(app_fd, &fonts);

        // The agent asks to read from row 500, which is nowhere in that tree.
        agent
            .write_all(&encode(&["query", "rows", "awsheet", "sheet", "500"]))
            .unwrap();
        let progress = screen.readable(agent_fd, &fonts).expect("the agent was read");
        screen.requests(&fonts, agent_fd, progress.requests);

        // The application is asked, and nothing has been said to the agent
        // yet: there is nothing true to say until the answer arrives.
        let asked = frame(&mut app);
        assert_eq!(asked[0], "event", "the application was not asked: {asked:?}");
        assert_eq!(asked[3], ACTION_SCROLL, "asked for the wrong thing: {asked:?}");
        assert_eq!(asked[4], "500", "asked for the wrong row: {asked:?}");

        // It answers with that window, and the agent's reply follows.
        app.write_all(&encode(&["render", "2", &sheet(500)])).unwrap();
        screen.readable(app_fd, &fonts);

        let reply = frame(&mut agent);
        assert_eq!(reply[0], agent::MSG_VIEW, "not a view: {reply:?}");
        assert_eq!(reply[1], "awsheet");
        assert!(reply[2].contains("first-row=\"500\""), "the window did not move: {}", reply[2]);
        assert!(reply[2].contains("id=\"A501\""), "the rows are not the ones asked for: {}", reply[2]);
    }

    /// A window the application is already describing is answered at once,
    /// rather than asking for what is already there and waiting to be told.
    #[test]
    fn a_window_already_on_screen_is_answered_without_asking() {
        ui::set_scale(1.0);
        let fonts = Fonts::load().unwrap();
        let mut screen = Screen::new(Rect::new(0, 0, 1200, 800), &fonts);

        let (mut app, app_end) = UnixStream::pair().unwrap();
        let app_fd = app_end.as_raw_fd();
        screen
            .attach(
                &fonts,
                &["app-attached".into(), "1".into(), "awsheet".into(), "0".into()],
                OwnedFd::from(app_end),
            )
            .unwrap();
        let (mut agent, agent_end) = UnixStream::pair().unwrap();
        let agent_fd = agent_end.as_raw_fd();
        screen
            .attach(
                &fonts,
                &["agent-attached".into(), "1".into(), "0".into()],
                OwnedFd::from(agent_end),
            )
            .unwrap();

        app.write_all(&encode(&["render", "1", &sheet(500)])).unwrap();
        screen.readable(app_fd, &fonts);

        agent
            .write_all(&encode(&["query", "rows", "awsheet", "sheet", "500"]))
            .unwrap();
        let progress = screen.readable(agent_fd, &fonts).unwrap();
        screen.requests(&fonts, agent_fd, progress.requests);

        let reply = frame(&mut agent);
        assert_eq!(reply[0], agent::MSG_VIEW);
        assert!(reply[2].contains("first-row=\"500\""), "{}", reply[2]);
    }
}
