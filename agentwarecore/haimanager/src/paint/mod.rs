//! Software rasterizer.
//!
//! Drawing happens into a plain `Vec<u32>` in ordinary memory, and the finished
//! frame is copied to the display in one pass. Drawing straight into the mapped
//! scanout buffer would be tempting and is wrong twice over: that memory is
//! typically write-combined, where the read-modify-write a blend needs is
//! extremely slow, and the display is scanning out of it while it is being
//! drawn, so a half-finished frame is visible.
//!
//! Copying a whole frame costs a few milliseconds at 1280x800 and buys atomic
//! updates without page-flip machinery or vblank event handling.

// The drawing surface is deliberately a complete little API rather than only
// the parts the specimen sheet happens to use. Its real consumer is the layout
// engine, which lands next; this allow should come off when it does.
#![allow(dead_code)]

pub mod font;

/// A colour in `XRGB8888`, matching the scanout buffer: `0x00RRGGBB`.
pub type Color = u32;

pub const fn rgb(r: u8, g: u8, b: u8) -> Color {
    ((r as u32) << 16) | ((g as u32) << 8) | b as u32
}

/// A rectangle in pixels. `x` and `y` are the top-left corner.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }

    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.w && y < self.y + self.h
    }

    /// The overlap of two rectangles, or `None` if they do not touch.
    pub fn intersect(&self, other: &Rect) -> Option<Rect> {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let right = (self.x + self.w).min(other.x + other.w);
        let bottom = (self.y + self.h).min(other.y + other.h);

        if right <= x || bottom <= y {
            return None;
        }
        Some(Rect::new(x, y, right - x, bottom - y))
    }

    pub fn inset(&self, by: i32) -> Rect {
        Rect::new(self.x + by, self.y + by, self.w - by * 2, self.h - by * 2)
    }
}

/// A frame being drawn.
pub struct Canvas {
    pixels: Vec<Color>,
    width: i32,
    height: i32,
    /// Drawing outside this is discarded. Every primitive clips against it, so
    /// a caller can pass wildly out-of-range coordinates without corrupting
    /// memory or wrapping to the far side of the screen.
    clip: Rect,
}

impl Canvas {
    pub fn new(width: usize, height: usize) -> Self {
        Self {
            pixels: vec![0; width * height],
            width: width as i32,
            height: height as i32,
            clip: Rect::new(0, 0, width as i32, height as i32),
        }
    }

    pub fn width(&self) -> i32 {
        self.width
    }

    pub fn height(&self) -> i32 {
        self.height
    }

    pub fn bounds(&self) -> Rect {
        Rect::new(0, 0, self.width, self.height)
    }

    pub fn clip(&self) -> Rect {
        self.clip
    }

    /// Narrow the clip to the overlap with `rect`, run `draw`, and restore it.
    ///
    /// Intersecting rather than replacing is what makes nesting safe: a scroll
    /// region inside a panel cannot draw its way back out of the panel by
    /// setting a larger clip.
    pub fn clipped(&mut self, rect: Rect, draw: impl FnOnce(&mut Canvas)) {
        let Some(inner) = self.clip.intersect(&rect) else {
            return;
        };
        let saved = std::mem::replace(&mut self.clip, inner);
        draw(self);
        self.clip = saved;
    }

    pub fn clear(&mut self, color: Color) {
        self.pixels.fill(color);
    }

    pub fn fill_rect(&mut self, rect: Rect, color: Color) {
        let Some(area) = self.clip.intersect(&rect) else {
            return;
        };

        for y in area.y..area.y + area.h {
            let start = (y * self.width + area.x) as usize;
            self.pixels[start..start + area.w as usize].fill(color);
        }
    }

    /// A rectangle outline drawn inside `rect`, `thickness` pixels wide.
    pub fn stroke_rect(&mut self, rect: Rect, thickness: i32, color: Color) {
        if thickness <= 0 || rect.w <= 0 || rect.h <= 0 {
            return;
        }
        let t = thickness.min(rect.w).min(rect.h);

        self.fill_rect(Rect::new(rect.x, rect.y, rect.w, t), color);
        self.fill_rect(Rect::new(rect.x, rect.y + rect.h - t, rect.w, t), color);
        self.fill_rect(Rect::new(rect.x, rect.y + t, t, rect.h - t * 2), color);
        self.fill_rect(
            Rect::new(rect.x + rect.w - t, rect.y + t, t, rect.h - t * 2),
            color,
        );
    }

    /// Draw one glyph with its top-left corner at `x, y`.
    ///
    /// `scale` multiplies both axes, so each source pixel becomes a solid
    /// `scale` by `scale` block. Integer scaling keeps a bitmap font crisp; any
    /// filtering would turn it to mush at these sizes.
    fn draw_glyph(&mut self, character: char, x: i32, y: i32, scale: i32, color: Color) {
        let bitmap = font::glyph(character);

        for (row, bits) in bitmap.iter().enumerate() {
            for column in 0..font::WIDTH {
                // Bit `WIDTH - 1` is the leftmost pixel, which is what makes
                // the literals in font.rs read as pictures.
                let lit = bits & (1 << (font::WIDTH - 1 - column)) != 0;
                if !lit {
                    continue;
                }
                self.fill_rect(
                    Rect::new(
                        x + column as i32 * scale,
                        y + row as i32 * scale,
                        scale,
                        scale,
                    ),
                    color,
                );
            }
        }
    }

    /// Draw a line of text, returning the x coordinate just past it.
    pub fn draw_text(&mut self, text: &str, x: i32, y: i32, scale: i32, color: Color) -> i32 {
        let advance = (font::WIDTH as i32 + 1) * scale;
        let mut pen = x;

        for character in text.chars() {
            self.draw_glyph(character, pen, y, scale, color);
            pen += advance;
        }
        pen
    }

    /// How wide [`Canvas::draw_text`] would draw this string.
    ///
    /// Includes the gap after the final glyph, matching what `draw_text`
    /// returns, so laying out text by measuring and then drawing agrees with
    /// itself.
    pub fn text_width(text: &str, scale: i32) -> i32 {
        text.chars().count() as i32 * (font::WIDTH as i32 + 1) * scale
    }

    pub fn text_height(scale: i32) -> i32 {
        font::HEIGHT as i32 * scale
    }

    /// The finished frame, row by row, for copying to a scanout buffer.
    pub fn rows(&self) -> impl Iterator<Item = &[Color]> {
        self.pixels.chunks_exact(self.width as usize)
    }
}
