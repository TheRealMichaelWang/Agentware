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
//!     <theme>/default_themes/dark.xml</theme>
//!   </desktop>
//!   <time>
//!     <utc-offset>+00:00</utc-offset>
//!   </time>
//!   <agent>
//!     <anthropic-key>sk-ant-...</anthropic-key>
//!     <workspace-id>wrkspc_...</workspace-id>
//!   </agent>
//! </settings>
//! ```
//!
//! ## The agent
//!
//! The per-turn agent has a model behind it, and a model needs a key. The key
//! is entered once, on the Settings app's Agent page, and read by every agent
//! the broker forks from then on; like every other setting it crosses between
//! processes as this file and nothing else. Which model answers is not here:
//! that is chosen per agentdesk, in the pane, because it is a property of the
//! conversation being had rather than of the machine having it. It is a secret, and the file is
//! plain text on the state volume: that is the machine's own disk, the same
//! place a browser keeps its cookies, and nothing here pretends otherwise.
//! What the system does promise is that an agent never sees it back through
//! the screen: a password field's value is masked in the agent's view by the
//! compositor, so the one process that could echo the key to a model reads
//! dots.
//!
//! The workspace id beside it is not a second secret. A key linked to an
//! identity belongs to an organisation rather than to one workspace, and the
//! API refuses such a key with `400 anthropic-workspace-id required when
//! authenticated with api key linked to identity` until the request says
//! which workspace to bill and scope to. An ordinary workspace-scoped key
//! carries that in itself and needs nothing here, so this is `None` until a
//! human sets it and the header is sent only when it is `Some`.
//!
//! ## Time
//!
//! The kernel keeps one clock, UTC, and knows nothing of time zones; a zone
//! is a userspace convention for turning that clock into the numbers on a
//! wall. Agentware has no zone database in its image, so its zone is the
//! simplest true thing: an offset from UTC, chosen once here and applied by
//! whatever shows a time. No daylight saving: an offset is what it says, and
//! a person who moves between the two sets it twice a year, which is what a
//! wall clock asks of them too.
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
    /// The theme the compositor paints with: a path to a theme file. Always
    /// something, because the screen always has a palette; a path that stops
    /// loading leaves the compositor on the palette it already holds.
    pub theme: String,
    /// The time zone, as minutes east of UTC. Zero until someone says
    /// otherwise, which is the one honest default for a machine that cannot
    /// know where it is.
    pub utc_offset: i32,
    /// The Anthropic API key the agent authenticates with, or `None` until
    /// the human enters one on the Agent settings page. `None` rather than an
    /// empty string, so "is there a key" is a question the type answers and
    /// no caller can forget to ask; a `Some` is never empty. An agent asked
    /// to work without one answers with where to set it rather than failing
    /// mutely.
    pub anthropic_key: Option<String>,
    /// The workspace the key acts in, for a key linked to an identity, which
    /// the API will not accept without one. `None` for an ordinary
    /// workspace-scoped key, which already says which workspace it is.
    pub anthropic_workspace: Option<String>,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            wallpaper: Some(DEFAULT_WALLPAPER.to_owned()),
            theme: crate::theme::DEFAULT_THEME.to_owned(),
            utc_offset: 0,
            anthropic_key: None,
            anthropic_workspace: None,
        }
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
            "<settings>\n  <desktop>\n    <wallpaper>{}</wallpaper>\n    <theme>{}</theme>\n  </desktop>\n  <time>\n    <utc-offset>{}</utc-offset>\n  </time>\n  <agent>\n    <anthropic-key>{}</anthropic-key>\n    <workspace-id>{}</workspace-id>\n  </agent>\n</settings>\n",
            escape(wallpaper),
            escape(&self.theme),
            format_offset(self.utc_offset),
            escape(self.anthropic_key.as_deref().unwrap_or("")),
            escape(self.anthropic_workspace.as_deref().unwrap_or("")),
        )
    }
}

/// `+05:30` for 330 minutes, `-04:00` for -240, `+00:00` for none.
pub fn format_offset(minutes: i32) -> String {
    let sign = if minutes < 0 { '-' } else { '+' };
    let minutes = minutes.abs();
    format!("{sign}{:02}:{:02}", minutes / 60, minutes % 60)
}

/// The offset in `+HH:MM` or `-HH:MM`, or `None` for anything else.
pub fn parse_offset(text: &str) -> Option<i32> {
    let text = text.trim();
    let (sign, rest) = match text.chars().next()? {
        '+' => (1, &text[1..]),
        '-' => (-1, &text[1..]),
        _ => return None,
    };
    let (hours, minutes) = rest.split_once(':')?;
    let hours: i32 = hours.parse().ok()?;
    let minutes: i32 = minutes.parse().ok()?;
    if !(0..=14).contains(&hours) || !(0..60).contains(&minutes) {
        return None;
    }
    Some(sign * (hours * 60 + minutes))
}

