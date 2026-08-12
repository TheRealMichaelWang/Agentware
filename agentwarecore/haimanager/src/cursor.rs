//! The pointers.
//!
//! Two of them, and they must never be confusable. The human's is the one they
//! are moving; the agent's is the "fake cursor" VISION.md promises, which the
//! haimanager drives to whatever node an agent is acting on so its actions are
//! observable rather than instantaneous.
//!
//! They are drawn in different colours and different shapes on purpose. If a
//! human cannot tell at a glance which pointer just clicked something, the
//! visible embodiment is decorative rather than informative.

use crate::paint::{Canvas, Color, Rect, rgb};

const HUMAN: Color = rgb(0xff, 0xff, 0xff);
const AGENT: Color = rgb(0x4f, 0x9c, 0xf5);
const OUTLINE: Color = rgb(0x10, 0x12, 0x18);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Human,
    Agent,
}

/// An arrow, as a run-length mask: one entry per row, giving the first lit
/// column and how many are lit.
///
/// A bitmap would be tidier, but the outline has to be derived from the shape,
/// and spans make that a matter of widening each row rather than a second
/// hand-drawn mask that could disagree with the first.
const ARROW: [(i32, i32); 16] = [
    (0, 1),
    (0, 2),
    (0, 3),
    (0, 4),
    (0, 5),
    (0, 6),
    (0, 7),
    (0, 8),
    (0, 9),
    (0, 10),
    (0, 6),
    (0, 3),
    (2, 4),
    (3, 4),
    (4, 3),
    (5, 2),
];

pub fn draw(canvas: &mut Canvas, x: i32, y: i32, kind: Kind) {
    let fill = match kind {
        Kind::Human => HUMAN,
        Kind::Agent => AGENT,
    };

    // Outline first, as the shape grown by one pixel in every direction, so the
    // pointer stays visible against a background of its own colour.
    for (row, &(start, len)) in ARROW.iter().enumerate() {
        let row = row as i32;
        canvas.fill_rect(Rect::new(x + start - 1, y + row - 1, len + 2, 3), OUTLINE);
    }

    for (row, &(start, len)) in ARROW.iter().enumerate() {
        canvas.fill_rect(Rect::new(x + start, y + row as i32, len, 1), fill);
    }

    // The agent's pointer carries a ring, so a still screenshot of an agent
    // mid-action is unambiguous even in greyscale.
    if kind == Kind::Agent {
        canvas.stroke_rect(Rect::new(x - 5, y - 5, 24, 26), 1, AGENT);
    }
}
