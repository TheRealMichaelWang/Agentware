//! The Computer Use section of the Agent page: how fast the agent's cursor
//! moves.
//!
//! Two numbers, and neither of them makes the machine faster or slower. An
//! agent's actions are performed and answered the instant they arrive; the
//! cursor travelling to a control is a picture of something that has already
//! happened. What these set is how long that picture is drawn for, which
//! decides one thing only: whether a human watching can follow along.
//!
//! So it is a preference rather than a constant. Someone learning what an
//! agent does to their machine wants to see every step; someone who has
//! watched it a hundred times wants it out of the way.
//!
//! A range rather than one number, because the useful pace depends on how far
//! behind the cursor already is. See [`awproto::pace`] for the curve between
//! them; this file is the part a person turns.

use std::fmt::Write as _;
use std::time::Duration;

use awproto::pace::{self, Pace};

/// The ids this section owns. An event naming anything else is not its.
pub const MIN_FIELD: &str = "ui-delay-min";
pub const MAX_FIELD: &str = "ui-delay-max";
pub const SAVE: &str = "ui-delay-save";

/// What the human has typed, before it is committed.
///
/// Held as text rather than as numbers because a half-typed "6" on the way to
/// "600" is not a preference anybody holds, and re-parsing it into the
/// setting on every keystroke would save it sixty times on the way to one
/// value. Committed on Enter or the button, like the key beside it.
pub struct Editing {
    pub min: String,
    pub max: String,
}

impl Editing {
    pub fn new(pace: Pace) -> Editing {
        Editing {
            min: pace.min.as_millis().to_string(),
            max: pace.max.as_millis().to_string(),
        }
    }

    /// Take an event this section owns. `None` if it is not one of ours.
    ///
    /// Answering `None` rather than swallowing anything unrecognised is what
    /// lets the page hand every event here first without this file having to
    /// know what else is on the page.
    pub fn accept(&mut self, target: &str, action: &str, value: &str) -> Option<Answer> {
        match (target, action) {
            (MIN_FIELD, awproto::display::ACTION_TYPE_TEXT) => {
                self.min = value.to_owned();
                Some(Answer::Typed)
            }
            (MAX_FIELD, awproto::display::ACTION_TYPE_TEXT) => {
                self.max = value.to_owned();
                Some(Answer::Typed)
            }
            (MIN_FIELD | MAX_FIELD, awproto::display::ACTION_SUBMIT)
            | (SAVE, awproto::display::ACTION_CLICK) => Some(Answer::Commit(self.parsed())),
            _ => None,
        }
    }

    /// The range as typed, with anything unreadable falling back to what is
    /// stored rather than to zero.
    ///
    /// `Pace::new` puts the two ends in order and inside the bounds, so no
    /// pair of numbers a person can type here produces a cursor that
    /// teleports or one that takes a minute to cross the screen.
    fn parsed(&self) -> Pace {
        let fallback = Pace::default();
        let millis = |text: &str, default: Duration| {
            text.trim()
                .parse::<u64>()
                .map(Duration::from_millis)
                .unwrap_or(default)
        };
        Pace::new(
            millis(&self.min, fallback.min),
            millis(&self.max, fallback.max),
        )
    }

    /// Put the fields back to what is actually stored, after a save or a
    /// change made elsewhere.
    pub fn reset(&mut self, pace: Pace) {
        *self = Editing::new(pace);
    }

