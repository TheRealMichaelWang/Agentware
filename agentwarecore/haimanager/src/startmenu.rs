//! The start menu: the human's door to a new agentdesk and to every
//! installed application.
//!
//! The Agentware mark at the left end of the taskbar opens a panel centred over
//! the workspace. On top, a prompt: Enter makes it a new agentdesk, empty makes
//! one with nothing to do yet. Below, a grid of every installed application,
//! three across, icon over name; pressing one opens it into the workspace on
//! screen. A click anywhere else, or Escape, closes the panel.
//!
//! It is compositor chrome for the reasons the dock and the navigation bar are:
//! it needs the icons only the compositor holds, it must vanish on a click
//! anywhere else, which only the compositor sees, and it must work when the
//! workspace under it does not. It is still AWML, built here and run through
//! the same parser, layout and painter as everything else, so it cannot drift
//! from the style of what it floats over. What it asks for goes to PID 1 the
//! way the stop button and a tab's close do, `create-desk` and `open-app`;
//! this module decides, and [`crate::screen`] carries the request, since the
//! menu never touches the control connection.
//!
//! An agent cannot see or reach any of it: chrome documents are not clients.
//! An agent opens applications only through its agentdesk.

use std::collections::HashMap;

use awproto::display::escape;

use crate::document::Document;
use crate::images::Images;
use crate::input::Key;
use crate::paint::font::Fonts;
use crate::paint::{Canvas, Rect};
use crate::ui::{self, Focus, Frame, Layout};

/// The name the Agentware mark is installed under in the icon cache. Not an
/// application, so no package; the SVG is compiled in.
pub const AGENTWARE_ICON: &str = "agentware";
pub const AGENTWARE_SVG: &[u8] = include_bytes!("../assets/agentware.svg");

/// Where installed applications live, one folder per package.
const APP_DIR: &str = "/apps";

/// Tiles per row in the application grid.
const COLUMNS: usize = 3;

/// The icon on a tile, prepared when the menu opens so painting is a lookup.
fn tile_icon() -> i32 { ui::sc(28) }
/// The room between the panel's edge and its content.
fn inset() -> i32 { ui::sc(20) }
/// The prompt box: tall, because it is the point of the panel.
fn prompt_h() -> i32 { ui::sc(44) }
/// The panel's width, and the most height it takes before the grid scrolls.
/// Smaller screens get what fits.
fn panel_w() -> i32 { ui::sc(640) }
fn panel_max_h() -> i32 { ui::sc(560) }

/// What a press or a keystroke on the menu came to.
pub enum StartOutcome {
    Nothing,
    /// The document changed and wants rebuilding.
    Changed,
    Close,
    /// Make a workspace, with this prompt or with none.
    CreateDesk(Option<String>),
    /// Open this application into the workspace on screen.
    Open(String),
    /// Shut the machine down. Real power-off, not a screen that pretends:
    /// PID 1 runs the same orderly shutdown the machine's power button gets.
    PowerOff,
    /// Shut everything down and boot fresh.
    Restart,
}

/// An installed application, as the menu lists it.
struct Installed {
    name: String,
    label: String,
    description: String,
}

/// The menu while it is open: the prompt being typed, and the document it is
/// drawn from, rebuilt whenever the prompt or the scroll changes.
pub struct StartMenu {
    prompt: String,
    doc: Option<Document>,
    layout: Layout,
    scroll: HashMap<String, i32>,
    /// Where the panel is: centred in the area it was built for, as tall as
    /// its content up to a limit, so two applications do not get a panel
    /// sized for twenty.
    panel: Rect,
    /// Read from the package directory when the menu opened, and again each
    /// time it opens, so an app installed while the machine runs appears the
    /// next time the menu does.
    apps: Vec<Installed>,
}

impl StartMenu {
    pub fn open() -> StartMenu {
        StartMenu {
            prompt: String::new(),
            doc: None,
            layout: Layout::empty(),
            scroll: HashMap::new(),
            panel: Rect::new(0, 0, 0, 0),
            apps: installed_apps(),
        }
    }

    /// The panel's rectangle, once built.
    pub fn panel(&self) -> Rect {
        self.panel
    }

