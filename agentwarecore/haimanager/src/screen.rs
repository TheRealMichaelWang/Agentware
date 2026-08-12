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

use std::collections::HashMap;
use std::os::fd::{OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

use crate::client::{Client, Kind, Progress};
use crate::cursor;
use crate::document::Document;
use crate::input::{Button, Event, Key};
use crate::paint::font::{Family, Fonts, Style};
use crate::paint::{Canvas, Rect};
use crate::ui::{self, Focus, Frame, Layout, Regions};

/// Height of the navigation bar, which sits above every workspace.
pub const NAV_HEIGHT: i32 = 40;
const TASKBAR_HEIGHT: i32 = 56;
const PANE_WIDTH: i32 = 380;
/// Height of the title bar the compositor draws around an application window.
const WINDOW_TITLE: i32 = 28;
/// How far each successive window is offset, so none is entirely hidden.
const CASCADE: i32 = 34;
const WINDOW_MARGIN: i32 = 22;

/// Where the keyboard is pointed.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Surface {
    Nav,
    Desk,
    App(RawFd),
}

/// One application window: a connection and the cascade position it was given.
struct Window {
    fd: RawFd,
    /// Assigned when the window opens and never changed, so raising a window
    /// brings it forward without also moving it.
    slot: usize,
}

struct Workspace {
    id: u32,
    desk: Option<RawFd>,
    /// Front to back is last to first: the final entry is on top.
    windows: Vec<Window>,
    /// The agent running a turn here, if there is one. Its presence is what
    /// freezes the `apps` region.
    agent: Option<RawFd>,
    /// Slots handed out so far, so a closed window does not free its position
    /// and shuffle everything else under the human's pointer.
    next_slot: usize,
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

    /// Where a window sits, given its slot and how many have been handed out.
    ///
    /// A cascade rather than a tiling because overlap is the point: a window
    /// that is partly covered is the case that makes "is this node reachable"
    /// a real question rather than a formality, and that question is what an
    /// agent's intent is checked against.
    fn window_rect(&self, slot: usize, slots: usize) -> Rect {
        let apps = self.regions().apps;
        let spread = CASCADE * slots.saturating_sub(1) as i32;
        let offset = CASCADE * slot as i32;
        Rect::new(
            apps.x + WINDOW_MARGIN + offset,
            apps.y + WINDOW_MARGIN + offset,
            (apps.w - WINDOW_MARGIN * 2 - spread).max(200),
            (apps.h - WINDOW_MARGIN * 2 - spread).max(160),
        )
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
                let slot = self.workspaces[at].next_slot;
                self.workspaces[at].next_slot += 1;
                self.workspaces[at].windows.push(Window { fd, slot });
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
            next_slot: 0,
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
            let slots = workspace.next_slot;
            for window in &workspace.windows {
                let rect = self.window_rect(window.slot, slots);
                frames.push((
                    window.fd,
                    Frame::Whole(Rect::new(
                        rect.x,
                        rect.y + WINDOW_TITLE,
                        rect.w,
                        rect.h - WINDOW_TITLE,
                    )),
                ));
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
            // else. No client is told the pointer went past it.
            Event::PointerMoved { .. } => true,

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

        let workspace = self.workspaces.get(self.current)?;
        if self.regions().apps.contains(x, y) {
            let slots = workspace.next_slot;
            // Front to back, so the window on top wins.
            for window in workspace.windows.iter().rev() {
                if self.window_rect(window.slot, slots).contains(x, y) {
                    return Some(Surface::App(window.fd));
                }
            }
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
                if self.agent_running() {
                    // Not an error and not silent. The human is being told the
                    // workspace is being driven, not that their click was lost.
                    self.notes.push(format!(
                        "workspace {}: apps are frozen while an agent is running",
                        self.workspaces[self.current].id
                    ));
                    return true;
                }
                self.raise(fd, fonts);
                self.focus = Surface::App(fd);
                self.route_to(fd, fonts, Event::ButtonPressed { button: Button::Left, x, y })
            }

            Surface::Desk => {
                self.focus = Surface::Desk;
                self.route_desk(fonts, Event::ButtonPressed { button: Button::Left, x, y })
            }
        }
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

    fn raise(&mut self, fd: RawFd, fonts: &Fonts) {
        let Some(workspace) = self.workspaces.get_mut(self.current) else { return };
        let Some(at) = workspace.windows.iter().position(|w| w.fd == fd) else { return };
        if at + 1 == workspace.windows.len() {
            return;
        }
        let window = workspace.windows.remove(at);
        workspace.windows.push(window);
        // The frame does not change, since a window keeps its slot when raised,
        // but the client list order did and painting follows it.
        let _ = fonts;
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
        let slots = workspace.next_slot;
        for window in &workspace.windows {
            let focused = self.focus == Surface::App(window.fd);
            let rect = self.window_rect(window.slot, slots);
            let Some(client) = self.client(window.fd) else { continue };
            canvas.clipped(regions.apps, |canvas| {
                draw_window(canvas, fonts, client, rect, focused)
            });
        }

        for region in ["pane", "taskbar"] {
            let rect = if region == "pane" { regions.pane } else { regions.taskbar };
            canvas.fill_rect(rect, ui::SURFACE);
            canvas.stroke_rect(rect, 1, ui::BORDER);
            if let Some(desk) = desk {
                canvas.clipped(rect, |canvas| desk.draw_region(canvas, fonts, region));
            }
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

/// The chrome around an application window.
///
/// Drawn by the compositor, from the `title` the application declared. An app
/// names its window; it does not draw one, and it cannot draw outside the one it
/// was given.
fn draw_window(canvas: &mut Canvas, fonts: &Fonts, client: &Client, rect: Rect, focused: bool) {
    let bar = Rect::new(rect.x, rect.y, rect.w, WINDOW_TITLE);
    let edge = if focused { ui::ACCENT } else { ui::BORDER };

    canvas.fill_rect(rect, ui::BACKGROUND);
    canvas.fill_rect(bar, if focused { ui::RAISED } else { ui::SURFACE });

    let style = Style { size: 13.0, ..Style::default() };
    let ink = if focused { ui::TEXT } else { ui::MUTED };
    canvas.draw_text(fonts, client.title(), bar.x + 10, bar.y + 6, &style, ink);

    let content = Rect::new(rect.x, rect.y + WINDOW_TITLE, rect.w, rect.h - WINDOW_TITLE);
    canvas.clipped(content, |canvas| client.draw(canvas, fonts));

    canvas.stroke_rect(rect, 1, edge);
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
