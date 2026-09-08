//! The `slider` element: one number chosen along a track.
//!
//! Everything about a slider that is arithmetic rather than plumbing lives
//! here: where the thumb sits for a value, what value a pointer at some x
//! means, and how it is drawn. `ui` asks it to paint and `client` asks it to
//! translate a press, so neither of those files grows a second geometry of
//! its own to disagree with this one.
//!
//! ## What an agent sees
//!
//! A number, and the range it lies in. The action is `set-value`, which is
//! already in the closed vocabulary and already means "the number for
//! set-value", so a slider needs no new verb: an agent says what the value
//! should be and never where a thumb should sit. That is the same division
//! every other element makes, and it is why an agent never has to know that
//! this control is a track with a knob on it rather than a field.
//!
//! A human drags the thumb, and the drag is turned into the same `set-value`
//! the agent sends, through the one `Client::act` both paths end in.
//!
//! ## Why the value is the application's
//!
//! Like a field's text. The compositor moves the thumb while a drag is in
//! progress so the control answers the hand immediately, and the application
//! is sent the value it should now hold; the next tree it renders is what the
//! thumb finally agrees with. An application that refuses a value simply
//! renders the old one back and the thumb returns, which is the behaviour a
//! disabled range wants and needs no separate mechanism.

use crate::awml::Node;
use crate::paint::{Canvas, Rect};
use crate::ui;

/// The span a slider runs over, as its attributes declare it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Range {
    pub min: f32,
    pub max: f32,
    /// The granularity. Every value the control produces is a whole number of
    /// steps above `min`, so a slider of milliseconds can be made to move in
    /// tens and never emit 437.
    pub step: f32,
}

impl Default for Range {
    fn default() -> Range {
        Range { min: 0.0, max: 100.0, step: 1.0 }
    }
}

impl Range {
    /// The range an element declares, with anything missing or nonsensical
    /// falling back rather than refusing to draw.
    ///
    /// A backwards range is put in order for the same reason the pace's is:
    /// it is a typo in a document, and a control that will not appear is a
    /// worse answer than one that works.
    pub fn of(node: &Node) -> Range {
        let number = |name: &str| node.attr(name).and_then(|text| text.trim().parse::<f32>().ok());
        let fallback = Range::default();
        let (min, max) = match (number("min"), number("max")) {
            (Some(min), Some(max)) if min > max => (max, min),
            (min, max) => (min.unwrap_or(fallback.min), max.unwrap_or(fallback.max)),
        };
        // A step of zero or less would make every value a division by nothing.
        let step = number("step").filter(|step| *step > 0.0).unwrap_or(fallback.step);
        Range { min, max, step }
    }

    /// `value` held inside the range and rounded to the nearest step.
    pub fn clamp(&self, value: f32) -> f32 {
        let held = value.clamp(self.min, self.max);
        let steps = ((held - self.min) / self.step).round();
        (self.min + steps * self.step).clamp(self.min, self.max)
    }

    /// Where `value` sits along the track, from 0 at the left to 1 at the
    /// right. A range of no width is all the way along, not a crash.
    pub fn fraction(&self, value: f32) -> f32 {
        let span = self.max - self.min;
        if span <= 0.0 {
            return 1.0;
        }
        ((value - self.min) / span).clamp(0.0, 1.0)
    }
}

/// The value an element is currently showing.
pub fn value_of(node: &Node, range: Range) -> f32 {
    let value = node
        .attr("value")
        .and_then(|text| text.trim().parse::<f32>().ok())
        .unwrap_or(range.min);
    range.clamp(value)
}

/// The value as it travels in an event: a whole number when the range is
/// whole, so a slider of milliseconds sends `240` rather than `240.0`.
pub fn format(value: f32, range: Range) -> String {
    let whole = range.step.fract() == 0.0 && range.min.fract() == 0.0;
    if whole { format!("{}", value.round() as i64) } else { format!("{value}") }
}

/// How wide the thumb is. The one number the rest of the geometry is built
/// from, so the track and the travel cannot disagree about where it stops.
pub fn thumb_size() -> i32 {
    ui::sc(16)
}

/// The height a slider asks for: enough for the thumb, and no more.
pub fn natural_height() -> i32 {
    thumb_size()
}

/// The width a slider asks for when nothing else decides. A track is only
/// useful if there is room to move along it.
pub fn natural_width() -> i32 {
    ui::sc(180)
}

/// The part of the box the thumb's centre can occupy, inset by its own
/// radius at each end so it never hangs off the track.
fn travel(rect: Rect) -> (i32, i32) {
    let radius = thumb_size() / 2;
    (rect.x + radius, rect.x + rect.w - radius)
}

/// Where the thumb sits for `value`.
pub fn thumb(rect: Rect, value: f32, range: Range) -> Rect {
    let (left, right) = travel(rect);
    let centre = left + ((right - left) as f32 * range.fraction(value)).round() as i32;
    let size = thumb_size();
    Rect::new(centre - size / 2, rect.y + (rect.h - size) / 2, size, size)
}

/// What value the pointer at `x` is asking for.
///
/// The inverse of [`thumb`], so a thumb dragged to a place and released names
/// the value that redraws it in that same place. Anywhere left of the track
/// is the minimum and anywhere right of it the maximum, because a hand that
/// overshoots the end of a slider means the end of it.
pub fn value_at(rect: Rect, range: Range, x: i32) -> f32 {
    let (left, right) = travel(rect);
    let span = (right - left).max(1) as f32;
    range.clamp(range.min + (range.max - range.min) * ((x - left) as f32 / span).clamp(0.0, 1.0))
}

