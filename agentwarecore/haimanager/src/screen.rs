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
use crate::input::{Button, Event, Key};
use crate::paint::font::{Family, Fonts, Style};
use crate::paint::{Canvas, Rect, rgb};
use crate::ui::{self, Focus, Frame, Layout, Regions};

/// Height of the navigation bar, which sits above every workspace.
pub const NAV_HEIGHT: i32 = 40;
const TASKBAR_HEIGHT: i32 = 56;
const PANE_WIDTH: i32 = 380;
/// Height of the title bar the compositor draws around an application window.
const WINDOW_TITLE: i32 = 34;
/// How far each successive window is offset, so none opens exactly on another.
const CASCADE: i32 = 32;
const WINDOW_MARGIN: i32 = 26;
/// Radius of the close, minimize and maximize dots.
const LIGHT: i32 = 6;
/// Centre of the first dot, from the left edge of the title bar.
const LIGHT_INSET: i32 = 17;
/// Distance between dot centres.
const LIGHT_STEP: i32 = 20;
/// The strip along the bottom of the apps region holding minimized windows.
const DOCK_HEIGHT: i32 = 42;

/// What a point in a title bar means.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Title {
    Close,
    Minimize,
    Maximize,
    /// Anywhere else on the bar: pick the window up.
    Drag,
}

/// A window being moved by the human.
struct Drag {
    fd: RawFd,
    /// Where in the window the pointer took hold, so it does not jump.
    grab: (i32, i32),
}

