//! Settings: the one thing that crosses between processes on the filesystem,
//! and the one thing that outlives the machine.
//!
//! Everything else in Agentware travels over a socket the supervisor created,
//! and workspaces, apps and conversations live in RAM and die at power off. A
//! preference is different on both counts. It is set in one process and read
//! by every workspace, which have no socket between them; and it is the kind
//! of thing a person expects to find as they left it, so it lives on the state
//! volume the supervisor mounts at boot, in one file, `settings.xml`.
//!
//! ## The file
//!
//! ```xml
//! <settings>
//!   <desktop>
//!     <wallpaper>/default_wallpapers/dusk.svg</wallpaper>
//!   </desktop>
//! </settings>
//! ```
//!
//! Elements nest by category, text is the value, and the five XML entities
//! are escaped, so a person can read it with `cat` and a future setting is one
//! more element. The first boot has no file; whoever loads first writes the
//! defaults, so from then on the file is the truth and every reader agrees
//! with it. The settings application writes it when the human chooses; every
//! agentdesk stats it on its clock tick and re-reads it when it has changed,
//! so a choice reaches every workspace within a second and no process is told.
//!
//! ## Where it lives
//!
//! `/state`, the volume the supervisor mounts from the machine's disk (a
//! virtio drive under QEMU, a partition on hardware). If there is no such
//! volume, `/state` is a directory in the RAM image and the file lasts until
//! power off, which is what it did before there was a volume: degraded, not
//! broken, and the log says so at boot.
//!
//! The reader here is deliberately small: it understands the file this module
//! writes, tolerates whitespace and unknown elements, and nothing else. A
//! settings file is not a place for a general XML parser.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;
use std::time::SystemTime;

/// The state volume, mounted by the supervisor when the machine has one.
pub const STATE_DIR: &str = "/state";

/// The settings file.
pub const SETTINGS_FILE: &str = "/state/settings.xml";

/// Where the wallpapers that ship with the system live: SVG or PNG files, one
/// per wallpaper, named for people. A wallpaper need not come from here; the
/// settings application also takes any picture chosen through the file dialog.
pub const WALLPAPER_DIR: &str = "/default_wallpapers";

/// The wallpaper shown before anyone has chosen one.
pub const DEFAULT_WALLPAPER: &str = "/default_wallpapers/dusk.svg";

/// The setting's value for no wallpaper at all.
pub const WALLPAPER_NONE: &str = "none";

/// Every setting, as loaded from the file or as it should be written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    /// The wallpaper every workspace shows: a path, or `None` for a plain
    /// background.
    pub wallpaper: Option<String>,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings { wallpaper: Some(DEFAULT_WALLPAPER.to_owned()) }
    }
}

impl Settings {
    /// Read the settings file, creating it with the defaults if there is none.
    ///
    /// The first boot has no file; the first process to ask writes one, so
    /// the file exists from then on and every reader reads the same thing. A
    /// file that cannot be read or parsed is treated as absent and rewritten
    /// with the defaults rather than guessed at, and the log says so.
    pub fn load() -> Settings {
        match std::fs::read_to_string(SETTINGS_FILE) {
            Ok(text) => match parse(&text) {
                Some(settings) => settings,
                None => {
                    let settings = Settings::default();
                    let _ = settings.save();
                    settings
                }
            },
            Err(_) => {
                let settings = Settings::default();
                let _ = settings.save();
                settings
            }
        }
    }

    /// Write the file, whole, atomically and durably: to a sibling, synced,
    /// renamed into place, and the directory synced, so a reader on its clock
    /// tick never sees half a file and a power cut a moment later does not
    /// lose the choice. This is the machine's disk; the write is worth the
    /// two syncs, and nothing else here writes at all.
    pub fn save(&self) -> io::Result<()> {
        use std::io::Write;
        std::fs::create_dir_all(STATE_DIR)?;
        let temp = format!("{SETTINGS_FILE}.new");
        {
            let mut file = std::fs::File::create(&temp)?;
            file.write_all(self.to_xml().as_bytes())?;
            file.sync_all()?;
        }
        std::fs::rename(&temp, SETTINGS_FILE)?;
        std::fs::File::open(STATE_DIR)?.sync_all()
    }

    /// The file's contents for these settings.
    pub fn to_xml(&self) -> String {
        let wallpaper = self.wallpaper.as_deref().unwrap_or(WALLPAPER_NONE);
        format!(
            "<settings>\n  <desktop>\n    <wallpaper>{}</wallpaper>\n  </desktop>\n</settings>\n",
            escape(wallpaper)
        )
    }
}

/// When the file last changed, for a reader that polls. `None` if there is no
/// file yet.
pub fn modified() -> Option<SystemTime> {
    std::fs::metadata(SETTINGS_FILE).and_then(|meta| meta.modified()).ok()
}

