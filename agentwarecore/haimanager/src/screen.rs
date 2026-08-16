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
use crate::cursor;
use crate::document::Document;
use crate::icons::Icons;
use crate::input::{Button, Event, Key};
use crate::paint::font::{Family, Fonts, Style};
use crate::paint::{Canvas, Rect};
use crate::ui::{self, Focus, Frame, Layout, Regions};

/// Height of the navigation bar, which sits above every workspace.
fn nav_height() -> i32 { ui::sc(32) }
/// The agentdesk's taskbar region is parked at zero height for now. The strip
/// duplicated what the dock does and spent a full-width band saying so. The
/// region stays in the protocol and the layout path, so an agentdesk may still
/// declare content for it and nothing breaks; it simply gets no room until
/// there is a design worth giving room to.
const TASKBAR_HEIGHT: i32 = 0;
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
/// The strip along the bottom of the apps region holding every open window.
///
/// Taller than the old text-pill dock, because a tile now holds an icon with a
/// running dot beneath it rather than a line of text beside one.
fn dock_height() -> i32 { ui::sc(36) }
/// One square tile per window, sized to sit inside the dock with equal margin.
fn dock_tile() -> i32 { dock_height() - ui::sc(10) }
/// The icon inside a tile, leaving room below for the running dot.
fn dock_icon() -> i32 { ui::sc(18) }

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