/// How long the fake cursor takes to travel to what an agent named.
///
/// Long enough for a human to follow, which is the entire point of it. An agent
/// that acted instantly would be indistinguishable from one that had never
/// shown its work, and the visible embodiment VISION.md promises would be a
/// claim rather than something on screen.
const FLIGHT: Duration = Duration::from_millis(600);

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
    /// Requests for the supervisor. Collected here rather than sent from here
    /// because the control connection is owned by the event loop, and a screen
    /// that could write to PID 1 from inside a click handler is a screen that
    /// can block inside one.
    requests: Vec<Vec<String>>,
    notes: Vec<String>,
    drag: Option<Drag>,

    /// The intent being performed, if any.
    flight: Option<Flight>,
    /// Intents that arrived while one was in flight. An agent waits for its
    /// answer before sending the next, so this is a safety net rather than a
    /// pipeline.
    queued: VecDeque<(RawFd, Vec<String>)>,
    /// Where the agent's pointer is, and whose workspace it is in. Kept after a
    /// flight lands, so the human can see what was just touched.
    agent_cursor: Option<(u32, i32, i32)>,
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
            requests: Vec::new(),
            notes: Vec::new(),
            drag: None,
            flight: None,
            queued: VecDeque::new(),
            agent_cursor: None,
            animated: false,
        }
    }

    // ---- geometry ----------------------------------------------------------

    fn nav_rect(&self) -> Rect {
        Rect::new(self.bounds.x, self.bounds.y, self.bounds.w, NAV_HEIGHT)
    }

    fn workspace_area(&self) -> Rect {
        Rect::new(
            self.bounds.x,
            self.bounds.y + NAV_HEIGHT,
            self.bounds.w,
            self.bounds.h - NAV_HEIGHT,
        )
    }

    fn regions(&self) -> Regions {
        Regions::carve(self.workspace_area(), TASKBAR_HEIGHT, PANE_WIDTH)
    }

    /// Where windows may go: the apps region, less the dock if it is showing.
    fn window_area(&self, at: usize) -> Rect {
        let apps = self.regions().apps;
        if self.docked(at).is_empty() {
            return apps;
        }
        Rect::new(apps.x, apps.y, apps.w, apps.h - DOCK_HEIGHT)
    }

    /// Where a window opens, before the human has an opinion about it.
    ///
    /// A cascade rather than a tiling because overlap is the point: a window
    /// that is partly covered is the case that makes "is this node reachable"
    /// a real question rather than a formality, and that question is what an
    /// agent's intent is checked against.
    fn opening_rect(&self, at: usize, opened: usize) -> Rect {
        let area = self.window_area(at);
        // Wraps after a few, so the tenth window is not off the bottom corner.
        let step = CASCADE * (opened % 5) as i32;
        Rect::new(
            area.x + WINDOW_MARGIN + step,
            area.y + WINDOW_MARGIN + step,
            (area.w - WINDOW_MARGIN * 2 - CASCADE).max(320),
            (area.h - WINDOW_MARGIN * 2 - CASCADE).max(220),
        )
    }

    /// The strip of minimized windows along the bottom of the apps region.
    ///
    /// Drawn by the compositor rather than put in the agentdesk's taskbar,
    /// because whether a window is minimized is compositor state and the
    /// agentdesk is never told that windows exist at all.
    fn dock_rect(&self) -> Rect {
        let apps = self.regions().apps;
        Rect::new(
            apps.x,
            apps.y + apps.h - DOCK_HEIGHT,
            apps.w,
            DOCK_HEIGHT,
        )
    }

    fn docked(&self, at: usize) -> Vec<RawFd> {
        self.workspaces
            .get(at)
            .map(|workspace| {
                workspace
                    .windows
                    .iter()
                    .filter(|window| window.minimized)
                    .map(|window| window.fd)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Where each minimized window's pill sits.
    fn dock_pills(&self, at: usize) -> Vec<(RawFd, Rect)> {
        let dock = self.dock_rect();
        let mut x = dock.x + 14;
        self.docked(at)
            .into_iter()
            .map(|fd| {
                let width = 150;
                let pill = Rect::new(x, dock.y + 7, width, DOCK_HEIGHT - 14);
                x += width + 8;
                (fd, pill)
            })
            .collect()
    }

    /// What part of a window's title bar a point is on.
    fn title_hit(rect: Rect, x: i32, y: i32) -> Option<Title> {
        let bar = Rect::new(rect.x, rect.y, rect.w, WINDOW_TITLE);
        if !bar.contains(x, y) {
            return None;
        }

        let cy = bar.y + bar.h / 2;
        for (index, action) in
            [Title::Close, Title::Minimize, Title::Maximize].into_iter().enumerate()
        {
            let cx = bar.x + LIGHT_INSET + LIGHT_STEP * index as i32;
            let (dx, dy) = (x - cx, y - cy);
            // A little larger than the dot is drawn. A six pixel target is not
            // a target.
            if dx * dx + dy * dy <= (LIGHT + 4) * (LIGHT + 4) {
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
        });
        self.workspaces.len() - 1
    }

    /// Hand every client the part of the screen it currently occupies.
    fn reframe(&mut self, fonts: &Fonts) {
        let regions = self.regions();
        let mut frames: Vec<(RawFd, Frame)> = Vec::new();

        for workspace in &self.workspaces {
            if let Some(fd) = workspace.desk {
                frames.push((fd, Frame::Regions(regions)));
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
            // else, unless a window is being carried.
            Event::PointerMoved { x, y } => {
                self.drag_to(fonts, x, y);
                true
            }

            Event::ButtonReleased { button: Button::Left, .. } => {
                self.drag = None;
                false
            }

            Event::ButtonPressed { button: Button::Left, x, y } => self.click(fonts, x, y),

            Event::Scrolled { delta, x, y } => match self.surface_at(x, y) {
                Some(Surface::App(fd)) => self.route_to(fd, fonts, event),
                Some(Surface::Desk) => self.route_desk(fonts, event),
                _ => {
                    let _ = delta;
                    false
                }
            },

            Event::KeyPressed(_) | Event::KeyReleased(_) => match self.focus {
                Surface::App(fd) => self.route_to(fd, fonts, event),
                Surface::Desk => self.route_desk(fonts, event),
                Surface::Nav => false,
            },

            _ => false,
        }
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
                self.route_to(fd, fonts, Event::ButtonPressed { button: Button::Left, x, y })
            }

            Surface::Desk => {
                // A minimized window's pill sits over the wallpaper, so it is
                // checked before the click is handed to the workspace.
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
                self.route_desk(fonts, Event::ButtonPressed { button: Button::Left, x, y })
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
                self.drag = Some(Drag { fd, grab: (x - rect.x, y - rect.y) });
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
                    window.rect = area.inset(8);
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

    /// Carry a window with the pointer.
    ///
    /// Clamped so the title bar can always be reached again. A window dragged
    /// entirely off the bottom of its region is a window the human has lost.
    fn drag_to(&mut self, fonts: &Fonts, x: i32, y: i32) {
        let Some(drag) = &self.drag else { return };
        let (fd, grab) = (drag.fd, drag.grab);
        let at = self.current;
        let area = self.window_area(at);

        let Some(window) = self
            .workspaces
            .get_mut(at)
            .and_then(|w| w.windows.iter_mut().find(|window| window.fd == fd))
        else {
            self.drag = None;
            return;
        };

        window.rect.x = (x - grab.0).clamp(area.x - window.rect.w + 120, area.x + area.w - 120);
        window.rect.y = (y - grab.1).clamp(area.y, area.y + area.h - WINDOW_TITLE);
        // Dragging a maximized window makes it a normal one again, which is what
        // grabbing hold of something ought to mean.
        window.maximized = false;
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

        let Some(client) = self.client(app_fd) else {
            self.refuse(from, &app, &target, agent::REASON_NO_SUCH_APP);
            return false;
        };
        let Some(index) = client.node_by_id(&target) else {
            self.refuse(from, &app, &target, agent::REASON_NO_SUCH_NODE);
            return false;
        };

        // `scroll-into-view` is the one action the compositor performs itself,
        // and the one that targets any node rather than only a control. It is
        // also the way out of both ways a node can be unreachable: it scrolls
        // the container, and it brings a covered window forward. The agent says
        // what it wants to be true and not how to bring it about.
        if action == "scroll-into-view" {
            self.raise(at, app_fd);
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

        // Behind another window. The test is literally "would a human clicking
        // here have hit this", which is the standard every intent is held to.
        if self.topmost_at(at, to.0, to.1) != Some(app_fd) {
            self.refuse(from, &app, &target, agent::REASON_NOT_VISIBLE);
            return false;
        }

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
        self.flight.is_some() || self.clients.iter().any(Client::animating)
    }

    /// Advance whatever is moving.
    pub fn tick(&mut self, fonts: &Fonts) -> bool {
        let busy = self.wants_frame();
        // One frame after the last animation ends, so a pressed control is
        // repainted unpressed rather than staying that way until the next time
        // something happens to redraw the screen.
        let settling = self.animated && !busy;
        self.animated = busy;

        let Some(flight) = &self.flight else { return busy || settling };

        match flight.stage {
            Stage::Travelling => {
                let elapsed = flight.started.elapsed();
                if elapsed >= FLIGHT {
                    self.land(fonts);
                    return true;
                }

                // Eased, because a pointer that moves at a constant speed and
                // stops dead does not read as a pointer.
                let t = elapsed.as_secs_f32() / FLIGHT.as_secs_f32();
                let eased = 1.0 - (1.0 - t).powi(3);
                let x = flight.from.0 + ((flight.to.0 - flight.from.0) as f32 * eased) as i32;
                let y = flight.from.1 + ((flight.to.1 - flight.from.1) as f32 * eased) as i32;
                self.agent_cursor = Some((flight.desk, x, y));
                true
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
        // application sees a value growing rather than one appearing.
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
            "<window font=\"sans\">\n  <hstack gap=\"sm\">\n\
             \x20   <button id=\"nav-home\" label=\"Home\" \
             description=\"Opens the start menu, where a new workspace is created\"/>\n",
        );

        for (at, workspace) in self.workspaces.iter().enumerate() {
            let busy = if workspace.agent.is_some() { " *" } else { "" };
            out.push_str(&format!(
                "    <button id=\"nav-desk-{id}\" label=\"Workspace {id}{busy}\"{emphasis} \
                 description=\"Switches to workspace {id}\"/>\n",
                id = workspace.id,
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
        self.nav_layout = ui::layout(
            fonts,
            &doc,
            &Frame::Whole(self.nav_rect()),
            &mut self.nav_scroll,
        );
        self.nav = Some(doc);
    }

    fn click_nav(&mut self, fonts: &Fonts, x: i32, y: i32) -> bool {
        if self.nav.is_none() {
            self.build_nav(fonts);
        }
        let Some(doc) = &self.nav else { return false };
        let Some(index) = self.nav_layout.hit(&doc.tree, x, y) else { return true };
        let Some(id) = doc.tree.node(index).id().map(str::to_owned) else { return true };

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
            && let Some(at) = self.workspaces.iter().position(|w| w.id == wanted)
        {
            return self.switch(at) || true;
        }

        true
    }

    // ---- painting ----------------------------------------------------------

    pub fn draw(&mut self, canvas: &mut Canvas, fonts: &Fonts, pointer: (i32, i32)) {
        if self.nav.is_none() {
            self.build_nav(fonts);
        }

        canvas.clear(ui::BACKGROUND);

        match self.workspaces.get(self.current) {
            Some(_) => self.draw_workspace(canvas, fonts),
            None => self.draw_empty(canvas, fonts),
        }

        self.draw_nav(canvas, fonts);

        if self.debug {
            self.draw_debug(canvas, fonts);
        }

        // The agent's pointer is drawn only in the workspace it is working in.
        // Showing it elsewhere would say an agent was acting on a screen it is
        // not touching.
        if let Some((desk, x, y)) = self.agent_cursor
            && self.workspaces.get(self.current).is_some_and(|w| w.id == desk)
        {
            cursor::draw(canvas, x, y, cursor::Kind::Agent);
        }

        let (x, y) = pointer;
        cursor::draw(canvas, x, y, cursor::Kind::Human);
    }

    /// Wallpaper, then windows, then the pane and the taskbar over them.
    ///
    /// The order is the reason painting has to be able to start partway down a
    /// tree: the workspace's regions are branches of one document that do not
    /// paint consecutively, because the application windows go between them.
    fn draw_workspace(&self, canvas: &mut Canvas, fonts: &Fonts) {
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
            canvas.clipped(regions.apps, |canvas| {
                draw_window(canvas, fonts, client, window.rect, focused)
            });
        }

        self.draw_dock(canvas, fonts);

        for region in ["pane", "taskbar"] {
            let rect = if region == "pane" { regions.pane } else { regions.taskbar };
            canvas.fill_rect(rect, ui::SURFACE);
            canvas.stroke_rect(rect, 1, ui::BORDER);
            if let Some(desk) = desk {
                canvas.clipped(rect, |canvas| desk.draw_region(canvas, fonts, region));
            }
        }
    }

    /// The minimized windows, as pills along the bottom of the apps region.
    fn draw_dock(&self, canvas: &mut Canvas, fonts: &Fonts) {
        let pills = self.dock_pills(self.current);
        if pills.is_empty() {
            return;
        }

        let dock = self.dock_rect();
        canvas.fill_round_rect(
            Rect::new(dock.x + 8, dock.y, dock.w - 16, dock.h - 6),
            ui::RADIUS_SURFACE,
            ui::SURFACE,
        );

        let style = Style { size: 13.0, ..Style::default() };
        for (fd, pill) in pills {
            let Some(client) = self.client(fd) else { continue };
            canvas.fill_round_rect(pill, ui::RADIUS_CONTROL, ui::RAISED);
            canvas.stroke_round_rect(pill, ui::RADIUS_CONTROL, 1, ui::BORDER);
            canvas.clipped(pill.inset(2), |canvas| {
                canvas.draw_text(fonts, client.title(), pill.x + 12, pill.y + 8, &style, ui::MUTED);
            });
        }
    }

    fn draw_empty(&self, canvas: &mut Canvas, fonts: &Fonts) {
        let style = Style { family: Family::Mono, size: 18.0, ..Style::default() };
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

    fn draw_nav(&self, canvas: &mut Canvas, fonts: &Fonts) {
        let rect = self.nav_rect();
        canvas.fill_rect(rect, ui::RAISED);
        let Some(doc) = &self.nav else { return };
        canvas.clipped(rect, |canvas| {
            ui::paint_subtree(canvas, fonts, &doc.tree, &self.nav_layout, Document::ROOT, &Focus::default())
        });
        canvas.fill_rect(Rect::new(rect.x, rect.y + rect.h - 1, rect.w, 1), ui::BORDER);
    }

    /// A readout of what the compositor is holding, for screenshots.
    fn draw_debug(&self, canvas: &mut Canvas, fonts: &Fonts) {
        let style = Style { family: Family::Mono, size: 13.0, ..Style::default() };
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
    Rect::new(rect.x, rect.y + WINDOW_TITLE, rect.w, rect.h - WINDOW_TITLE)
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
fn draw_window(canvas: &mut Canvas, fonts: &Fonts, client: &Client, rect: Rect, focused: bool) {
    let bar = Rect::new(rect.x, rect.y, rect.w, WINDOW_TITLE);

    // Depth rather than a heavy outline. A focused window sits higher.
    canvas.shadow(rect, ui::RADIUS_WINDOW, if focused { 22 } else { 12 }, 130);
    canvas.fill_round_rect(rect, ui::RADIUS_WINDOW, ui::BACKGROUND);

    // The bar is the top of the same rounded shape, clipped to its own height so
    // the two lower corners stay square against the content below.
    canvas.clipped(bar, |canvas| {
        canvas.fill_round_rect(
            Rect::new(bar.x, bar.y, bar.w, bar.h + ui::RADIUS_WINDOW),
            ui::RADIUS_WINDOW,
            if focused { ui::RAISED } else { ui::SURFACE },
        );
    });

    let cy = bar.y + bar.h / 2;
    for (index, colour) in [rgb(0xff, 0x5f, 0x57), rgb(0xfe, 0xbc, 0x2e), rgb(0x28, 0xc8, 0x40)]
        .into_iter()
        .enumerate()
    {
        let cx = bar.x + LIGHT_INSET + LIGHT_STEP * index as i32;
        let dot = Rect::new(cx - LIGHT, cy - LIGHT, LIGHT * 2, LIGHT * 2);
        // Unfocused windows keep the dots but drain them, the way every desktop
        // does, so the focused window is obvious without a coloured border.
        canvas.fill_round_rect(dot, LIGHT, if focused { colour } else { ui::BORDER });
    }

    let style = Style { size: 13.0, ..Style::default() };
    let ink = if focused { ui::TEXT } else { ui::MUTED };
    let title = client.title();
    let x = bar.x + (bar.w - fonts.measure(title, &style)) / 2;
    let x = x.max(bar.x + LIGHT_INSET + LIGHT_STEP * 3);
    canvas.draw_text(fonts, title, x, cy - fonts.line_height(&style) / 2, &style, ink);

    let content = content_of(rect);
    canvas.clipped(content, |canvas| client.draw(canvas, fonts));

    canvas.fill_rect(Rect::new(bar.x, bar.y + bar.h - 1, bar.w, 1), ui::BORDER);
    canvas.stroke_round_rect(rect, ui::RADIUS_WINDOW, 1, ui::BORDER);
}

/// Keys the compositor keeps for itself, before anything is routed.
pub fn compositor_key(key: Key) -> Option<CompositorKey> {
    match key {
        Key::Other(59) => Some(CompositorKey::CycleWorkspace),
        Key::Other(60) => Some(CompositorKey::ToggleDebug),
        _ => None,
    }
}

pub enum CompositorKey {
    /// F1. A stand-in for the start menu, which does not exist.
    CycleWorkspace,
    /// F2. The diagnostic overlay.
    ToggleDebug,
}
