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

/// One shadow corner quadrant at full quality, cached per configuration.
///
/// The tile is the top-left corner of a canonical rounded rectangle, computed
/// with the same signed-distance falloff the per-pixel version used, so the
/// cached shadow is pixel-identical to the one it replaced.
fn corner_tile(radius: i32, blur: i32, strength: u8) -> std::sync::Arc<Vec<u8>> {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};
    type TileCache = Mutex<HashMap<(i32, i32, u8), Arc<Vec<u8>>>>;
    static TILES: OnceLock<TileCache> = OnceLock::new();

    let tiles = TILES.get_or_init(|| Mutex::new(HashMap::new()));
    let mut tiles = tiles.lock().unwrap();
    if let Some(tile) = tiles.get(&(radius, blur, strength)) {
        return tile.clone();
    }

    let side = (radius + blur * 2) as usize;
    // A canonical rectangle whose top-left corner the tile covers: its corner
    // sits at (blur, blur) and it is large enough that no other edge is felt.
    let canon = Rect::new(blur, blur, (radius + blur) * 8, (radius + blur) * 8);
    let mut tile = vec![0u8; side * side];
    for (index, alpha) in tile.iter_mut().enumerate() {
        let (x, y) = ((index % side) as f32, (index / side) as f32);
        let distance = round_rect_distance(x, y, canon, radius as f32);
        if distance > 0.0 && distance < blur as f32 {
            let fade = 1.0 - distance / blur as f32;
            *alpha = (strength as f32 * fade * fade) as u8;
        }
    }
    let tile = Arc::new(tile);
    tiles.insert((radius, blur, strength), tile.clone());
    tile
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

/// The same colour as tiny-skia holds one, with an alpha.
fn skia_color(color: Color, alpha: u8) -> tiny_skia::Color {
    tiny_skia::Color::from_rgba8(
        ((color >> 16) & 0xff) as u8,
        ((color >> 8) & 0xff) as u8,
        (color & 0xff) as u8,
        alpha,
    )
}

/// The colour `numerator/denominator` of the way from `a` to `b`.
fn lerp_color(a: Color, b: Color, numerator: i32, denominator: i32) -> Color {
    let mix = |shift: u32| {
        let from = ((a >> shift) & 0xff) as i32;
        let to = ((b >> shift) & 0xff) as i32;
        ((from + (to - from) * numerator / denominator.max(1)) as u32) << shift
    };
    mix(16) | mix(8) | mix(0)
}

