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

/// The range as it is being adjusted, before it is committed.
///
/// Two sliders rather than two fields. A duration is a quantity rather than a
/// name, so the useful question is "a bit longer than that" and the useful
/// answer is a thumb that moves; nobody knows what 340 milliseconds feels
/// like until they have tried it. The numbers are shown beside them for the
/// person who does want to be exact.
///
/// Dragging is not saving. A drag is a run of `set-value` events, one per
/// value the thumb passes, and each save is a synced write to the state
/// volume; committing every one of them would write the file a hundred times
/// on the way to one preference. So the thumb moves freely and Save commits,
/// exactly as the key beside it does.
pub struct Editing {
    pub min: Duration,
    pub max: Duration,
}

impl Editing {
    pub fn new(pace: Pace) -> Editing {
        Editing { min: pace.min, max: pace.max }
    }

    /// Take an event this section owns. `None` if it is not one of ours.
    ///
    /// Answering `None` rather than swallowing anything unrecognised is what
    /// lets the page hand every event here first without this file having to
    /// know what else is on the page.
    pub fn accept(&mut self, target: &str, action: &str, value: &str) -> Option<Answer> {
        match (target, action) {
            (MIN_FIELD, awproto::display::ACTION_SET_VALUE) => {
                self.min = millis(value, self.min);
                Some(Answer::Moved)
            }
            (MAX_FIELD, awproto::display::ACTION_SET_VALUE) => {
                self.max = millis(value, self.max);
                Some(Answer::Moved)
            }
            (SAVE, awproto::display::ACTION_CLICK) => Some(Answer::Commit(self.parsed())),
            _ => None,
        }
    }

    /// The range as the thumbs currently stand.
    ///
    /// `Pace::new` puts the two ends in order and inside the bounds, so a
    /// minimum dragged past the maximum is stored as the range a person
    /// plainly meant rather than refused.
    fn parsed(&self) -> Pace {
        Pace::new(self.min, self.max)
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

        for (id, label, value, what) in [
            (MAX_FIELD, "Slowest", self.max, "when nothing else is waiting"),
            (MIN_FIELD, "Fastest", self.min, "when the cursor is far behind"),
        ] {
            let _ = writeln!(
                out,
                r#"            <text role="caption" color="muted">{label}: {shown}ms, {what}</text>
            <slider id="{id}" value="{value}" min="{floor}" max="{ceiling}" step="10" description="Milliseconds one action takes to show {what}, between {floor} and {ceiling}"/>"#,
                shown = value.as_millis(),
                value = value.as_millis(),
                floor = pace::FLOOR.as_millis(),
                ceiling = pace::CEILING.as_millis(),
            );
        }

        let _ = writeln!(
            out,
            r#"            <hstack gap="sm">
              <button id="{SAVE}" label="Save" emphasis="primary" description="Saves both delays where the thumbs stand"/>
            </hstack>"#,
        );

        // What is stored, said back, because the thumbs show a draft and
        // those are not the same thing until Save.
        let _ = writeln!(
            out,
            r#"            <text role="caption" color="muted">Saved: up to {max}ms for a single action, down towards {min}ms as the agent gets ahead of the cursor.</text>"#,
            max = stored.max.as_millis(),
            min = stored.min.as_millis(),
        );
        out.push_str("          </vstack>\n        </group>\n");
    }
}

/// What taking an event did.
pub enum Answer {
    /// A thumb moved. Re-render, save nothing.
    Moved,
    /// The button: this is the range to store.
    Commit(Pace),
}

/// Milliseconds out of an event's value, falling back rather than to zero.
fn millis(value: &str, fallback: Duration) -> Duration {
    value
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|number| number.is_finite() && *number >= 0.0)
        .map(|number| Duration::from_millis(number.round() as u64))
        .unwrap_or(fallback)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn dragging_does_not_commit() {
        // A drag is a run of set-value events, one per value the thumb
        // passes. Each save is a synced write to the state volume, so
        // committing every one of them would write the file a hundred times
        // on the way to one preference.
        let mut editing = Editing::new(Pace::default());
        for value in ["600", "590", "580", "570"] {
            assert!(matches!(
                editing.accept(MAX_FIELD, "set-value", value),
                Some(Answer::Moved)
            ));
        }
        assert_eq!(editing.max, ms(570));
    }

    #[test]
    fn the_button_commits_where_the_thumbs_stand() {
        let mut editing = Editing::new(Pace::default());
        editing.accept(MIN_FIELD, "set-value", "25");
        editing.accept(MAX_FIELD, "set-value", "900");
        match editing.accept(SAVE, "click", "") {
            Some(Answer::Commit(pace)) => assert_eq!(pace, Pace::new(ms(25), ms(900))),
            _ => panic!("expected a commit"),
        }
    }

    #[test]
    fn a_value_that_is_not_a_number_leaves_the_thumb_alone() {
        // Nothing should be able to make the cursor teleport, whatever
        // arrives on the wire.
        let mut editing = Editing::new(Pace::default());
        let was = editing.max;
        editing.accept(MAX_FIELD, "set-value", "soon");
        editing.accept(MAX_FIELD, "set-value", "-40");
        assert_eq!(editing.max, was);
    }

    #[test]
    fn the_ends_are_put_in_order_rather_than_refused() {
        // A minimum dragged past the maximum is a range a person plainly
        // meant, not a mistake to reject.
        let mut editing = Editing::new(Pace::default());
        editing.accept(MIN_FIELD, "set-value", "900");
        editing.accept(MAX_FIELD, "set-value", "100");
        match editing.accept(SAVE, "click", "") {
            Some(Answer::Commit(pace)) => {
                assert_eq!(pace.min, ms(100));
                assert_eq!(pace.max, ms(900));
            }
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
        assert_eq!(editing.min, Pace::default().min);
    }

    #[test]
    fn the_section_says_what_is_stored_not_what_is_typed() {
        // The fields show a draft; the caption has to show the machine's
        // actual answer or there is no way to tell whether Save worked.
        let mut editing = Editing::new(Pace::default());
        editing.accept(MAX_FIELD, "set-value", "1230");
        let mut out = String::new();
        editing.render(&mut out, Pace::new(ms(40), ms(600)));
        assert!(out.contains(r#"value="1230""#), "the draft is not on the thumb");
        assert!(out.contains("up to 600ms"), "the stored value is not said back");
        // And it is a slider, with the bounds the setting allows.
        assert!(out.contains("<slider"), "not a slider");
        assert!(out.contains(&format!(r#"max="{}""#, pace::CEILING.as_millis())));
    }
}