    /// Lay the document out, centred in `area`, preparing the icons it shows.
    ///
    /// The panel is as tall as its content wants, within a ceiling and within
    /// the area; past the ceiling the grid scrolls. Measured before it is
    /// placed, which is what the measure pass exists for.
    pub fn build(&mut self, fonts: &Fonts, images: &mut Images, area: Rect) {
        for app in &self.apps {
            images.icons.prepare(&app.name, &[tile_icon()]);
        }
        let Ok(doc) = Document::parse(&self.markup(), 0) else { return };

        let w = panel_w().min(area.w - ui::sc(40));
        let wanted = ui::natural_height(fonts, &doc.tree, w - inset() * 2) + inset() * 2;
        let h = wanted.min(panel_max_h()).min(area.h - ui::sc(40));
        self.panel = Rect::new(area.x + (area.w - w) / 2, area.y + (area.h - h) / 2, w, h);

        let frame = self.panel.inset(inset());
        self.layout = ui::layout_chrome(fonts, &doc, &Frame::Whole(frame), &mut self.scroll);
        self.doc = Some(doc);
    }

    /// The panel's document: a prompt on top, the grid below.
    fn markup(&self) -> String {
        let mut out = String::from(
            "<window font=\"sans\" pad=\"none\">\n  <vstack gap=\"md\">\n\
             \x20   <text role=\"heading\">New agentdesk</text>\n\
             \x20   <text color=\"muted\">Say what it should do. Enter starts it; \
             an empty prompt makes one with nothing to do yet.</text>\n",
        );
        out.push_str(&format!(
            "    <field id=\"start-prompt\" size=\"16\" height=\"{height}\" value=\"{value}\" \
             placeholder=\"What should the new agentdesk do?\" \
             description=\"The prompt a new agentdesk begins with\"/>\n",
            height = prompt_h(),
            value = escape(&self.prompt),
        ));
        out.push_str(
            "    <hstack gap=\"sm\">\n      <text grow=\"true\"/>\n\
             \x20     <button id=\"start-go\" label=\"Start agentdesk\" emphasis=\"primary\" \
             description=\"Creates a new agentdesk that begins with the prompt above\"/>\n\
             \x20   </hstack>\n\
             \x20   <divider/>\n\
             \x20   <text role=\"subheading\">Applications</text>\n\
             \x20   <scroll grow=\"true\">\n      <vstack gap=\"sm\">\n",
        );
        for row in self.apps.chunks(COLUMNS) {
            out.push_str("        <hstack gap=\"sm\">\n");
            for app in row {
                out.push_str(&format!(
                    "          <button id=\"launch-{name}\" tile=\"true\" grow=\"true\" icon=\"{name}\" \
                     label=\"{label}\" description=\"Opens {label} in this agentdesk: {description}\"/>\n",
                    name = escape(&app.name),
                    label = escape(&app.label),
                    description = escape(&app.description),
                ));
            }
            // Empty cells keep a short last row from stretching its tiles to
            // three times the width of the ones above.
            for _ in row.len()..COLUMNS {
                out.push_str("          <text grow=\"true\"/>\n");
            }
            out.push_str("        </hstack>\n");
        }
        if self.apps.is_empty() {
            out.push_str("        <text color=\"muted\">No applications are installed.</text>\n");
        }
        out.push_str("      </vstack>\n    </scroll>\n");

        // The machine's power, in the panel's lower-left corner. Here rather
        // than in a workspace because turning the machine off belongs to no
        // workspace, and the start menu is the one piece of chrome that is
        // the machine's rather than a desk's. No sleep button: the kernel
        // could suspend, but nothing could wake it, and a sleep that cannot
        // be woken from the keyboard is a power-off wearing the wrong label.
        out.push_str(
            "    <divider/>\n    <hstack gap=\"sm\">\n\
             \x20     <button id=\"power-off\" label=\"Power off\" emphasis=\"danger\" \
             description=\"Shuts the machine down. Every agentdesk, its apps and its conversation end; settings survive.\"/>\n\
             \x20     <button id=\"restart\" label=\"Restart\" \
             description=\"Shuts everything down and starts the machine again from scratch\"/>\n\
             \x20     <text grow=\"true\"/>\n    </hstack>\n",
        );
        out.push_str("  </vstack>\n</window>\n");
        out
    }

    /// Whether a point is over the prompt, for the pointer's shape.
    pub fn over_prompt(&self, x: i32, y: i32) -> bool {
        self.doc
            .as_ref()
            .and_then(|doc| doc.index_of("#start-prompt"))
            .is_some_and(|index| self.layout.rect_of(index).contains(x, y))
    }

