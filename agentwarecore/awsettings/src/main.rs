//! Settings: the application that changes what every workspace looks like.
//!
//! A rail of categories down the right-hand side and the chosen category's
//! page beside it. One category so far, Desktop, and one setting on it: the
//! wallpaper, a dropdown of the pictures that ship with the system plus
//! "Choose an image...", which opens the shared file dialog on any SVG or PNG
//! on the machine. Choosing writes `settings.xml` on the state volume; every
//! agentdesk stats it on its clock tick and re-renders its background when it
//! has changed. No process is told, nothing is broadcast, and the choice is
//! there again after a reboot, because the volume is the machine's disk.
//!
//! Written the way every application is written: a model, a `render`, whole
//! tree every time, hand-written stable ids, no diffing and no ephemeral state.
//! The dropdown's `open` is the application's, like every other piece of
//! state in its tree: the compositor asks to open and close it and the
//! application answers by re-rendering. What is different is only what it
//! links, and that is awproto and awkit: this is a first-party application
//! with no more access than a calculator.

use std::fmt::Write as _;
use std::io::Write as IoWrite;
use std::path::Path;

use std::time::SystemTime;

use awkit::{Answer, FileDialog};
use awproto::display::{self, Event, Surface, escape};
use awproto::settings::{self, Settings as Stored};

/// The categories down the rail. One so far.
const CATEGORIES: &[(&str, &str)] = &[("desktop", "Desktop")];

/// The dropdown's option for opening the file dialog.
const CHOOSE: &str = "choose";

struct Settings {
    category: &'static str,
    /// The wallpapers that ship with the system, as (name, path).
    defaults: Vec<(String, String)>,
    /// The current choice, `None` for a plain background.
    current: Option<String>,
    /// When the settings file was last read, so a change made elsewhere is
    /// picked up before the next render rather than overwritten.
    seen: Option<SystemTime>,
    /// Whether the wallpaper dropdown is showing its options.
    open: bool,
    /// The file dialog, while one is up.
    choosing: Option<FileDialog>,
    status: String,
}

fn main() {
    let mut surface = match Surface::inherited() {
        Ok(surface) => surface,
        Err(err) => {
            log(&format!("no interface connection: {err}"));
            std::process::exit(1);
        }
    };

    let mut app = Settings {
        category: CATEGORIES[0].0,
        defaults: settings::wallpapers(),
        current: Stored::load().wallpaper,
        seen: settings::modified(),
        open: false,
        choosing: None,
        status: String::new(),
    };

    if let Err(err) = surface.render(&app.render()) {
        log(&format!("could not send the first tree: {err}"));
        std::process::exit(1);
    }

    loop {
        let event = match surface.next_event() {
            Ok(Some(event)) => event,
            Ok(None) => return,
            Err(err) => {
                log(&format!("connection failed: {err}"));
                std::process::exit(1);
            }
        };

        if !app.accept(&surface, &event) {
            continue;
        }

        if let Err(err) = surface.render(&app.render()) {
            log(&format!("could not send a tree: {err}"));
            return;
        }
    }
}

impl Settings {
    fn accept(&mut self, surface: &Surface, event: &Event) -> bool {
        // A click on a stale tree is a click on a control that may no longer
        // mean what it meant. Discarded, not guessed at.
        if surface.is_stale(event) && event.action == display::ACTION_CLICK {
            return false;
        }

        // The file is the truth. If something else wrote it since it was last
        // read, take that before acting, so this window never shows or saves
        // a choice the machine has already moved past.
        let seen = settings::modified();
        if seen != self.seen {
            self.seen = seen;
            self.current = Stored::load().wallpaper;
        }

        // The dialog first: it owns its ids and ignores the rest.
        if let Some(dialog) = &mut self.choosing {
            match dialog.accept(event) {
                Answer::Ignored => {}
                Answer::Changed => return true,
                Answer::Cancelled => {
                    self.choosing = None;
                    self.status = "kept the wallpaper as it was".into();
                    return true;
                }
                Answer::Chosen(path) => {
                    self.choosing = None;
                    self.choose(Some(path.to_string_lossy().into_owned()));
                    return true;
                }
            }
        }

        match (event.target.as_str(), event.action.as_str()) {
            ("wallpaper", display::ACTION_OPEN) => self.open = true,
            ("wallpaper", display::ACTION_CLOSE) => self.open = false,

            (target, display::ACTION_SELECT) if target.starts_with("wallpaper-") => {
                self.open = false;
                match &target["wallpaper-".len()..] {
                    CHOOSE => {
                        // Any picture on the machine. Starts where the shipped
                        // ones are, since that is where pictures are known to
                        // be, and lists only what the compositor can draw.
                        self.choosing = Some(
                            FileDialog::open(settings::WALLPAPER_DIR).only(&["svg", "png"]),
                        );
                        self.status = "choose a picture".into();
                    }
                    "current" => {}
                    index => {
                        if let Some((_, path)) = index.parse::<usize>().ok().and_then(|i| self.defaults.get(i)) {
                            let path = path.clone();
                            self.choose(Some(path));
                        }
                    }
                }
            }

            (target, display::ACTION_CLICK) if target.starts_with("category-") => {
                let name = &target["category-".len()..];
                if let Some((id, _)) = CATEGORIES.iter().find(|(id, _)| *id == name) {
                    self.category = id;
                }
            }

            _ => return false,
        }
        true
    }

