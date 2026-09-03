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
use awproto::theme;

/// The categories down the rail.
const CATEGORIES: &[(&str, &str)] =
    &[("desktop", "Desktop"), ("time", "Time"), ("agent", "Agent")];

/// The dropdown's option for opening the file dialog.
const CHOOSE: &str = "choose";

struct Settings {
    category: &'static str,
    /// The wallpapers that ship with the system, as (name, path).
    defaults: Vec<(String, String)>,
    /// The current choice, `None` for a plain background.
    current: Option<String>,
    /// The themes that ship with the system, as (name, path).
    themes: Vec<(String, String)>,
    /// The theme in use, as a path. Always something: the screen always has
    /// a palette.
    theme: String,
    /// The time zone, as minutes east of UTC.
    utc_offset: i32,
    /// The Anthropic API key as saved, or `None` when the machine has none,
    /// so the page can say whether one is set.
    key: Option<String>,
    /// The key as typed but not yet saved: the field's contents, which may
    /// legitimately be empty mid-edit, so a plain string. A password is
    /// committed on Enter or the Save button, not per keystroke: half a
    /// pasted key is not a key, and every save is a synced write to the
    /// state volume.
    key_edit: String,
    /// The workspace the key acts in, as saved, or `None` when the machine
    /// has none. Only a key linked to an identity needs one; an ordinary
    /// workspace-scoped key names its own workspace and this stays empty.
    workspace: Option<String>,
    /// The workspace as typed but not yet saved, on the same terms as the
    /// key: committed on Enter or Save, never per keystroke.
    workspace_edit: String,
    /// When the settings file was last read, so a change made elsewhere is
    /// picked up before the next render rather than overwritten.
    seen: Option<SystemTime>,
    /// Whether the wallpaper dropdown is showing its options.
    open: bool,
    /// Whether the theme dropdown is showing its options.
    theme_open: bool,
    /// The file dialog, while one is up.
    choosing: Option<FileDialog>,
    /// The last thing that happened, shown under the page, or `None` when
    /// nothing has happened yet: absence is a state, not an empty string.
    status: Option<String>,
}