    /// The section, as markup.
    pub fn render(&self, out: &mut String, stored: Pace) {
        out.push_str("        <group label=\"Computer Use\">\n          <vstack gap=\"sm\">\n");
        out.push_str(
            "            <text role=\"caption\" color=\"muted\">How long the agent's cursor \
             takes to show one action. It changes nothing about how fast the agent works: \
             the action has already happened by the time the cursor sets off.</text>\n",
        );

        let _ = writeln!(
            out,
            r#"            <hstack gap="sm">
              <field id="{MAX_FIELD}" value="{max}" placeholder="600" description="Milliseconds one action takes to show when nothing else is waiting. The slowest the cursor ever moves"/>
              <field id="{MIN_FIELD}" value="{min}" placeholder="40" description="Milliseconds one action takes to show when the cursor is far behind. The fastest it ever moves"/>
              <button id="{SAVE}" label="Save" description="Saves both delays as typed"/>
            </hstack>"#,
            max = escape(&self.max),
            min = escape(&self.min),
        );
        out.push_str(
            "            <text role=\"caption\" color=\"muted\">Slowest, then fastest, in \
             milliseconds.</text>\n",
        );

        // What is stored, said back, because the fields show what was typed
        // and those are not the same thing until Save.
        let _ = writeln!(
            out,
            r#"            <text role="caption" color="muted">Now: up to {max}ms for a single action, down towards {min}ms as the agent gets ahead of the cursor. Between {floor}ms and {ceiling}ms.</text>"#,
            max = stored.max.as_millis(),
            min = stored.min.as_millis(),
            floor = pace::FLOOR.as_millis(),
            ceiling = pace::CEILING.as_millis(),
        );
        out.push_str("          </vstack>\n        </group>\n");
    }
}

/// What taking an event did.
pub enum Answer {
    /// A keystroke. Re-render, save nothing.
    Typed,
    /// Enter or the button: this is the range to store.
    Commit(Pace),
}

/// The five XML entities, as everything that writes markup escapes them.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn typing_does_not_commit() {
        // Every save is a synced write to the state volume, and half a typed
        // number is not a preference anybody holds.
        let mut editing = Editing::new(Pace::default());
        assert!(matches!(editing.accept(MAX_FIELD, "type-text", "6"), Some(Answer::Typed)));
        assert!(matches!(editing.accept(MAX_FIELD, "type-text", "60"), Some(Answer::Typed)));
        assert_eq!(editing.max, "60");
    }

    #[test]
    fn enter_and_the_button_commit_the_same_thing() {
        let mut editing = Editing::new(Pace::default());
        editing.accept(MIN_FIELD, "type-text", "25");
        editing.accept(MAX_FIELD, "type-text", "900");

        let by_enter = editing.accept(MIN_FIELD, "submit", "");
        let by_button = editing.accept(SAVE, "click", "");
        for answer in [by_enter, by_button] {
            match answer {
                Some(Answer::Commit(pace)) => {
                    assert_eq!(pace, Pace::new(ms(25), ms(900)));
                }
                _ => panic!("expected a commit"),
            }
        }
    }

    #[test]
    fn nonsense_falls_back_rather_than_saving_zero() {
        // A field cleared or typed into wrongly must not become a cursor
        // that teleports; it becomes the default it replaced.
        let mut editing = Editing::new(Pace::default());
        editing.accept(MIN_FIELD, "type-text", "");
        editing.accept(MAX_FIELD, "type-text", "soon");
        match editing.accept(SAVE, "click", "") {
            Some(Answer::Commit(pace)) => assert_eq!(pace, Pace::default()),
            _ => panic!("expected a commit"),
        }
    }

    #[test]
    fn someone_elses_event_is_not_ours() {
        // The page hands every event here first, so this has to be sure
        // about what it does not own.
        let mut editing = Editing::new(Pace::default());
        assert!(editing.accept("api-key", "type-text", "sk-ant-x").is_none());
        assert!(editing.accept("api-key-save", "click", "").is_none());
        assert_eq!(editing.min, Pace::default().min.as_millis().to_string());
    }

    #[test]
    fn the_section_says_what_is_stored_not_what_is_typed() {
        // The fields show a draft; the caption has to show the machine's
        // actual answer or there is no way to tell whether Save worked.
        let mut editing = Editing::new(Pace::default());
        editing.accept(MAX_FIELD, "type-text", "1234");
        let mut out = String::new();
        editing.render(&mut out, Pace::new(ms(40), ms(600)));
        assert!(out.contains(r#"value="1234""#), "the draft is not in the field");
        assert!(out.contains("up to 600ms"), "the stored value is not said back");
    }
}
