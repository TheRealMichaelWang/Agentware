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

/// The four corner boxes of a rounded rectangle, each with the centre of the
/// circle that rounds it.
fn corners(rect: Rect, r: i32) -> [((i32, i32), (f32, f32)); 4] {
    let (l, t) = (rect.x, rect.y);
    let (right, bottom) = (rect.x + rect.w - r, rect.y + rect.h - r);
    [
        ((l, t), ((l + r) as f32 - 0.5, (t + r) as f32 - 0.5)),
        ((right, t), (right as f32 - 0.5, (t + r) as f32 - 0.5)),
        ((l, bottom), ((l + r) as f32 - 0.5, bottom as f32 - 0.5)),
        ((right, bottom), (right as f32 - 0.5, bottom as f32 - 0.5)),
    ]
}

/// How much of a pixel is inside a circle, as a coverage value.
fn disc_coverage(px: i32, py: i32, cx: f32, cy: f32, radius: f32) -> u8 {
    if radius <= 0.0 {
        return 0;
    }
    let dx = px as f32 - cx;
    let dy = py as f32 - cy;
    let distance = (dx * dx + dy * dy).sqrt();
    // Half a pixel of feathering either side of the edge. Enough to remove the
    // staircase, little enough that the shape does not look soft.
    let coverage = (radius - distance + 0.5).clamp(0.0, 1.0);
    (coverage * 255.0) as u8
}