fn main() {
    let mut surface = match Surface::inherited() {
        Ok(surface) => surface,
        Err(err) => {
            log(&format!("no interface connection: {err}"));
            std::process::exit(1);
        }
    };

    let stored = Stored::load();
    let mut app = Settings {
        category: CATEGORIES[0].0,
        defaults: settings::wallpapers(),
        current: stored.wallpaper,
        themes: theme::themes(),
        theme: stored.theme,
        utc_offset: stored.utc_offset,
        key: stored.anthropic_key.clone(),
        key_edit: stored.anthropic_key.unwrap_or_default(),
        workspace: stored.anthropic_workspace.clone(),
        workspace_edit: stored.anthropic_workspace.unwrap_or_default(),
        seen: settings::modified(),
        open: false,
        theme_open: false,
        choosing: None,
        status: None,
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

        // The cross on the window. It asks rather than closing, because an
        // application may have something to lose; this one writes every change
        // straight to the settings file, so it answers by going. An application
        // that says nothing here is closed by the human's next press instead,
        // which is a click they should not have to spend.
        if event.action == display::ACTION_CLOSE && event.target.is_empty() {
            return;
        }

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
            let stored = Stored::load();
            self.current = stored.wallpaper;
            self.theme = stored.theme;
            self.utc_offset = stored.utc_offset;
            // A half-typed key survives an external change to the file; a
            // box that was showing the saved key follows it.
            if Some(self.key_edit.as_str()) == self.key.as_deref()
                || (self.key.is_none() && self.key_edit.is_empty())
            {
                self.key_edit = stored.anthropic_key.clone().unwrap_or_default();
            }
            self.key = stored.anthropic_key;
            if Some(self.workspace_edit.as_str()) == self.workspace.as_deref()
                || (self.workspace.is_none() && self.workspace_edit.is_empty())
            {
                self.workspace_edit = stored.anthropic_workspace.clone().unwrap_or_default();
            }
            self.workspace = stored.anthropic_workspace;
        }

        // The dialog first: it owns its ids and ignores the rest.
        if let Some(dialog) = &mut self.choosing {
            match dialog.accept(event) {
                Answer::Ignored => {}
                Answer::Changed => return true,
                Answer::Cancelled => {
                    self.choosing = None;
                    self.status = Some("kept the wallpaper as it was".into());
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

            ("theme", display::ACTION_OPEN) => self.theme_open = true,
            ("theme", display::ACTION_CLOSE) => self.theme_open = false,

            (target, display::ACTION_SELECT) if target.starts_with("theme-") => {
                self.theme_open = false;
                match &target["theme-".len()..] {
                    "current" => {}
                    index => {
                        if let Some((_, path)) =
                            index.parse::<usize>().ok().and_then(|i| self.themes.get(i))
                        {
                            let path = path.clone();
                            self.choose_theme(path);
                        }
                    }
                }
            }

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
                        self.status = Some("choose a picture".into());
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

            (target, display::ACTION_CLICK) if target.starts_with("zone-") => {
                let Some(minutes) = target["zone-".len()..].parse::<usize>().ok()
                    .and_then(|i| settings::utc_offsets().get(i).map(|(_, m)| *m))
                else {
                    return false;
                };
                let mut stored = Stored::load();
                stored.utc_offset = minutes;
                let status = match stored.save() {
                    Ok(()) => {
                        self.utc_offset = minutes;
                        self.seen = settings::modified();
                        format!("time zone set to UTC{}", settings::format_offset(minutes))
                    }
                    Err(err) => format!("could not save the setting: {err}"),
                };
                log(&status);
                self.status = Some(status);
            }

            // The key is typed (or pasted through the compositor's caret) and
            // committed as one save, Enter or the button alike.
            ("api-key", display::ACTION_TYPE_TEXT) => self.key_edit = event.value.clone(),
            ("api-key", display::ACTION_SUBMIT) | ("api-key-save", display::ACTION_CLICK) => {
                self.save_key()
            }
            ("workspace-id", display::ACTION_TYPE_TEXT) => {
                self.workspace_edit = event.value.clone()
            }
            ("workspace-id", display::ACTION_SUBMIT)
            | ("workspace-id-save", display::ACTION_CLICK) => self.save_workspace(),

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
        let status = match stored.save() {
            Ok(()) => {
                let mut status = match &choice {
                    Some(path) => format!("wallpaper set to {path}"),
                    None => "wallpaper cleared".to_owned(),
                };
                if !settings::persistent() {
                    status.push_str(" (no state volume: kept until power off)");
                }
                self.current = choice;
                self.seen = settings::modified();
                status
            }
            Err(err) => format!("could not save the setting: {err}"),
        };
        log(&status);
        self.status = Some(status);
    }

    /// Record a theme and say so. The compositor notices the file's clock
    /// moved, exactly as every agentdesk notices a wallpaper, so the palette
    /// changes within a second of the click.
    fn choose_theme(&mut self, path: String) {
        let mut stored = Stored::load();
        stored.theme = path.clone();
        let status = match stored.save() {
            Ok(()) => {
                let mut status = format!("theme set to {path}");
                if !settings::persistent() {
                    status.push_str(" (no state volume: kept until power off)");
                }
                self.theme = path;
                self.seen = settings::modified();
                status
            }
            Err(err) => format!("could not save the setting: {err}"),
        };
        log(&status);
        self.status = Some(status);
    }

    /// Save the key as typed. An emptied box clears it, which is how a key is
    /// revoked from the machine's side.
    fn save_key(&mut self) {
        let typed = self.key_edit.trim();
        let key: Option<String> = (!typed.is_empty()).then(|| typed.to_owned());
        let mut stored = Stored::load();
        stored.anthropic_key = key.clone();
        let status = match stored.save() {
            Ok(()) => {
                let mut status = match &key {
                    None => "API key cleared".to_owned(),
                    Some(key) => format!("API key saved ({} characters)", key.chars().count()),
                };
                if !settings::persistent() {
                    status.push_str(" (no state volume: kept until power off)");
                }
                self.key_edit = key.clone().unwrap_or_default();
                self.key = key;
                self.seen = settings::modified();
                status
            }
            Err(err) => format!("could not save the setting: {err}"),
        };
        log(&status);
        self.status = Some(status);
    }

    /// Whether the current theme is one of the shipped ones.
    fn theme_is_default(&self) -> bool {
        self.themes.iter().any(|(_, path)| *path == self.theme)
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
            "time" => self.render_time(&mut out),
            "agent" => self.render_agent(&mut out),
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

        // The theme: the palette the compositor paints everything with,
        // chosen from the ones that ship. The compositor re-reads the
        // settings file when its clock moves, so the choice takes hold
        // within a second, this window included.
        out.push_str("        <group label=\"Theme\">\n          <vstack gap=\"sm\">\n            <hstack>\n");
        let value = match self.themes.iter().position(|(_, path)| *path == self.theme) {
            Some(index) => index.to_string(),
            None => "current".to_owned(),
        };
        let _ = writeln!(
            out,
            r#"              <select id="theme" value="{value}"{open} placeholder="Theme" description="Chooses the colours everything on the machine is drawn in">"#,
            open = if self.theme_open { r#" open="true""# } else { "" },
        );
        for (index, (name, path)) in self.themes.iter().enumerate() {
            let _ = writeln!(
                out,
                r#"                <option id="theme-{index}" label="{name}" value="{index}"{selected} description="Sets the theme to {name}"/>"#,
                name = escape(name),
                selected = if self.theme == *path { r#" selected="true""# } else { "" },
            );
        }
        if !self.theme_is_default() {
            let name = Path::new(&self.theme).file_stem().and_then(|n| n.to_str()).unwrap_or(&self.theme);
            let _ = writeln!(
                out,
                r#"                <option id="theme-current" label="{name}" value="current" selected="true" description="The theme currently set, {path}"/>"#,
                name = escape(name),
                path = escape(&self.theme),
            );
        }
        out.push_str("              </select>\n            </hstack>\n");
        let _ = writeln!(
            out,
            r#"            <text role="caption" color="muted">{}</text>"#,
            escape(&self.theme),
        );
        out.push_str("          </vstack>\n        </group>\n");

        if let Some(status) = &self.status {
            let _ = writeln!(out, r#"        <text role="caption" color="muted">{}</text>"#, escape(status));
        }
    }
}

impl Settings {
    /// The Time page: the zone as an offset from UTC, picked from a list.
    ///
    /// An offset rather than a named zone, because the image carries no zone
    /// database and the kernel has no notion of one either: it keeps UTC and
    /// nothing else, and a zone is whatever userspace adds when it shows a
    /// time. A list rather than a dropdown, because forty rows want a
    /// scrolling list, not a menu hanging off a box.
    fn render_time(&self, out: &mut String) {
        let now = local_clock(self.utc_offset);
        let _ = write!(
            out,
            r#"        <text role="heading">Time</text>
        <group label="Time zone">
          <vstack gap="sm">
            <text>Now {now}, UTC{offset}</text>
            <text role="caption" color="muted">The clock is kept in UTC and shown shifted by this. No daylight saving: set it again when the clocks change.</text>
            <scroll grow="true">
              <list>
"#,
            offset = settings::format_offset(self.utc_offset),
        );
        for (index, (label, minutes)) in settings::utc_offsets().iter().enumerate() {
            let _ = writeln!(
                out,
                r#"                <item id="zone-{index}" label="{label}"{selected} description="Sets the time zone to {label}"/>"#,
                selected = if *minutes == self.utc_offset { r#" selected="true""# } else { "" },
            );
        }
        out.push_str("              </list>\n            </scroll>\n          </vstack>\n        </group>\n");
        if let Some(status) = &self.status {
            let _ = writeln!(out, r#"        <text role="caption" color="muted">{}</text>"#, escape(status));
        }
    }

    /// Save the workspace as typed. An emptied box clears it, which is what
    /// an ordinary workspace-scoped key wants: the header is then not sent
    /// at all, rather than sent empty.
    fn save_workspace(&mut self) {
        let typed = self.workspace_edit.trim();
        let workspace: Option<String> = (!typed.is_empty()).then(|| typed.to_owned());
        let mut stored = Stored::load();
        stored.anthropic_workspace = workspace.clone();
        let status = match stored.save() {
            Ok(()) => {
                let mut status = match &workspace {
                    None => "workspace cleared".to_owned(),
                    Some(id) => format!("workspace saved ({id})"),
                };
                if !settings::persistent() {
                    status.push_str(" (no state volume: kept until power off)");
                }
                self.workspace_edit = workspace.clone().unwrap_or_default();
                self.workspace = workspace;
                self.seen = settings::modified();
                status
            }
            Err(err) => format!("could not save the setting: {err}"),
        };
        log(&status);
        self.status = Some(status);
    }

    /// The Agent page: the key the agent authenticates with. Which model
    /// answers is not a machine setting: it is chosen per agentdesk, in the
    /// pane, beside the conversation it applies to.
    ///
    /// The key is a password field, so the compositor masks it on screen and
    /// in every agent's view alike; this window only ever learns what was
    /// typed, and says whether a key is set rather than what it is. Committed
    /// on Enter or the Save button rather than per keystroke, because every
    /// save is a synced write to the state volume and half a pasted key is
    /// not a key.
    fn render_agent(&self, out: &mut String) {
        out.push_str("        <text role=\"heading\">Agent</text>\n");

        out.push_str("        <group label=\"Anthropic API key\">\n          <vstack gap=\"sm\">\n");
        let _ = writeln!(
            out,
            r#"            <hstack gap="sm">
              <field id="api-key" kind="password" value="{value}" placeholder="sk-ant-..." description="The Anthropic API key the agent authenticates with"/>
              <button id="api-key-save" label="Save" emphasis="primary" description="Saves the API key as typed"/>
            </hstack>"#,
            value = escape(&self.key_edit),
        );
        let standing = match &self.key {
            None => "No key is set: the agent will answer that it cannot reach a model.".to_owned(),
            Some(key) => format!("A key is set ({} characters).", key.chars().count()),
        };
        let _ = writeln!(out, r#"            <text role="caption" color="muted">{}</text>"#, escape(&standing));

        // The workspace under the key it qualifies, in the same group: it is
        // part of authenticating, not a setting of its own. Plain text rather
        // than a password, because it is not a secret and a wrong one is only
        // spottable if it can be read.
        let _ = writeln!(
            out,
            r#"            <hstack gap="sm">
              <field id="workspace-id" value="{value}" placeholder="wrkspc_... (only if the key is not scoped to one)" description="The Anthropic workspace the key acts in"/>
              <button id="workspace-id-save" label="Save" description="Saves the workspace as typed"/>
            </hstack>"#,
            value = escape(&self.workspace_edit),
        );
        if let Some(id) = &self.workspace {
            let _ = writeln!(
                out,
                r#"            <text role="caption" color="muted">Workspace {}.</text>"#,
                escape(id)
            );
        }

        let _ = writeln!(
            out,
            r#"            <text role="caption" color="muted">Which model answers is chosen in each agentdesk's pane.</text>"#
        );
        out.push_str("          </vstack>\n        </group>\n");

        if let Some(status) = &self.status {
            let _ = writeln!(out, r#"        <text role="caption" color="muted">{}</text>"#, escape(status));
        }
    }
}

/// Hours and minutes in the zone, as the taskbar would show them right now.
fn local_clock(utc_offset: i32) -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
        + utc_offset as i64 * 60;
    let secs = secs.rem_euclid(86_400);
    format!("{:02}:{:02}", secs / 3600, (secs % 3600) / 60)
}

fn log(message: &str) {
    let line = format!("<6>awsettings: {message}\n");
    if let Ok(mut kmsg) = std::fs::OpenOptions::new().write(true).open("/dev/kmsg") {
        let _ = kmsg.write_all(line.as_bytes());
    } else {
        eprintln!("awsettings: {message}");
    }
}