/// Whether the settings file is on a volume that outlives the machine.
///
/// True when `/state` is a mount point rather than a directory in the RAM
/// image: its device differs from its parent's. What the supervisor logs at
/// boot, and what the settings page can say if it is not.
pub fn persistent() -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(state) = std::fs::metadata(STATE_DIR) else { return false };
    let Ok(root) = std::fs::metadata("/") else { return false };
    state.dev() != root.dev()
}

/// The wallpapers that ship with the system, as (display name, path), in name
/// order.
pub fn wallpapers() -> Vec<(String, String)> {
    let Ok(entries) = std::fs::read_dir(WALLPAPER_DIR) else { return Vec::new() };
    let mut found: Vec<(String, String)> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            let extension = path.extension()?.to_str()?;
            if !matches!(extension, "svg" | "png") {
                return None;
            }
            let stem = path.file_stem()?.to_str()?;
            Some((display_name(stem), path.to_str()?.to_owned()))
        })
        .collect();
    found.sort();
    found
}

/// `deep-space` becomes `Deep space`.
fn display_name(stem: &str) -> String {
    let mut out = String::with_capacity(stem.len());
    for (at, part) in stem.split(['-', '_']).enumerate() {
        if at > 0 {
            out.push(' ');
        }
        let mut chars = part.chars();
        if let Some(first) = chars.next() {
            if at == 0 {
                out.extend(first.to_uppercase());
            } else {
                out.push(first);
            }
            out.push_str(chars.as_str());
        }
    }
    out
}

/// Whether a path names something the compositor could show.
pub fn is_picture(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| matches!(e.to_lowercase().as_str(), "svg" | "png"))
}

// ---- the file format ----------------------------------------------------------

/// Read the file into settings. `None` if it is not the file this module
/// writes: no `<settings>` root, or markup the reader does not follow.
fn parse(text: &str) -> Option<Settings> {
    let values = read_elements(text)?;
    if !values.contains_key("settings") {
        return None;
    }
    let wallpaper = match values.get("settings/desktop/wallpaper").map(String::as_str) {
        None => Some(DEFAULT_WALLPAPER.to_owned()),
        Some(WALLPAPER_NONE) | Some("") => None,
        Some(path) => Some(path.to_owned()),
    };
    Some(Settings { wallpaper })
}

/// Every element's text, keyed by its path from the root, `a/b/c`. Elements
/// with children have their own entry too, with whatever text sits between
/// them (usually nothing but whitespace, trimmed away).
fn read_elements(text: &str) -> Option<BTreeMap<String, String>> {
    let mut values: BTreeMap<String, String> = BTreeMap::new();
    let mut stack: Vec<String> = Vec::new();
    let mut rest = text;

    // A declaration or comment before the root is skipped, so a file touched
    // by hand still reads.
    loop {
        rest = rest.trim_start();
        if let Some(after) = rest.strip_prefix("<?") {
            rest = &after[after.find("?>")? + 2..];
        } else if let Some(after) = rest.strip_prefix("<!--") {
            rest = &after[after.find("-->")? + 3..];
        } else {
            break;
        }
    }

    while let Some(open) = rest.find('<') {
        let content = &rest[..open];
        if let Some(path) = stack.last()
            && !content.trim().is_empty()
        {
            let entry = values.entry(path.clone()).or_default();
            entry.push_str(&unescape(content.trim()));
        }
        let close = rest[open..].find('>')? + open;
        let tag = rest[open + 1..close].trim();
        rest = &rest[close + 1..];

        if let Some(name) = tag.strip_prefix('/') {
            let name = name.trim();
            let top = stack.pop()?;
            if top.rsplit('/').next() != Some(name) {
                return None;
            }
            continue;
        }
        let self_closing = tag.ends_with('/');
        let name = tag.trim_end_matches('/').split_whitespace().next()?;
        let path = match stack.last() {
            Some(parent) => format!("{parent}/{name}"),
            None => name.to_owned(),
        };
        values.entry(path.clone()).or_default();
        if !self_closing {
            stack.push(path);
        }
    }

    if stack.is_empty() { Some(values) } else { None }
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(character),
        }
    }
    out
}

fn unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let settings = Settings { wallpaper: Some("/pictures/a & b.png".into()) };
        assert_eq!(parse(&settings.to_xml()), Some(settings));
        let none = Settings { wallpaper: None };
        assert_eq!(parse(&none.to_xml()), Some(none));
    }

    #[test]
    fn tolerates_hand_edits_and_refuses_junk() {
        let text = "<?xml version=\"1.0\"?>\n<!-- mine -->\n<settings>\n  <desktop>\n    <wallpaper> /x.svg </wallpaper>\n    <future/>\n  </desktop>\n</settings>";
        assert_eq!(parse(text).unwrap().wallpaper.as_deref(), Some("/x.svg"));
        assert!(parse("<other/>").is_none());
        assert!(parse("<settings><desktop>").is_none());
        assert_eq!(parse("<settings/>").unwrap(), Settings::default());
    }
}
