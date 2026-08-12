//! A specimen sheet for checking the rasterizer by eye.
//!
//! Every primitive appears at least once, and the font appears in full, because
//! a font with one wrong glyph looks fine until the day that letter matters.
//! This exists only until real clients are drawing; it is the rendering
//! equivalent of the supervisor's boot report.

use crate::paint::{Canvas, Color, Rect, rgb};

const BACKGROUND: Color = rgb(0x10, 0x12, 0x18);
const PANEL: Color = rgb(0x1c, 0x20, 0x2a);
const BORDER: Color = rgb(0x3a, 0x42, 0x54);
const TEXT: Color = rgb(0xe6, 0xe9, 0xef);
const MUTED: Color = rgb(0x8a, 0x93, 0xa6);
const ACCENT: Color = rgb(0x4f, 0x9c, 0xf5);
const DANGER: Color = rgb(0xe0, 0x5a, 0x5a);

pub fn draw(canvas: &mut Canvas) {
    canvas.clear(BACKGROUND);

    let margin = 24;
    let mut y = margin;

    y = heading(canvas, "AGENTWARE HAIMANAGER", margin, y);
    y = line(canvas, "software rasterizer specimen", margin, y, MUTED, 2);
    y += 16;

    y = font_sheet(canvas, margin, y);
    y += 20;

    y = scales(canvas, margin, y);
    y += 20;

    y = primitives(canvas, margin, y);
    y += 20;

    clipping(canvas, margin, y);
}

fn heading(canvas: &mut Canvas, text: &str, x: i32, y: i32) -> i32 {
    canvas.draw_text(text, x, y, 4, TEXT);
    y + Canvas::text_height(4) + 8
}

fn line(canvas: &mut Canvas, text: &str, x: i32, y: i32, color: Color, scale: i32) -> i32 {
    canvas.draw_text(text, x, y, scale, color);
    y + Canvas::text_height(scale) + 6
}

/// Every glyph in the font, so a broken one is visible immediately rather than
/// the first time some label happens to use it.
fn font_sheet(canvas: &mut Canvas, x: i32, y: i32) -> i32 {
    let mut y = line(canvas, "FULL CHARACTER SET", x, y, ACCENT, 2);

    let all: String = (0x20u8..=0x7Eu8).map(|c| c as char).collect();
    for chunk in all.as_bytes().chunks(48) {
        let row: String = chunk.iter().map(|&c| c as char).collect();
        canvas.draw_text(&row, x, y, 3, TEXT);
        y += Canvas::text_height(3) + 6;
    }
    y
}

/// The same string at several scales. Integer scaling should keep every stroke
/// exactly one source pixel wide; blurring or dropped rows show up here.
fn scales(canvas: &mut Canvas, x: i32, y: i32) -> i32 {
    let mut y = line(canvas, "SCALES", x, y, ACCENT, 2);

    for scale in [1, 2, 3, 4] {
        canvas.draw_text(&format!("{scale}x  Sphinx of black quartz, judge my vow"), x, y, scale, TEXT);
        y += Canvas::text_height(scale) + 8;
    }
    y
}

/// Fills and outlines, including a control-like arrangement, so the pieces a
/// real interface is made of are all exercised.
fn primitives(canvas: &mut Canvas, x: i32, y: i32) -> i32 {
    let y = line(canvas, "PRIMITIVES", x, y, ACCENT, 2);

    let panel = Rect::new(x, y, 520, 96);
    canvas.fill_rect(panel, PANEL);
    canvas.stroke_rect(panel, 1, BORDER);

    button(canvas, Rect::new(x + 16, y + 16, 120, 32), "Send", ACCENT, TEXT);
    button(canvas, Rect::new(x + 152, y + 16, 120, 32), "Discard", DANGER, TEXT);
    // A disabled control: drawn from the same parts, distinguished only by
    // colour, exactly as a real `disabled` button will be.
    button(canvas, Rect::new(x + 288, y + 16, 120, 32), "Locked", PANEL, MUTED);

    canvas.draw_text("one pixel border, two pixel border:", x + 16, y + 60, 2, MUTED);
    canvas.stroke_rect(Rect::new(x + 380, y + 56, 50, 24), 1, TEXT);
    canvas.stroke_rect(Rect::new(x + 444, y + 56, 50, 24), 2, TEXT);

    y + panel.h + 8
}

fn button(canvas: &mut Canvas, rect: Rect, label: &str, fill: Color, text: Color) {
    canvas.fill_rect(rect, fill);
    canvas.stroke_rect(rect, 1, BORDER);

    let scale = 2;
    let tx = rect.x + (rect.w - Canvas::text_width(label, scale)) / 2;
    let ty = rect.y + (rect.h - Canvas::text_height(scale)) / 2;
    canvas.draw_text(label, tx, ty, scale, text);
}

/// Text deliberately drawn past the edge of its region.
///
/// If clipping works the sentence is cut mid-glyph at the boundary. If it does
/// not, it runs across the rest of the screen, which is the failure that would
/// otherwise show up much later as one app drawing over another.
fn clipping(canvas: &mut Canvas, x: i32, y: i32) {
    let y = line(canvas, "CLIPPING", x, y, ACCENT, 2);

    let window = Rect::new(x, y, 300, 40);
    canvas.fill_rect(window, PANEL);
    canvas.stroke_rect(window, 1, BORDER);

    canvas.clipped(window.inset(1), |inner| {
        inner.draw_text(
            "this sentence is far too long for the box it is in",
            x + 8,
            y + 14,
            2,
            TEXT,
        );
    });
}
