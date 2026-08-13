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

/// Subsamples per axis. Sixteen looks identical to four here; four is enough.
const SUB: i32 = 4;

struct Mask {
    w: usize,
    h: usize,
    data: Vec<u8>,
}

/// Rasterize a shape given in logical coordinates, at the interface scale.
///
/// The test function is asked in logical space and the sampling happens in
/// pixels, so a scaled cursor is the shape drawn larger, not a small mask
/// blown up.
fn build(logical_w: f32, logical_h: f32, inside: impl Fn(f32, f32) -> bool) -> Mask {
    let s = crate::ui::scale();
    let w = (logical_w * s).ceil() as usize;
    let h = (logical_h * s).ceil() as usize;
    let mut data = vec![0u8; w * h];
    for (index, coverage) in data.iter_mut().enumerate() {
        let (px, py) = ((index % w) as f32, (index / w) as f32);
        let mut hits = 0;
        for sy in 0..SUB {
            for sx in 0..SUB {
                let x = px + (sx as f32 + 0.5) / SUB as f32;
                let y = py + (sy as f32 + 0.5) / SUB as f32;
                if inside(x / s, y / s) {
                    hits += 1;
                }
            }
        }
        *coverage = (hits * 255 / (SUB * SUB)) as u8;
    }
    Mask { w, h, data }
}

fn arrow_mask() -> &'static Mask {
    static MASK: OnceLock<Mask> = OnceLock::new();
    MASK.get_or_init(|| build(13.0, 19.0, arrow_inside))
}

/// The I-beam: a thin stem with serifs, the shape every text field has taught
/// every hand to expect. Axis-aligned, so the inside test is arithmetic rather
/// than a polygon walk.
fn beam_mask() -> &'static Mask {
    static MASK: OnceLock<Mask> = OnceLock::new();
    MASK.get_or_init(|| {
        build(7.0, 16.0, |x, y| {
            let stem = (x - 3.5).abs() <= 0.8;
            let serif = !(2.0..14.0).contains(&y) && (0.5..6.5).contains(&x);
            stem || serif
        })
    })
}

/// Even-odd point-in-polygon test against the arrow.
fn arrow_inside(x: f32, y: f32) -> bool {
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

/// What the pointer is shaped like, decided by what it is over.
///
/// The shape is the compositor's promise about what a click will do here: an
/// arrow acts, a beam places a caret. Applications do not choose it, for the
/// same reason they do not choose their actions: the promise has to be true.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    Arrow,
    /// Over editable text.
    Beam,
}

pub fn draw(canvas: &mut Canvas, x: i32, y: i32, kind: Kind, shape: Shape) {
    let fill = match kind {
        Kind::Human => HUMAN,
        Kind::Agent => AGENT,
    };
    // The arrow's hotspot is its tip at the top left; the beam's is its middle,
    // because it marks where the caret would land rather than pointing at it.
    let (mask, x, y) = match shape {
        Shape::Arrow => (arrow_mask(), x, y),
        Shape::Beam => {
            let mask = beam_mask();
            (mask, x - mask.w as i32 / 2, y - mask.h as i32 / 2)
        }
    };

    // The agent's pointer carries a halo, so a still screenshot of an agent
    // mid-action is unambiguous even in greyscale.
    if kind == Kind::Agent {
        let r = crate::ui::sc(13);
        canvas.stroke_round_rect(
            Rect::new(x - r / 2, y - r / 2, 2 * r + 1, 2 * r + 3),
            r,
            1,
            AGENT,
        );
    }

    // Outline first: the fill's own mask stamped at the eight neighbouring
    // offsets, which grows the shape by one antialiased pixel all round and
    // keeps the pointer visible against a background of its own colour.
    for (dx, dy) in [(-1, -1), (0, -1), (1, -1), (-1, 0), (1, 0), (-1, 1), (0, 1), (1, 1)] {
        stamp(canvas, mask, x + dx, y + dy, OUTLINE);
    }
    stamp(canvas, mask, x, y, fill);
}

fn stamp(canvas: &mut Canvas, mask: &Mask, x: i32, y: i32, color: Color) {
    for row in 0..mask.h {
        for column in 0..mask.w {
            let coverage = mask.data[row * mask.w + column];
            if coverage > 0 {
                canvas.blend_px(x + column as i32, y + row as i32, color, coverage);
            }
        }
    }
}