    /// A press inside the panel.
    pub fn click(&mut self, x: i32, y: i32) -> StartOutcome {
        let Some(doc) = &self.doc else { return StartOutcome::Nothing };
        let Some(index) = self.layout.hit(&doc.tree, x, y) else { return StartOutcome::Nothing };
        let Some(id) = doc.tree.node(index).id() else { return StartOutcome::Nothing };

        if id == "start-go" {
            return self.submit();
        }
        if id == "power-off" {
            return StartOutcome::PowerOff;
        }
        if id == "restart" {
            return StartOutcome::Restart;
        }
        if let Some(app) = id.strip_prefix("launch-") {
            return StartOutcome::Open(app.to_owned());
        }
        // The prompt field: focus is already here and the caret sits at the
        // end, so a press on it changes nothing.
        StartOutcome::Nothing
    }

    /// The prompt becomes a workspace: with the text if there is any, empty
    /// if there is not.
    fn submit(&mut self) -> StartOutcome {
        let prompt = self.prompt.trim().to_owned();
        StartOutcome::CreateDesk(if prompt.is_empty() { None } else { Some(prompt) })
    }

    pub fn key(&mut self, key: Key) -> StartOutcome {
        match key {
            Key::Char(c) => self.prompt.push(c),
            Key::Backspace => {
                self.prompt.pop();
            }
            Key::Enter => return self.submit(),
            Key::Escape => return StartOutcome::Close,
            _ => return StartOutcome::Nothing,
        }
        StartOutcome::Changed
    }

    /// The wheel over the application grid.
    pub fn wheel(&mut self, delta: i32, x: i32, y: i32) -> StartOutcome {
        let Some(doc) = &self.doc else { return StartOutcome::Nothing };
        let Some(scroller) = self.layout.scroller_at(x, y) else { return StartOutcome::Nothing };
        let furthest = (scroller.content - scroller.viewport).max(0);
        let next = (scroller.offset - delta * ui::wheel_step()).clamp(0, furthest);
        if next == scroller.offset {
            return StartOutcome::Nothing;
        }
        self.scroll.insert(doc.key(scroller.node).to_owned(), next);
        StartOutcome::Changed
    }

    /// The panel and its document. `caret_lit` is the compositor's blink
    /// phase; the caret is in the prompt while the menu is open.
    pub fn draw(&self, canvas: &mut Canvas, fonts: &Fonts, images: &Images, panel: Rect, caret_lit: bool) {
        canvas.shadow(panel, ui::radius_surface(), ui::sc(28), 160);
        canvas.fill_round_rect_vgrad(panel, ui::radius_surface(), ui::lift(ui::surface(), 6), ui::surface());
        canvas.stroke_round_rect(panel, ui::radius_surface(), 1, ui::border());

        let Some(doc) = &self.doc else { return };
        let focus = Focus {
            node: doc.index_of("#start-prompt"),
            caret: self.prompt.chars().count(),
            caret_visible: caret_lit,
            ..Focus::default()
        };
        // Painted from the root's child rather than the root: the window node
        // would fill its whole rectangle square, over the panel's rounded
        // corners.
        let Some(&body) = doc.tree.node(Document::ROOT).children.first() else { return };
        canvas.clipped(panel, |canvas| {
            ui::paint_subtree(canvas, fonts, images, &doc.tree, &self.layout, body, &focus)
        });
    }
}

/// Every application installed, read from the package directory: the folders
/// with an `exec` and a description, since a package without one is a
/// stand-in or a leftover rather than something to offer. `name.txt` is the
/// label people see; the folder name stands in for it.
fn installed_apps() -> Vec<Installed> {
    let Ok(entries) = std::fs::read_dir(APP_DIR) else { return Vec::new() };
    let mut apps: Vec<Installed> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let dir = entry.path();
            if !dir.join("exec").is_file() {
                return None;
            }
            let read = |file: &str| {
                std::fs::read_to_string(dir.join(file))
                    .map(|text| text.trim().to_owned())
                    .unwrap_or_default()
            };
            let description = read("description.txt");
            if description.is_empty() {
                return None;
            }
            let label = read("name.txt");
            Some(Installed {
                label: if label.is_empty() { name.clone() } else { label },
                description,
                name,
            })
        })
        .collect();
    apps.sort_by(|a, b| a.label.cmp(&b.label));
    apps
}