/// Where the keyboard is pointed.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Surface {
    Nav,
    Desk,
    App(RawFd),
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
    /// App icons, parsed and rasterized when an app attaches so drawing them
    /// is a lookup. Cached across windows and workspaces by app name.
    icons: Icons,
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
    /// A tab held by the pointer. Whether it becomes a drag or a click is
    /// decided by whether it moves before it is released.
    tab_drag: Option<TabDrag>,
    /// The agentdesk tab being renamed: its workspace index, the text so far,
    /// and the width of the tab it replaced, so entering the editor does not
    /// change the tab's size under the click that opened it.
    renaming: Option<(usize, String, i32)>,

    /// The intent being performed, if any.
    flight: Option<Flight>,
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
    pub fn new(bounds: Rect) -> Screen {
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
            icons: Icons::new(),
            requests: Vec::new(),
            notes: Vec::new(),
            drag: None,
            scroll_drag: None,
            tab_drag: None,
            renaming: None,
            flight: None,
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
        Regions::carve(area, TASKBAR_HEIGHT, pane)
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
            Surface::Nav => None,
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
        if !in_client && self.renaming.is_none() {
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

    /// Where windows may go: the apps region, less the dock if it is showing.
    fn window_area(&self, at: usize) -> Rect {
        let apps = self.regions_for(at);
        let apps = apps.apps;
        if self.dock_pills(at).is_empty() {
            return apps;
        }
        Rect::new(apps.x, apps.y, apps.w, apps.h - dock_height() - 8)
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

    /// Size a window to what its content asks for, at the narrowest width a
    /// resize would allow.
    ///
    /// Runs once per client, when its first tree arrives. The width is the
    /// resize minimum rather than anything measured, because AWML describes
    /// affordances rather than arrangement and has no natural width to ask for;
    /// the height is the measure pass at that width plus the title bar, so
    /// every element is visible without dead room below. The clamp against the
    /// workspace is what a long list runs into, and its scroll container takes
    /// over from there.
    ///
    /// First trees only. An application that re-renders taller does not get to
    /// move a window the human may have already taken hold of.
    fn fit_window(&mut self, fd: RawFd, fonts: &Fonts) {
        let (min_w, min_h) = min_window();
        let Some(content) = self.client(fd).and_then(|c| c.natural_height(fonts, min_w)) else {
            return;
        };
        let Some(at) = self
            .workspaces
            .iter()
            .position(|workspace| workspace.windows.iter().any(|window| window.fd == fd))
        else {
            return;
        };

        let area = self.window_area(at);
        let Some(window) = self.workspaces[at].windows.iter_mut().find(|w| w.fd == fd) else {
            return;
        };
        if window.maximized {
            return;
        }

        let room = (area.y + area.h - window.rect.y).max(min_h);
        window.rect.w = min_w;
        window.rect.h = (content + window_title_h()).clamp(min_h, room);
        window.restored = window.rect;
        self.reframe(fonts);
    }

    /// The strip of minimized windows along the bottom of the apps region.
    ///
    /// Drawn by the compositor rather than put in the agentdesk's taskbar,
    /// because whether a window is minimized is compositor state and the
    /// agentdesk is never told that windows exist at all.
    /// The dock: one pill per open window, floating at the bottom of the apps
    /// region.
    ///
    /// Every window rather than only the minimized ones, because switching
    /// between windows is what a dock is for and half a switcher is worse than
    /// none. It is compositor chrome for the same reason the title bars are:
    /// which window is where is not something the agentdesk is told.
    fn dock_rect(&self, at: usize) -> Rect {
        let apps = self.regions_for(at).apps;
        let count = self
            .workspaces
            .get(at)
            .map(|workspace| workspace.windows.len())
            .unwrap_or(0) as i32;
        let width = (dock_tile() + 6) * count + 6;
        Rect::new(
            apps.x + (apps.w - width) / 2,
            apps.y + apps.h - dock_height() - 8,
            width,
            dock_height(),
        )
    }

    /// Where each window's pill sits.
    fn dock_pills(&self, at: usize) -> Vec<(RawFd, Rect)> {
        let Some(workspace) = self.workspaces.get(at) else { return Vec::new() };
        if workspace.windows.is_empty() {
            return Vec::new();
        }

        let dock = self.dock_rect(at);
        let inset = (dock.h - dock_tile()) / 2;
        let mut x = dock.x + 6;
        // In the order they opened rather than in z-order, so a tile does not
        // move under the pointer when the window behind it is raised.
        let mut pills: Vec<(RawFd, Rect)> = workspace
            .windows
            .iter()
            .map(|window| {
                let pill = Rect::new(x, dock.y + inset, dock_tile(), dock_tile());
                x += dock_tile() + 6;
                (window.fd, pill)
            })
            .collect();
        pills.sort_by_key(|(fd, _)| *fd);
        let mut x = dock.x + 6;
        for (_, pill) in &mut pills {
            pill.x = x;
            x += dock_tile() + 6;
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
            self.icons.prepare(&name, &[title_icon(), dock_icon()]);
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
        self.workspaces
            .retain(|workspace| workspace.desk.is_some() || !workspace.windows.is_empty());
        self.current = self.current.min(self.workspaces.len().saturating_sub(1));

        if self.focus == Surface::App(fd) {
            self.focus = Surface::Desk;
        }

        // An agent that has gone leaves no pointer behind, and an intent whose
        // agent or target has gone has nobody to answer and nothing to act on.
        if self.flight.as_ref().is_some_and(|f| f.agent == fd || f.app == fd) {
            self.flight = None;
        }
        self.queued.retain(|(from, _)| *from != fd);
        if self.clients.iter().all(|client| client.kind != Kind::Agent) {
            self.agent_cursor = None;
        }

        self.reframe(fonts);
        self.nav = None;
        Some(label)
    }

    pub fn client_mut(&mut self, fd: RawFd) -> Option<&mut Client> {
        self.clients.iter_mut().find(|client| client.fd() == fd)
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
                    client.drag_scroll(fonts, y);
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
                self.nav_release(fonts)
            }

            Event::ButtonPressed { button: Button::Left, x, y } => {
                self.blink_epoch = Instant::now();
                self.click(fonts, x, y)
            }

            Event::Scrolled { delta, x, y } => match self.surface_at(x, y) {
                Some(Surface::App(fd)) => self.route_to(fd, fonts, event),
                Some(Surface::Desk) => self.route_desk(fonts, event),
                _ => {
                    let _ = delta;
                    false
                }
            },

            Event::KeyPressed(key) => {
                self.blink_epoch = Instant::now();
                match self.focus {
                    Surface::App(fd) => self.route_to(fd, fonts, event),
                    Surface::Desk => self.route_desk(fonts, event),
                    Surface::Nav => self.nav_key(fonts, key),
                }
            }

            Event::KeyReleased(_) => match self.focus {
                Surface::App(fd) => self.route_to(fd, fonts, event),
                Surface::Desk => self.route_desk(fonts, event),
                Surface::Nav => false,
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
                    // Not an error and not silent. The human is being told the
                    // workspace is being driven, not that their click was lost.
                    self.notes.push(format!(
                        "workspace {}: apps are frozen while an agent is running",
                        self.workspaces[self.current].id
                    ));
                    return true;
                }
                self.raise(self.current, fd);
                self.focus = Surface::App(fd);
                let dirty =
                    self.route_to(fd, fonts, Event::ButtonPressed { button: Button::Left, x, y });
                if self.client(fd).is_some_and(Client::scroll_dragging) {
                    self.scroll_drag = Some(fd);
                }
                dirty
            }

            Surface::Desk => {
                // Both of these are compositor chrome sitting over the
                // workspace, so they are checked before the click is handed to
                // it.
                if self.pane_handle(self.current).contains(x, y) {
                    return self.toggle_pane(fonts);
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

                self.focus = Surface::Desk;
                let desk = self.workspaces.get(self.current).and_then(|w| w.desk);
                let dirty =
                    self.route_desk(fonts, Event::ButtonPressed { button: Button::Left, x, y });
                if let Some(fd) = desk
                    && self.client(fd).is_some_and(Client::scroll_dragging)
                {
                    self.scroll_drag = Some(fd);
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
                // The dock appearing takes room from the windows above it.
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
        self.client_mut(fd)
            .map(|client| client.handle(fonts, event))
            .unwrap_or(false)
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

        // `scroll-into-view` is the one action the compositor performs itself,
        // and the one that targets any node rather than only a control. With
        // arrangement automatic, scrolling is the one way a node can still be
        // out of sight, and this is the way out. The agent says what it wants
        // to be true and not how to bring it about.
        if action == "scroll-into-view" {
            let moved = self
                .client_mut(app_fd)
                .map(|client| client.reveal(fonts, index))
                .unwrap_or(false);
            self.confirm(from, &app, &target, &action);
            self.notes.push(format!("agent revealed {target} in {app}"));
            return moved || true;
        }

        if client.is_disabled(index) {
            self.refuse(from, &app, &target, agent::REASON_DISABLED);
            return false;
        }
        if client.needs_approval(index) {
            self.refuse(from, &app, &target, agent::REASON_NEEDS_APPROVAL);
            return false;
        }
        // Scrolled out of its own container.
        if !client.is_visible(index) {
            self.refuse(from, &app, &target, agent::REASON_NOT_VISIBLE);
            return false;
        }

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
            "<window font=\"sans\" pad=\"none\" size=\"sm\">\n  <hstack gap=\"sm\">\n\
             \x20   <button id=\"nav-home\" label=\"Home\" \
             description=\"Opens the start menu, where a new workspace is created\"/>\n",
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
        self.nav_layout = ui::layout(fonts, &doc, &Frame::Whole(frame), &mut self.nav_scroll);
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

        if id == "nav-home" {
            self.notes.push("the start menu does not exist yet".into());
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

        canvas.fill_rect(canvas.bounds(), ui::BACKGROUND);

        match self.workspaces.get(self.current) {
            Some(_) => self.draw_workspace(canvas, fonts, pointer),
            None => self.draw_empty(canvas, fonts),
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

        canvas.fill_rect(regions.background, ui::BACKGROUND);
        if let Some(desk) = desk {
            canvas.clipped(regions.background, |canvas| {
                desk.draw_region(canvas, fonts, "background")
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
            let Some(client) = self.client(window.fd) else { continue };
            let icon = self.icons.get(&client.name, title_icon());
            canvas.clipped(regions.apps, |canvas| {
                draw_window(canvas, fonts, client, icon, window, focused, pointer)
            });
        }

        self.draw_dock(canvas, fonts);

        // Only the pane has a painted background now; the taskbar region has no
        // height and the dock is its own floating surface.
        let rect = regions.pane;
        if rect.w > 0 {
            canvas.fill_rect(rect, ui::SURFACE);
            canvas.fill_rect(Rect::new(rect.x, rect.y, 1, rect.h), ui::BORDER);
            if let Some(desk) = desk {
                canvas.clipped(rect, |canvas| desk.draw_region(canvas, fonts, "pane"));
            }
        }

        self.draw_pane_handle(canvas);
    }

    /// The dock: one icon tile per open window, floating over the apps region.
    fn draw_dock(&self, canvas: &mut Canvas, fonts: &Fonts) {
        let pills = self.dock_pills(self.current);
        if pills.is_empty() {
            return;
        }

        let dock = self.dock_rect(self.current);
        canvas.shadow(dock, ui::radius_surface(), ui::sc(14), 110);
        canvas.fill_round_rect_vgrad(dock, ui::radius_surface(), ui::lift(ui::SURFACE, 8), ui::SURFACE);
        canvas.stroke_round_rect(dock, ui::radius_surface(), 1, ui::BORDER);

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
                canvas.fill_round_rect(pill, ui::radius_control(), ui::RAISED);
            }

            match self.icons.get(&client.name, dock_icon()) {
                Some(icon) => {
                    canvas.blend_pixmap(
                        icon.as_ref(),
                        pill.x + (pill.w - dock_icon()) / 2,
                        pill.y + ui::sc(2),
                    );
                }
                None => {
                    let initial = client.title().chars().next().unwrap_or('?').to_string();
                    let ink = if minimized { ui::MUTED } else { ui::TEXT };
                    let x = pill.x + (pill.w - fonts.measure(&initial, &style)) / 2;
                    canvas.draw_text(
                        fonts,
                        &initial,
                        x,
                        pill.y + ui::sc(2),
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
                    ui::ACCENT,
                );
            }
        }
    }

    /// The grip that folds the conversation pane away.
    fn draw_pane_handle(&self, canvas: &mut Canvas) {
        let grip = self.pane_handle(self.current);
        canvas.fill_round_rect(grip, pane_handle_w() / 2, ui::RAISED);
        canvas.stroke_round_rect(grip, pane_handle_w() / 2, 1, ui::BORDER);
        canvas.fill_rect(
            Rect::new(grip.x + grip.w / 2 - 1, grip.y + 14, 2, grip.h - 28),
            ui::MUTED,
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
            ui::MUTED,
        );
    }

    fn draw_nav(&self, canvas: &mut Canvas, fonts: &Fonts, _pointer: (i32, i32)) {
        let rect = self.nav_rect();
        canvas.fill_rect(rect, ui::RAISED);
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
            ui::paint_subtree(canvas, fonts, &doc.tree, &self.nav_layout, Document::ROOT, &focus)
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
            // document's own window paints BACKGROUND across the bar, so that
            // is what the empty slot has to be; RAISED here left a grey patch
            // over the black.
            canvas.fill_rect(home, ui::BACKGROUND);

            let ghost = Rect::new(ghost_x, home.y, home.w, home.h);
            let active = current_id == Some(id);
            canvas.shadow(ghost, ui::radius_control(), ui::sc(8), 110);
            canvas.fill_round_rect(ghost, ui::radius_control(), if active { ui::ACCENT } else { ui::PRESSED });
            let style = Style { size: 11.0 * ui::scale(), ..Style::default() };
            let label = self.tab_name(workspace);
            canvas.clipped(ghost.inset(2), |canvas| {
                canvas.draw_text(
                    fonts,
                    &label,
                    ghost.x + ui::sc(12),
                    ghost.y + (ghost.h - fonts.line_height(&style)) / 2,
                    &style,
                    ui::TEXT,
                );
            });
            Self::draw_tab_close(canvas, ghost, active);
        }

        canvas.fill_rect(Rect::new(rect.x, rect.y + rect.h - 1, rect.w, 1), ui::BORDER);
    }

    /// The close glyph at a tab's right end.
    fn draw_tab_close(canvas: &mut Canvas, tab: Rect, active: bool) {
        let cx = (tab.x + tab.w - tab_close_w() / 2 - ui::sc(2)) as f32;
        let cy = (tab.y + tab.h / 2) as f32;
        let r = ui::sc(3) as f32;
        let ink = if active { ui::TEXT } else { ui::MUTED };
        let t = ui::sc(1).max(1);
        canvas.stroke_line(cx - r, cy - r, cx + r, cy + r, t, ink);
        canvas.stroke_line(cx - r, cy + r, cx + r, cy - r, t, ink);
    }

    /// A readout of what the compositor is holding, for screenshots.
    fn draw_debug(&self, canvas: &mut Canvas, fonts: &Fonts) {
        let style = Style { family: Family::Mono, size: 13.0 * ui::scale(), ..Style::default() };
        let height = 20 * (self.clients.len() as i32 + 4);
        let panel = Rect::new(8, self.nav_rect().h + 8, 620, height);
        canvas.fill_rect(panel, ui::SURFACE);
        canvas.stroke_rect(panel, 1, ui::ACCENT);

        let mut y = panel.y + 6;
        let workspace = self
            .workspaces
            .get(self.current)
            .map(|w| format!("workspace {} ({} windows)", w.id, w.windows.len()))
            .unwrap_or_else(|| "no workspace".into());
        canvas.draw_text(fonts, &workspace, panel.x + 8, y, &style, ui::ACCENT);
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
                ui::TEXT,
            );
            y += 20;
            if front == "*" {
                canvas.draw_text(fonts, &client.focus_summary(), panel.x + 20, y, &style, ui::MUTED);
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
fn draw_window(
    canvas: &mut Canvas,
    fonts: &Fonts,
    client: &Client,
    icon: Option<&tiny_skia::Pixmap>,
    window: &Window,
    focused: bool,
    pointer: (i32, i32),
) {
    let rect = window.rect;
    let maximized = window.maximized;
    let bar = Rect::new(rect.x, rect.y, rect.w, window_title_h());

    // Depth rather than a heavy outline. A focused window sits higher.
    canvas.shadow(rect, ui::radius_window(), ui::sc(if focused { 22 } else { 12 }), 130);
    canvas.fill_round_rect(rect, ui::radius_window(), ui::BACKGROUND);

    // The bar is the top of the same rounded shape, clipped to its own height so
    // the two lower corners stay square against the content below. Lit faintly
    // from above like every other raised surface.
    canvas.clipped(bar, |canvas| {
        let base = if focused { ui::RAISED } else { ui::SURFACE };
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
            let chip = if action == Title::Close { ui::DANGER } else { ui::PRESSED };
            canvas.fill_round_rect(button.inset(ui::sc(4)), ui::radius_small(), chip);
        }

        let ink = match (hovered, focused) {
            (true, _) => ui::TEXT,
            (false, true) => ui::MUTED,
            (false, false) => ui::BORDER,
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
    let ink = if focused { ui::TEXT } else { ui::MUTED };
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
    canvas.clipped(content, |canvas| client.draw(canvas, fonts));

    canvas.fill_rect(Rect::new(bar.x, bar.y + bar.h - 1, bar.w, 1), ui::BORDER);
    canvas.stroke_round_rect(rect, ui::radius_window(), 1, ui::BORDER);
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
    /// F1. A stand-in for the start menu, which does not exist.
    CycleWorkspace,
    /// F2. The diagnostic overlay.
    ToggleDebug,
    /// F3. Fold the conversation pane away, for when the window needs the room.
    TogglePane,
}