/// A rounded rectangle as a tiny-skia path.
fn round_rect_path(rect: Rect, radius: f32) -> Option<tiny_skia::Path> {
    let (x, y, w, h) = (rect.x as f32, rect.y as f32, rect.w as f32, rect.h as f32);
    if radius <= 0.0 {
        return tiny_skia::PathBuilder::from_rect(tiny_skia::Rect::from_xywh(x, y, w, h)?).into();
    }
    let r = radius.min(w / 2.0).min(h / 2.0);
    // Circular corners from cubic curves, with the constant every 2D library
    // uses for a quarter arc.
    let k = r * 0.552_285;
    let mut path = tiny_skia::PathBuilder::new();
    path.move_to(x + r, y);
    path.line_to(x + w - r, y);
    path.cubic_to(x + w - r + k, y, x + w, y + r - k, x + w, y + r);
    path.line_to(x + w, y + h - r);
    path.cubic_to(x + w, y + h - r + k, x + w - r + k, y + h, x + w - r, y + h);
    path.line_to(x + r, y + h);
    path.cubic_to(x + r - k, y + h, x, y + h - r + k, x, y + h - r);
    path.line_to(x, y + r);
    path.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
    path.close();
    path.finish()
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
/// Opaque pixels, ready to be copied onto a canvas row by row.
///
/// What an image becomes once it has been fitted and composited: no alpha, no
/// stride surprises, XRGB in the canvas's own format, so drawing it is a copy
/// rather than a blend.
pub struct Bitmap {
    pub width: i32,
    pub height: i32,
    pub pixels: Vec<Color>,
}

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
    pub fn blend_px(&mut self, x: i32, y: i32, color: Color, coverage: u8) {
        self.blend(x, y, color, coverage);
    }

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
    /// heavy border doing the work. It is also drawn on every scene repaint of
    /// every window, which is why none of it computes geometry per pixel any
    /// more: the straight edges use one alpha per row or column, and the
    /// corners come from a tile rasterized once per (radius, blur, strength)
    /// and cached. The measured cost of the original, a square root per pixel
    /// over the whole band, was a visible share of every drag frame.
    pub fn shadow(&mut self, rect: Rect, radius: i32, blur: i32, strength: u8) {
        if blur <= 0 {
            return;
        }
        // Offset downward: a light source above is what every interface
        // assumes, and a shadow centred on its shape reads as a glow instead.
        let cast = Rect::new(rect.x, rect.y + blur / 3, rect.w, rect.h);

        // One alpha per distance from the edge, shared by all four strips.
        let falloff: Vec<u8> = (0..blur)
            .map(|d| {
                let fade = 1.0 - (d as f32 + 0.5) / blur as f32;
                (strength as f32 * fade * fade) as u8
            })
            .collect();

        let cs = radius + blur;
        let (left, right) = (cast.x + cs, cast.x + cast.w - cs);
        let (top, bottom) = (cast.y + cs, cast.y + cast.h - cs);

        // Edge strips: constant alpha along the edge, falloff across it.
        for (d, &alpha) in falloff.iter().enumerate() {
            if alpha == 0 {
                continue;
            }
            let d = d as i32;
            self.blend_run_h(left, right, cast.y - 1 - d, alpha);
            self.blend_run_h(left, right, cast.y + cast.h + d, alpha);
            self.blend_run_v(cast.x - 1 - d, top, bottom, alpha);
            self.blend_run_v(cast.x + cast.w + d, top, bottom, alpha);
        }

        // Corner tiles, mirrored from one cached quadrant.
        let tile = corner_tile(radius, blur, strength);
        let side = (cs + blur) as usize;
        for (corner_x, corner_y, flip_x, flip_y) in [
            (cast.x - blur, cast.y - blur, false, false),
            (cast.x + cast.w - cs, cast.y - blur, true, false),
            (cast.x - blur, cast.y + cast.h - cs, false, true),
            (cast.x + cast.w - cs, cast.y + cast.h - cs, true, true),
        ] {
            for ty in 0..side {
                for tx in 0..side {
                    let sx = if flip_x { side - 1 - tx } else { tx };
                    let sy = if flip_y { side - 1 - ty } else { ty };
                    let alpha = tile[sy * side + sx];
                    if alpha > 0 {
                        self.blend(corner_x + tx as i32, corner_y + ty as i32, 0x000000, alpha);
                    }
                }
            }
        }
    }

    /// A horizontal run of one shadow alpha.
    fn blend_run_h(&mut self, x0: i32, x1: i32, y: i32, alpha: u8) {
        for x in x0.max(self.clip.x)..x1.min(self.clip.x + self.clip.w) {
            self.blend(x, y, 0x000000, alpha);
        }
    }

    /// A vertical run of one shadow alpha.
    fn blend_run_v(&mut self, x: i32, y0: i32, y1: i32, alpha: u8) {
        for y in y0.max(self.clip.y)..y1.min(self.clip.y + self.clip.h) {
            self.blend(x, y, 0x000000, alpha);
        }
    }

    /// A stroked line segment, drawn as squares stepped along it.
    ///
    /// Not a real line rasterizer, and deliberately so: every stroke in the
    /// interface is a short glyph a few pixels long, where stepping a square is
    /// indistinguishable from Bresenham with a pen and costs nothing to get
    /// right. Anything that needs long precise lines should not be drawn with
    /// this.
    pub fn stroke_line(&mut self, ax: f32, ay: f32, bx: f32, by: f32, thickness: i32, color: Color) {
        let steps = ((bx - ax).abs().max((by - ay).abs()).ceil() as i32).max(1) * 2;
        let t = thickness.max(1);
        for step in 0..=steps {
            let f = step as f32 / steps as f32;
            let x = (ax + (bx - ax) * f).round() as i32;
            let y = (ay + (by - ay) * f).round() as i32;
            self.fill_rect(Rect::new(x - t / 2, y - t / 2, t, t), color);
        }
    }

    /// Rasterize a shape through tiny-skia and composite it onto the frame.
    ///
    /// tiny-skia is the path-and-gradient engine behind Canvas, and this is the
    /// one door it comes through. The shape is rendered into a scratch pixmap
    /// no larger than the clipped bounding box, with a transform that maps
    /// canvas coordinates into it, and [`Self::blend_pixmap`] converts its
    /// premultiplied RGBA into the XRGB frame. Rendering off to the side keeps
    /// the clip exact without tiny-skia's mask machinery, whose full-frame
    /// allocation is the wrong price for a rectangle.
    fn with_skia(&mut self, bbox: Rect, draw: impl FnOnce(&mut tiny_skia::PixmapMut, tiny_skia::Transform)) {
        let Some(area) = self.clip.intersect(&bbox) else {
            return;
        };
        let Some(mut scratch) = tiny_skia::Pixmap::new(area.w as u32, area.h as u32) else {
            return;
        };
        let to_scratch = tiny_skia::Transform::from_translate(-area.x as f32, -area.y as f32);
        draw(&mut scratch.as_mut(), to_scratch);
        self.blend_pixmap(scratch.as_ref(), area.x, area.y);
    }

    /// Composite a premultiplied RGBA pixmap onto the frame at `(x, y)`.
    ///
    /// This is the only place the two pixel formats meet: tiny-skia and resvg
    /// produce RGBA with premultiplied alpha, the frame is XRGB, and the
    /// conversion is source-over against an opaque destination, one pixel at a
    /// time, clipped like every other primitive.
    pub fn blend_pixmap(&mut self, pixmap: tiny_skia::PixmapRef, x: i32, y: i32) {
        let Some(area) =
            self.clip.intersect(&Rect::new(x, y, pixmap.width() as i32, pixmap.height() as i32))
        else {
            return;
        };
        let data = pixmap.data();
        let stride = pixmap.width() as usize * 4;

        for row in 0..area.h {
            let sy = (area.y + row - y) as usize;
            let src = &data[sy * stride + (area.x - x) as usize * 4..];
            let dst = ((area.y + row) * self.width + area.x) as usize;
            for column in 0..area.w as usize {
                let p = &src[column * 4..column * 4 + 4];
                let alpha = p[3] as u32;
                if alpha == 0 {
                    continue;
                }
                if alpha == 255 {
                    self.pixels[dst + column] = rgb(p[0], p[1], p[2]);
                    continue;
                }
                let under = self.pixels[dst + column];
                let inv = 255 - alpha;
                let mix = |channel: u8, shift: u32| {
                    let below = (under >> shift) & 0xff;
                    // The source is premultiplied, so it is added as it stands.
                    (channel as u32 + (below * inv + 127) / 255) << shift
                };
                self.pixels[dst + column] = mix(p[0], 16) | mix(p[1], 8) | mix(p[2], 0);
            }
        }
    }

    /// Darken a rectangle by blending black over it at `alpha`, clipped.
    ///
    /// The scrim under a modal dialog. A per-pixel blend, so it is paid only
    /// while a dialog is open, over the one window that has one.
    pub fn dim(&mut self, rect: Rect, alpha: u8) {
        let Some(area) = self.clip.intersect(&rect) else { return };
        let keep = 255 - alpha as u32;
        for y in area.y..area.y + area.h {
            let start = (y * self.width + area.x) as usize;
            for pixel in &mut self.pixels[start..start + area.w as usize] {
                let p = *pixel;
                let ch = |shift: u32| (((p >> shift) & 0xff) * keep / 255) << shift;
                *pixel = ch(16) | ch(8) | ch(0);
            }
        }
    }

    /// Copy an opaque bitmap with its top-left corner at `x`, `y`, clipped.
    ///
    /// Row copies and nothing else: this is the wallpaper's path, and a
    /// wallpaper is repainted under every frame that touches the desk.
    pub fn blit(&mut self, bitmap: &Bitmap, x: i32, y: i32) {
        let Some(area) = self.clip.intersect(&Rect::new(x, y, bitmap.width, bitmap.height)) else {
            return;
        };
        for row in 0..area.h {
            let sy = (area.y + row - y) as usize;
            let src = sy * bitmap.width as usize + (area.x - x) as usize;
            let dst = ((area.y + row) * self.width + area.x) as usize;
            self.pixels[dst..dst + area.w as usize]
                .copy_from_slice(&bitmap.pixels[src..src + area.w as usize]);
        }
    }

    /// A rounded rectangle filled with a vertical gradient.
    ///
    /// The gradient sibling of [`Self::fill_round_rect`]: the same row fills
    /// and corner coverage, with the colour interpolated per row. The first
    /// version rendered this through tiny-skia instead, and the frames log
    /// answered: a maximized window of gradient buttons doubled the paint
    /// time, because every repaint re-rasterized every button into a scratch
    /// pixmap. A vertical gradient on a rectilinear shape is row fills, and
    /// row fills are what this canvas is already fast at. tiny-skia stays for
    /// what genuinely needs a rasterizer: icons, and shapes a row cannot
    /// describe.
    pub fn fill_round_rect_vgrad(&mut self, rect: Rect, radius: i32, top: Color, bottom: Color) {
        if rect.w <= 0 || rect.h <= 0 {
            return;
        }
        let r = radius.min(rect.w / 2).min(rect.h / 2);

        // The straight spans: full width through the middle, inset beside the
        // corners. One fill per row, each with its own colour.
        for row in 0..rect.h {
            let color = lerp_color(top, bottom, row, rect.h - 1);
            let (x, w) = if row < r || row >= rect.h - r {
                (rect.x + r, rect.w - r * 2)
            } else {
                (rect.x, rect.w)
            };
            self.fill_rect(Rect::new(x, rect.y + row, w, 1), color);
        }
        if r <= 0 {
            return;
        }

        for (corner, (cx, cy)) in corners(rect, r) {
            for dy in 0..r {
                let py = corner.1 + dy;
                let color = lerp_color(top, bottom, py - rect.y, rect.h - 1);
                for dx in 0..r {
                    let px = corner.0 + dx;
                    let coverage = disc_coverage(px, py, cx, cy, r as f32);
                    self.blend(px, py, color, coverage);
                }
            }
        }
    }

    /// Fill this canvas from a region of another, for cutting overlay patches
    /// out of the painted scene.
    pub fn copy_from(&mut self, source: &Canvas, src_x: i32, src_y: i32) {
        for y in 0..self.height {
            let sy = src_y + y;
            if sy < 0 || sy >= source.height {
                continue;
            }
            let from = (src_x.max(0)).min(source.width);
            let to = (src_x + self.width).clamp(0, source.width);
            if from >= to {
                continue;
            }
            let dst_off = (y * self.width + (from - src_x)) as usize;
            let src_off = (sy * source.width + from) as usize;
            let count = (to - from) as usize;
            self.pixels[dst_off..dst_off + count]
                .copy_from_slice(&source.pixels[src_off..src_off + count]);
        }
    }

    /// The finished frame, row by row, for copying to a scanout buffer.
    pub fn rows(&self) -> impl Iterator<Item = &[Color]> {
        self.pixels.chunks_exact(self.width as usize)
    }
}