    /// Record a wallpaper and say so.
    fn choose(&mut self, choice: Option<String>) {
        let mut stored = Stored::load();
        stored.wallpaper = choice.clone();
        match stored.save() {
            Ok(()) => {
                self.status = match &choice {
                    Some(path) => format!("wallpaper set to {path}"),
                    None => "wallpaper cleared".to_owned(),
                };
                if !settings::persistent() {
                    self.status.push_str(" (no state volume: kept until power off)");
                }
                self.current = choice;
                self.seen = settings::modified();
            }
            Err(err) => self.status = format!("could not save the setting: {err}"),
        }
        log(&self.status);
    }

    /// Whether the current wallpaper is one of the shipped ones.
    fn current_is_default(&self) -> bool {
        self.current
            .as_ref()
            .is_some_and(|path| self.defaults.iter().any(|(_, default)| default == path))
    }

    fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("<window title=\"Settings\" font=\"sans\">\n  <hstack gap=\"lg\" grow=\"true\">\n");

        // The page for the chosen category, with room to breathe.
        out.push_str("    <scroll grow=\"true\">\n      <vstack gap=\"md\">\n");
        match self.category {
            "desktop" => self.render_desktop(&mut out),
            _ => out.push_str("        <text color=\"muted\">Nothing here yet.</text>\n"),
        }
        out.push_str("      </vstack>\n    </scroll>\n");

        // A rule, then the rail of categories down the right.
        out.push_str("    <divider dir=\"vertical\"/>\n    <vstack gap=\"sm\">\n      <list label=\"Settings\">\n");
        for (id, label) in CATEGORIES {
            let _ = writeln!(
                out,
                r#"        <item id="category-{id}" label="{label}"{selected} description="Shows the {label} settings"/>"#,
                selected = if *id == self.category { r#" selected="true""# } else { "" },
            );
        }
        out.push_str("      </list>\n    </vstack>\n  </hstack>\n");

        if let Some(dialog) = &self.choosing {
            out.push_str(&dialog.render());
        }
        out.push_str("</window>\n");
        out
    }

    fn render_desktop(&self, out: &mut String) {
        out.push_str("        <text role=\"heading\">Desktop</text>\n");

        // One setting, one labelled group, stacked: the dropdown, then what
        // it chose, seen. The words are the group's label and the options'
        // names; the picture says the rest.
        out.push_str("        <group label=\"Wallpaper\">\n          <vstack gap=\"sm\">\n            <hstack>\n");

        // The dropdown. The value names the option chosen; a wallpaper from
        // outside the shipped set appears as its own option so the box can
        // show what is set.
        let value = match &self.current {
            Some(path) => match self.defaults.iter().position(|(_, default)| default == path) {
                Some(index) => index.to_string(),
                None => "current".to_owned(),
            },
            None => String::new(),
        };
        let _ = writeln!(
            out,
            r#"              <select id="wallpaper" value="{value}"{open} placeholder="No wallpaper" description="Chooses the wallpaper every agentdesk shows: one that ships with the system, or a picture chosen through the file dialog">"#,
            open = if self.open { r#" open="true""# } else { "" },
        );
        for (index, (name, path)) in self.defaults.iter().enumerate() {
            let _ = writeln!(
                out,
                r#"                <option id="wallpaper-{index}" label="{name}" value="{index}"{selected} description="Sets the wallpaper to {name}"/>"#,
                name = escape(name),
                selected = if self.current.as_deref() == Some(path.as_str()) { r#" selected="true""# } else { "" },
            );
        }
        if let (Some(path), false) = (&self.current, self.current_is_default()) {
            let name = Path::new(path).file_name().and_then(|n| n.to_str()).unwrap_or(path);
            let _ = writeln!(
                out,
                r#"                <option id="wallpaper-current" label="{name}" value="current" selected="true" description="The picture currently set, {path}"/>"#,
                name = escape(name),
                path = escape(path),
            );
        }
        let _ = writeln!(
            out,
            r#"                <option id="wallpaper-{CHOOSE}" label="Choose an image..." value="{CHOOSE}" description="Opens a file dialog to pick any SVG or PNG on the machine as the wallpaper"/>
              </select>
            </hstack>"#
        );

        // What is set, seen.
        if let Some(path) = &self.current {
            let _ = writeln!(
                out,
                r#"            <hstack>
              <image src="{path}" alt="The current wallpaper" fit="cover"/>
            </hstack>
            <text role="caption" color="muted">{path}</text>"#,
                path = escape(path),
            );
        }
        out.push_str("          </vstack>\n        </group>\n");
        if !self.status.is_empty() {
            let _ = writeln!(out, r#"        <text role="caption" color="muted">{}</text>"#, escape(&self.status));
        }
    }
}

fn log(message: &str) {
    let line = format!("<6>awsettings: {message}\n");
    if let Ok(mut kmsg) = std::fs::OpenOptions::new().write(true).open("/dev/kmsg") {
        let _ = kmsg.write_all(line.as_bytes());
    } else {
        eprintln!("awsettings: {message}");
    }
}
