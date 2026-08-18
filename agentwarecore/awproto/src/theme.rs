//! Themes: the palette the compositor paints with, as a file.
//!
//! The thirteen colours in `haimanager`'s `ui` module were hardcoded and
//! looked right together, so together is how they travel: one XML file per
//! theme in `/default_themes`, named for people the way wallpapers are, with
//! the original palette shipped as `dark.xml`. Which theme the machine uses
//! is a setting (`settings.xml`, beside the wallpaper), chosen on the
//! Settings app's Desktop page and noticed by the compositor the way every
//! settings change is noticed: the file's clock moved, so re-read it.
//!
//! ## The file
//!
//! ```xml
//! <theme>
//!   <background>#0e1016</background>
//!   <surface>#1a1e28</surface>
//!   ...
//! </theme>
//! ```
//!
//! A theme names every colour, and a file that misses or mangles one is
//! refused whole rather than patched: there is nothing in the code to patch
//! it with, deliberately. The palette lives in the file and only in the
//! file; a copy compiled in would drift from it and defeat the point of
//! loading one. Colours are `#rrggbb`, matching what applications may
//! already write in their markup. A refused theme costs the machine
//! nothing but a log line: the caller keeps the palette it already has.

use crate::settings::read_elements;

/// Where the themes that ship with the system live, one XML file each.
pub const THEME_DIR: &str = "/default_themes";

/// The theme used before anyone has chosen one: the palette the compositor
/// was born with.
pub const DEFAULT_THEME: &str = "/default_themes/dark.xml";

/// A colour as the compositor holds one: `0x00RRGGBB`.
pub type Color = u32;

/// The palette, one field per thing the compositor paints.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Theme {
    /// The deepest layer: the desk behind everything, window bodies.
    pub background: Color,
    /// A layer above it: panels, the pane, title bars of unfocused windows.
    pub surface: Color,
    /// A layer above that: buttons, the navigation bar, focused title bars.
    pub raised: Color,
    /// One step above raised, for a control under the pointer or pressed.
    pub pressed: Color,
    pub border: Color,
    pub text: Color,
    pub muted: Color,
    pub accent: Color,
    pub accent_deep: Color,
    pub danger: Color,
    pub danger_deep: Color,
    pub ok: Color,
    pub selected: Color,
}

impl Theme {
    /// Read a theme file. `None` if it cannot be read or is not a whole
    /// theme, which a caller should treat as "keep what you have".
    pub fn load(path: &str) -> Option<Theme> {
        parse(&std::fs::read_to_string(path).ok()?)
    }
}

/// The themes that ship with the system, as (display name, path), in name
/// order. The name comes from the file stem the way a wallpaper's does, so a
/// theme is added by dropping a file in, not by registering it anywhere.
pub fn themes() -> Vec<(String, String)> {
    let Ok(entries) = std::fs::read_dir(THEME_DIR) else { return Vec::new() };
    let mut found: Vec<(String, String)> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension()?.to_str()? != "xml" {
                return None;
            }
            let stem = path.file_stem()?.to_str()?;
            Some((crate::settings::display_name(stem), path.to_str()?.to_owned()))
        })
        .collect();
    found.sort();
    found
}

/// Read the file into a theme. `None` if there is no `<theme>` root, the
/// markup does not follow, or any colour is missing or malformed: a theme
/// names all thirteen, because there is deliberately no palette in the code
/// to fill a gap with.
fn parse(text: &str) -> Option<Theme> {
    let values = read_elements(text)?;
    if !values.contains_key("theme") {
        return None;
    }
    let color = |name: &str| {
        values
            .get(&format!("theme/{name}"))
            .and_then(|text| parse_color(text))
    };
    Some(Theme {
        background: color("background")?,
        surface: color("surface")?,
        raised: color("raised")?,
        pressed: color("pressed")?,
        border: color("border")?,
        text: color("text")?,
        muted: color("muted")?,
        accent: color("accent")?,
        accent_deep: color("accent-deep")?,
        danger: color("danger")?,
        danger_deep: color("danger-deep")?,
        ok: color("ok")?,
        selected: color("selected")?,
    })
}

/// `#rrggbb`, the one form a theme file writes.
fn parse_color(text: &str) -> Option<Color> {
    let digits = text.trim().strip_prefix('#')?;
    if digits.len() != 6 {
        return None;
    }
    u32::from_str_radix(digits, 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn whole(mangle: impl Fn(String) -> String) -> String {
        let names = [
            "background", "surface", "raised", "pressed", "border", "text", "muted",
            "accent", "accent-deep", "danger", "danger-deep", "ok", "selected",
        ];
        let body: String = names
            .iter()
            .enumerate()
            .map(|(at, name)| mangle(format!("  <{name}>#{:06x}</{name}>\n", at + 1)))
            .collect();
        format!("<theme>\n{body}</theme>")
    }

    #[test]
    fn parses_a_whole_theme() {
        let theme = parse(&whole(|line| line)).unwrap();
        assert_eq!(theme.background, 0x000001);
        assert_eq!(theme.selected, 0x00000d);
    }

    #[test]
    fn refuses_what_is_not_a_theme() {
        assert!(parse("<settings/>").is_none());
        assert!(parse("not xml at all").is_none());
        assert!(parse("<theme><background>").is_none());
    }

    #[test]
    fn refuses_a_theme_with_a_hole() {
        // A missing colour and a mangled one are the same refusal: there is
        // no palette in the code to patch a file with.
        let missing = whole(|line| if line.contains("<accent>") { String::new() } else { line });
        assert!(parse(&missing).is_none());
        let mangled = whole(|line| line.replace("#000004", "chartreuse"));
        assert!(parse(&mangled).is_none());
    }
}