/// Draw the track, the part of it that is filled, and the thumb.
pub fn draw(canvas: &mut Canvas, rect: Rect, value: f32, range: Range, focused: bool, disabled: bool) {
    let groove = ui::sc(5).max(3);
    let (left, right) = travel(rect);
    let track = Rect::new(left, rect.y + (rect.h - groove) / 2, (right - left).max(1), groove);
    let radius = groove / 2;

    canvas.fill_round_rect(track, radius, ui::background());
    canvas.stroke_round_rect(track, radius, 1, ui::border());

    // The travelled part, so the value is readable without reading a number.
    let thumb = thumb(rect, value, range);
    let filled = (thumb.x + thumb.w / 2 - track.x).max(0).min(track.w);
    if filled > 0 {
        let ink = if disabled { ui::muted() } else { ui::accent() };
        canvas.fill_round_rect(Rect::new(track.x, track.y, filled, track.h), radius, ink);
    }

    let face = if disabled { ui::muted() } else { ui::accent() };
    canvas.fill_round_rect(thumb, thumb.w / 2, face);
    // The focus ring goes round the thumb rather than the whole control: the
    // thumb is what the keyboard would move.
    if focused && !disabled {
        canvas.stroke_round_rect(thumb.inset(-2), thumb.w / 2 + 2, sc_ring(), ui::accent());
    }
}

fn sc_ring() -> i32 {
    ui::sc(2).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range() -> Range {
        Range { min: 0.0, max: 100.0, step: 1.0 }
    }

    /// A bare node carrying attributes, which is all this module reads.
    fn element(attrs: &[(&str, &str)]) -> Node {
        Node {
            tag: crate::awml::Tag::Slider,
            attrs: attrs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect(),
            text: String::new(),
            children: Vec::new(),
            parent: None,
        }
    }

    fn box_of() -> Rect {
        Rect::new(100, 50, 200 + thumb_size(), thumb_size())
    }

    #[test]
    fn a_value_and_its_position_are_inverses() {
        // The property that matters: a thumb dragged somewhere and released
        // names the value that redraws it in the same place. Anything else
        // and a slider creeps as it is used.
        let rect = box_of();
        for step in 0..=100 {
            let value = step as f32;
            let thumb = thumb(rect, value, range());
            let centre = thumb.x + thumb.w / 2;
            assert_eq!(value_at(rect, range(), centre), value, "value {value} moved");
        }
    }

    #[test]
    fn the_ends_are_reachable_and_nothing_hangs_off_the_track() {
        let rect = box_of();
        let low = thumb(rect, 0.0, range());
        let high = thumb(rect, 100.0, range());
        assert_eq!(low.x, rect.x, "the minimum did not reach the left end");
        assert_eq!(high.x + high.w, rect.x + rect.w, "the maximum overshot the right end");

        // A hand that overshoots the end of a slider means the end of it.
        assert_eq!(value_at(rect, range(), rect.x - 500), 0.0);
        assert_eq!(value_at(rect, range(), rect.x + rect.w + 500), 100.0);
    }

    #[test]
    fn values_land_on_steps() {
        // A slider of milliseconds made to move in tens must never emit 437.
        let tens = Range { min: 0.0, max: 1000.0, step: 10.0 };
        let rect = box_of();
        for x in rect.x..rect.x + rect.w {
            let value = value_at(rect, tens, x);
            assert_eq!(value % 10.0, 0.0, "{value} is not a whole step");
            assert!((tens.min..=tens.max).contains(&value));
        }
    }

    #[test]
    fn a_backwards_or_missing_range_still_works() {
        // A typo in a document must not produce a control that cannot be
        // drawn: every other malformed attribute falls back too.
        let node = element(&[("min", "600"), ("max", "40")]);
        assert_eq!(Range::of(&node), Range { min: 40.0, max: 600.0, step: 1.0 });

        let bare = element(&[]);
        assert_eq!(Range::of(&bare), Range::default());

        // A step of zero would divide by nothing.
        let zero = element(&[("step", "0")]);
        assert_eq!(Range::of(&zero).step, 1.0);

        // And a range of no width is all the way along rather than a crash.
        let flat = Range { min: 5.0, max: 5.0, step: 1.0 };
        assert_eq!(flat.fraction(5.0), 1.0);
    }

    #[test]
    fn a_value_outside_the_range_is_held_inside_it() {
        let node = element(&[("min", "40"), ("max", "600"), ("value", "9000")]);
        let range = Range::of(&node);
        assert_eq!(value_of(&node, range), 600.0);

        let low = element(&[("min", "40"), ("max", "600"), ("value", "-3")]);
        assert_eq!(value_of(&low, Range::of(&low)), 40.0);

        // And a missing value is the minimum rather than zero, which for a
        // range that does not start at zero are different answers.
        let none = element(&[("min", "40"), ("max", "600")]);
        assert_eq!(value_of(&none, Range::of(&none)), 40.0);
    }

    #[test]
    fn whole_ranges_send_whole_numbers() {
        // The event carries text, and an application parsing milliseconds
        // should not have to cope with "240.0".
        let whole = Range { min: 0.0, max: 1000.0, step: 10.0 };
        assert_eq!(format(240.0, whole), "240");
        let fine = Range { min: 0.0, max: 1.0, step: 0.25 };
        assert_eq!(format(0.25, fine), "0.25");
    }
}
