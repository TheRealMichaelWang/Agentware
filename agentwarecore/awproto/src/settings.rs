//! The one thing that crosses between processes on the filesystem: settings.
//!
//! Everything else in Agentware travels over a socket the supervisor created,
//! and there is no persistent storage by choice. But a preference the human sets
//! in one process and every workspace should honour has no socket to travel on:
//! the settings application is an ordinary app that links this crate and nothing
//! else, and an agentdesk is never handed a descriptor to it. A file in the
//! runtime directory is the smallest thing that connects them. The settings app
//! writes it, the agentdesks read it on their clock tick, and nothing survives a
//! reboot because the directory is RAM.
//!
//! One value per file, plain text, so a setting can be read with `cat` and
//! nothing has to agree on a syntax.

/// Where settings live. Created by whoever writes first.
pub const SETTINGS_DIR: &str = "/run/agentware/settings";

/// The path of the wallpaper every workspace should show, or the word `none`
/// for a plain background. Absent means the default.
pub const WALLPAPER_FILE: &str = "/run/agentware/settings/wallpaper";

/// Where the wallpapers that ship with the system live: SVG or PNG files, one
/// per wallpaper, named for people. A wallpaper need not come from here; the
/// settings application also takes any picture chosen through the file dialog.
pub const WALLPAPER_DIR: &str = "/default_wallpapers";

/// The wallpaper shown before anyone has chosen one.
pub const DEFAULT_WALLPAPER: &str = "/default_wallpapers/dusk.svg";

/// The setting's value for no wallpaper at all.
pub const WALLPAPER_NONE: &str = "none";

/// The current wallpaper choice: `None` for a plain background, otherwise a
/// path. Falls back to the default when nothing has been written.
pub fn wallpaper() -> Option<String> {
    match std::fs::read_to_string(WALLPAPER_FILE) {
        Ok(text) => {
            let value = text.trim();
            if value == WALLPAPER_NONE {
                None
            } else if value.is_empty() {
                Some(DEFAULT_WALLPAPER.to_owned())
            } else {
                Some(value.to_owned())
            }
        }
        Err(_) => Some(DEFAULT_WALLPAPER.to_owned()),
    }
}

/// Record a wallpaper choice, `None` for a plain background.
pub fn set_wallpaper(path: Option<&str>) -> std::io::Result<()> {
    std::fs::create_dir_all(SETTINGS_DIR)?;
    std::fs::write(WALLPAPER_FILE, path.unwrap_or(WALLPAPER_NONE))
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