/// The offsets a settings page offers, as (label, minutes), west to east:
/// every whole hour from -12 to +14, and the half and three-quarter hours
/// that places actually keep.
pub fn utc_offsets() -> Vec<(String, i32)> {
    let mut all: Vec<i32> = (-12..=14).map(|h| h * 60).collect();
    all.extend([-570, -210, 210, 270, 330, 345, 390, 525, 570, 630, 765]);
    all.sort_unstable();
    all.dedup();
    all.into_iter().map(|minutes| (format!("UTC{}", format_offset(minutes)), minutes)).collect()
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

/// `deep-space` becomes `Deep space`. Shared with the theme listing, which
/// names its files the same way.
pub(crate) fn display_name(stem: &str) -> String {
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
    // A file from before themes existed has no element; the default theme is
    // what such a machine was showing anyway.
    let theme = match values.get("settings/desktop/theme").map(String::as_str) {
        Some(path) if !path.is_empty() => path.to_owned(),
        _ => crate::theme::DEFAULT_THEME.to_owned(),
    };
    // A missing or malformed offset is UTC, not a refusal: an old file, or a
    // hand edit that went wrong, should not lose the wallpaper with it.
    let utc_offset = values
        .get("settings/time/utc-offset")
        .and_then(|text| parse_offset(text))
        .unwrap_or(0);
    // A file from before the agent had settings has no element, and a cleared
    // key writes an empty one; both read back as no key at all. Whitespace is
    // not a key either, so a hand edit that leaves a stray space cannot make
    // `Some` mean nothing.
    let anthropic_key = values
        .get("settings/agent/anthropic-key")
        .map(|text| text.trim())
        .filter(|text| !text.is_empty())
        .map(str::to_owned);
    let anthropic_workspace = values
        .get("settings/agent/workspace-id")
        .map(|text| text.trim())
        .filter(|text| !text.is_empty())
        .map(str::to_owned);
    Some(Settings { wallpaper, theme, utc_offset, anthropic_key, anthropic_workspace })
}

/// Every element's text, keyed by its path from the root, `a/b/c`. Elements
/// with children have their own entry too, with whatever text sits between
/// them (usually nothing but whitespace, trimmed away). Shared with the
/// theme reader, which follows the same shape of file.
pub(crate) fn read_elements(text: &str) -> Option<BTreeMap<String, String>> {
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
        let settings = Settings {
            wallpaper: Some("/pictures/a & b.png".into()),
            theme: "/themes/mine & yours.xml".into(),
            utc_offset: -300,
            anthropic_key: Some("sk-ant-a&b<c>\"d\"".into()),
            anthropic_workspace: Some("wrkspc_&<>".into()),
        };
        assert_eq!(parse(&settings.to_xml()), Some(settings));
        let none = Settings { wallpaper: None, utc_offset: 345, ..Settings::default() };
        assert_eq!(parse(&none.to_xml()), Some(none));
    }

    #[test]
    fn no_key_is_none() {
        // A file from before the agent had settings simply has no element.
        let old = "<settings><desktop><wallpaper>/x.svg</wallpaper></desktop></settings>";
        assert_eq!(parse(old).unwrap().anthropic_key, None);
        // A cleared key writes an empty element, and whitespace is not a key:
        // `Some` is never empty, which is what lets callers trust it.
        let cleared = "<settings><agent><anthropic-key>  </anthropic-key></agent></settings>";
        assert_eq!(parse(cleared).unwrap().anthropic_key, None);
    }

    #[test]
    fn workspace_is_optional_and_independent_of_the_key() {
        // The ordinary case: a workspace-scoped key, no workspace element.
        let keyed = "<settings><agent><anthropic-key>sk-ant-x</anthropic-key></agent></settings>";
        let settings = parse(keyed).unwrap();
        assert_eq!(settings.anthropic_key.as_deref(), Some("sk-ant-x"));
        assert_eq!(settings.anthropic_workspace, None);
        // An identity-linked key, which the API refuses without this.
        let both = "<settings><agent><anthropic-key>sk-ant-x</anthropic-key><workspace-id> wrkspc_1 </workspace-id></agent></settings>";
        assert_eq!(parse(both).unwrap().anthropic_workspace.as_deref(), Some("wrkspc_1"));
        // Cleared reads as absent, so the header is not sent empty.
        let cleared = "<settings><agent><workspace-id></workspace-id></agent></settings>";
        assert_eq!(parse(cleared).unwrap().anthropic_workspace, None);
    }

    #[test]
    fn offsets() {
        assert_eq!(format_offset(330), "+05:30");
        assert_eq!(format_offset(-240), "-04:00");
        assert_eq!(parse_offset("+05:30"), Some(330));
        assert_eq!(parse_offset("-04:00"), Some(-240));
        assert_eq!(parse_offset("05:30"), None);
        assert_eq!(parse_offset("+25:00"), None);
        assert!(utc_offsets().iter().any(|(label, m)| label == "UTC+05:45" && *m == 345));
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