/// Signed distance from a point to a rounded rectangle. Negative inside.
fn round_rect_distance(px: f32, py: f32, rect: Rect, radius: f32) -> f32 {
    let cx = rect.x as f32 + rect.w as f32 / 2.0;
    let cy = rect.y as f32 + rect.h as f32 / 2.0;
    let bx = (rect.w as f32 / 2.0 - radius).max(0.0);
    let by = (rect.h as f32 / 2.0 - radius).max(0.0);

    let qx = (px - cx).abs() - bx;
    let qy = (py - cy).abs() - by;
    let outside = ((qx.max(0.0)).powi(2) + (qy.max(0.0)).powi(2)).sqrt();
    outside + qx.max(qy).min(0.0) - radius
}

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

    /// Blend one pixel, weighted by coverage.
    ///
    /// Antialiased text is the reason this exists: a glyph edge is partly
    /// covered, and plotting it as either on or off is what makes bitmap fonts
    /// look like bitmap fonts.
    fn blend(&mut self, x: i32, y: i32, color: Color, coverage: u8) {
        if coverage == 0 || !self.clip.contains(x, y) {
            return;
        }

        let index = (y * self.width + x) as usize;
        if coverage == 255 {
            self.pixels[index] = color;
            return;
        }

        let under = self.pixels[index];
        let alpha = coverage as u32;
        let mix = |shift: u32| {
            let a = (under >> shift) & 0xff;
            let b = (color >> shift) & 0xff;
            // Rounded rather than truncated, so a run of blends does not drift
            // steadily darker than it should.
            ((a * (255 - alpha) + b * alpha + 127) / 255) << shift
        };
        self.pixels[index] = mix(16) | mix(8) | mix(0);
    }

    /// Draw a line of text with its *top* at `y`, returning the x just past it.
    ///
    /// Taking the top rather than the baseline means callers place text the way
    /// they place everything else, and only this function needs to know where
    /// the baseline sits inside a line box.
    pub fn draw_text(
        &mut self,
        fonts: &font::Fonts,
        text: &str,
        x: i32,
        y: i32,
        style: &font::Style,
        color: Color,
    ) -> i32 {
        let baseline = y as f32 + fonts.line_metrics(style).ascent;
        let embolden = style.weight == font::Weight::Bold && style.family == font::Family::Mono;
        let mut pen = x as f32;

        for character in text.chars() {
            fonts.with_glyph(style, character, |glyph| {
                let origin_x = pen.round() as i32 + glyph.left;
                let origin_y = baseline.round() as i32 - glyph.top;

                for row in 0..glyph.height {
                    // Synthetic obliquing: shift each row by a fraction of its
                    // distance above the baseline. Cheaper than shipping an
                    // italic face and, at interface sizes, hard to tell apart.
                    let slant = if style.italic {
                        ((glyph.height - row) as f32 * 0.21) as i32
                    } else {
                        0
                    };

                    for column in 0..glyph.width {
                        let coverage = glyph.coverage[row * glyph.width + column];
                        let px = origin_x + column as i32 + slant;
                        let py = origin_y + row as i32;
                        self.blend(px, py, color, coverage);
                        if embolden {
                            self.blend(px + 1, py, color, coverage);
                        }
                    }
                }

                pen += glyph.advance + if embolden { 1.0 } else { 0.0 };
            });
        }

        pen.ceil() as i32
    }

    /// A filled rectangle with rounded corners.
    ///
    /// Corners are the reason this exists rather than `fill_rect`: at interface
    /// sizes a hard 90 degree corner is the single thing that makes a surface
    /// read as a box drawn by a program rather than as a panel. They are
    /// antialiased, because a stair-stepped curve is worse than no curve.
    ///
    /// Only the corners cost anything. The straight middle is three ordinary
    /// fills, so a full-window surface is not paying per-pixel for a shape that
    /// is square almost everywhere.
    pub fn fill_round_rect(&mut self, rect: Rect, radius: i32, color: Color) {
        let r = radius.min(rect.w / 2).min(rect.h / 2);
        if r <= 0 {
            self.fill_rect(rect, color);
            return;
        }

        self.fill_rect(Rect::new(rect.x, rect.y + r, rect.w, rect.h - r * 2), color);
        self.fill_rect(Rect::new(rect.x + r, rect.y, rect.w - r * 2, r), color);
        self.fill_rect(
            Rect::new(rect.x + r, rect.y + rect.h - r, rect.w - r * 2, r),
            color,
        );

        for (corner, (cx, cy)) in corners(rect, r) {
            for dy in 0..r {
                for dx in 0..r {
                    let px = corner.0 + dx;
                    let py = corner.1 + dy;
                    let coverage = disc_coverage(px, py, cx, cy, r as f32);
                    self.blend(px, py, color, coverage);
                }
            }
        }
    }

    /// A rounded outline drawn inside `rect`.
    pub fn stroke_round_rect(&mut self, rect: Rect, radius: i32, thickness: i32, color: Color) {
        let r = radius.min(rect.w / 2).min(rect.h / 2);
        if r <= 0 {
            self.stroke_rect(rect, thickness, color);
            return;
        }
        let t = thickness.max(1);

        self.fill_rect(Rect::new(rect.x + r, rect.y, rect.w - r * 2, t), color);
        self.fill_rect(
            Rect::new(rect.x + r, rect.y + rect.h - t, rect.w - r * 2, t),
            color,
        );
        self.fill_rect(Rect::new(rect.x, rect.y + r, t, rect.h - r * 2), color);
        self.fill_rect(
            Rect::new(rect.x + rect.w - t, rect.y + r, t, rect.h - r * 2),
            color,
        );

        for (corner, (cx, cy)) in corners(rect, r) {
            for dy in 0..r {
                for dx in 0..r {
                    let px = corner.0 + dx;
                    let py = corner.1 + dy;
                    let outer = disc_coverage(px, py, cx, cy, r as f32);
                    let inner = disc_coverage(px, py, cx, cy, (r - t) as f32);
                    self.blend(px, py, color, outer.saturating_sub(inner));
                }
            }
        }
    }

    /// A soft shadow cast by a rounded rectangle.
    ///
    /// Depth is what separates a window from the desktop behind it without a
    /// heavy border doing the work, and a heavy border is most of what makes an
    /// interface look blocky.
    ///
    /// Only the band outside the shape is touched. The interior is skipped
    /// entirely because whatever cast the shadow is about to be drawn over it,
    /// so the cost is a perimeter rather than an area.
    pub fn shadow(&mut self, rect: Rect, radius: i32, blur: i32, strength: u8) {
        // Offset downward: a light source above is what every interface assumes,
        // and a shadow centred on its shape reads as a glow instead.
        let cast = Rect::new(rect.x, rect.y + blur / 3, rect.w, rect.h);
        let inside = cast.inset(radius + 1);
        let area = Rect::new(
            cast.x - blur,
            cast.y - blur,
            cast.w + blur * 2,
            cast.h + blur * 2,
        );
        let Some(area) = self.clip.intersect(&area) else { return };

        for y in area.y..area.y + area.h {
            for x in area.x..area.x + area.w {
                if inside.contains(x, y) {
                    continue;
                }
                let distance = round_rect_distance(x as f32, y as f32, cast, radius as f32);
                if distance <= 0.0 || distance >= blur as f32 {
                    continue;
                }
                // Squared falloff, which is closer to how a real penumbra fades
                // than a straight ramp and costs one multiply.
                let fade = 1.0 - distance / blur as f32;
                self.blend(x, y, 0x000000, (strength as f32 * fade * fade) as u8);
            }
        }
    }

    /// The finished frame, row by row, for copying to a scanout buffer.
    pub fn rows(&self) -> impl Iterator<Item = &[Color]> {
        self.pixels.chunks_exact(self.width as usize)
    }
}
