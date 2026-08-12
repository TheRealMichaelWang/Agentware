//! The pointers.
//!
//! Two of them, and they must never be confusable. The human's is the one they
//! are moving; the agent's is the "fake cursor" VISION.md promises, which the
//! haimanager drives to whatever node an agent is acting on so its actions are
//! observable rather than instantaneous.
//!
//! The arrow is a polygon rasterized with supersampling, not a hand-drawn span
//! table. The first version was a span table, and it looked like one: a
//! staircase hypotenuse and a tail that read as a rendering error. A cursor is
//! the single most looked-at thing on a screen, so it is the last place to save
//! antialiasing.

use std::sync::OnceLock;

use crate::paint::{Canvas, Color, Rect, rgb};

const HUMAN: Color = rgb(0xf2, 0xf4, 0xf8);
const AGENT: Color = rgb(0x4f, 0x9c, 0xf5);
const OUTLINE: Color = rgb(0x14, 0x16, 0x1c);

/// The classic arrow, as a polygon with the hotspot at the origin: a vertical
/// left edge, a diagonal hypotenuse, and a tail. The exact shape every desktop
/// has trained every eye to expect.
const ARROW: [(f32, f32); 7] = [
    (0.0, 0.0),
    (0.0, 14.8),
    (3.6, 11.6),
    (6.3, 17.6),
    (9.1, 16.4),
    (6.4, 10.8),
    (11.4, 10.8),
];

const MASK_W: usize = 13;
const MASK_H: usize = 19;
/// Subsamples per axis. Sixteen looks identical to four here; four is enough.
const SUB: i32 = 4;

/// Per-pixel coverage of the arrow, computed once.
fn mask() -> &'static [u8; MASK_W * MASK_H] {
    static MASK: OnceLock<[u8; MASK_W * MASK_H]> = OnceLock::new();
    MASK.get_or_init(|| {
        let mut out = [0u8; MASK_W * MASK_H];
        for (index, coverage) in out.iter_mut().enumerate() {
            let (px, py) = ((index % MASK_W) as f32, (index / MASK_W) as f32);
            let mut hits = 0;
            for sy in 0..SUB {
                for sx in 0..SUB {
                    let x = px + (sx as f32 + 0.5) / SUB as f32;
                    let y = py + (sy as f32 + 0.5) / SUB as f32;
                    if inside(x, y) {
                        hits += 1;
                    }
                }
            }
            *coverage = (hits * 255 / (SUB * SUB)) as u8;
        }
        out
    })
}

/// Even-odd point-in-polygon test against the arrow.
fn inside(x: f32, y: f32) -> bool {
    let mut winding = false;
    let count = ARROW.len();
    for at in 0..count {
        let (x1, y1) = ARROW[at];
        let (x2, y2) = ARROW[(at + 1) % count];
        if (y1 > y) != (y2 > y) {
            let cross = x1 + (y - y1) / (y2 - y1) * (x2 - x1);
            if x < cross {
                winding = !winding;
            }
        }
    }
    winding
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Human,
    Agent,
}

pub fn draw(canvas: &mut Canvas, x: i32, y: i32, kind: Kind) {
    let fill = match kind {
        Kind::Human => HUMAN,
        Kind::Agent => AGENT,
    };
    let mask = mask();

    // The agent's pointer carries a halo, so a still screenshot of an agent
    // mid-action is unambiguous even in greyscale.
    if kind == Kind::Agent {
        canvas.stroke_round_rect(Rect::new(x - 7, y - 7, 27, 29), 13, 1, AGENT);
    }

    // Outline first: the fill's own mask stamped at the eight neighbouring
    // offsets, which grows the shape by one antialiased pixel all round and
    // keeps the pointer visible against a background of its own colour.
    for (dx, dy) in [(-1, -1), (0, -1), (1, -1), (-1, 0), (1, 0), (-1, 1), (0, 1), (1, 1)] {
        stamp(canvas, mask, x + dx, y + dy, OUTLINE);
    }
    stamp(canvas, mask, x, y, fill);
}

fn stamp(canvas: &mut Canvas, mask: &[u8; MASK_W * MASK_H], x: i32, y: i32, color: Color) {
    for row in 0..MASK_H {
        for column in 0..MASK_W {
            let coverage = mask[row * MASK_W + column];
            if coverage > 0 {
                canvas.blend_px(x + column as i32, y + row as i32, color, coverage);
            }
        }
    }
}
