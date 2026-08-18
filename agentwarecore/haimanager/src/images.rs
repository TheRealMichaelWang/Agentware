//! Pictures named by a document: wallpapers, previews, whatever an `image`
//! element points at. Loaded, fitted, cached.
//!
//! An `image` names a file and the compositor draws it. That is the whole of
//! the contract, and it is what keeps pixels out of the protocol: an agentdesk
//! that wants a wallpaper sends a path, never a bitmap, and the same path in
//! five workspaces is one rasterization. SVG and PNG, because between them one
//! is a drawing that scales to any display and the other is a photograph.
//!
//! ## Why the cache is lazy and interior
//!
//! An icon's size is known when its app attaches, so icons are prepared before
//! paint. An image's size is known only once layout has given it a rectangle,
//! and the region behind a workspace changes size when the display does. So
//! the paint path asks for `(source, width, height)` and the first ask does
//! the work. The cell makes that possible from behind the shared reference the
//! painter holds; the compositor is one thread and this is the honest way to
//! say so.
//!
//! ## Why the result is opaque
//!
//! What is cached is not a pixmap but rows of XRGB already composited over the
//! system background, so drawing one is a row copy: a full-screen wallpaper
//! costs a memcpy per frame, not a per-pixel blend of four million pixels.
//! That is the difference between a wallpaper and a frame budget. `cover` is
//! the only fit, so the picture always fills its rectangle and there is no
//! margin whose colour would have to be decided.
//!
//! The file is untrusted input into the one process that owns the screen. It
//! is read only if it is a regular file below a size ceiling; the decoders are
//! safe Rust, so the ceiling is about memory, not memory safety.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Read;
use std::rc::Rc;

use resvg::usvg;

use crate::icons::Icons;
use crate::paint::{Bitmap, Color, rgb};

/// Longest image file that will be read. Room for a large photograph, none
/// for a bomb.
const MAX_FILE: u64 = 16 * 1024 * 1024;

/// A source, decoded once, however many sizes it is later drawn at.
enum Source {
    Vector(Box<usvg::Tree>),
    Raster(tiny_skia::Pixmap),
}

/// A fitted picture, or the memory that fitting failed.
type Fitted = Option<Rc<Bitmap>>;

pub struct Images {
    /// Application icons, prepared when an app attaches. Kept here so the
    /// painter reaches both kinds of picture through one handle: a button
    /// with an `icon` is painted from the same cache the dock reads.
    pub icons: Icons,
    /// Decoded sources by path. `None` records a load that failed, so a bad
    /// path costs one probe rather than one per frame.
    sources: RefCell<HashMap<String, Option<Rc<Source>>>>,
    /// Fitted, composited results by path and size.
    fitted: RefCell<HashMap<(String, i32, i32), Fitted>>,
    /// What shows through where a picture is transparent.
    background: Color,
}

impl Images {
    pub fn new(background: Color) -> Images {
        Images {
            icons: Icons::new(),
            sources: RefCell::new(HashMap::new()),
            fitted: RefCell::new(HashMap::new()),
            background,
        }
    }

    /// The picture at `path`, covering `width` by `height`. `None` if it
    /// cannot be loaded, which is remembered.
    pub fn get(&self, path: &str, width: i32, height: i32) -> Option<Rc<Bitmap>> {
        if width <= 0 || height <= 0 {
            return None;
        }
        let key = (path.to_owned(), width, height);
        if let Some(hit) = self.fitted.borrow().get(&key) {
            return hit.clone();
        }
        let source = self.source(path);
        let bitmap = source.and_then(|source| fit(&source, width, height, self.background)).map(Rc::new);
        self.fitted.borrow_mut().insert(key, bitmap.clone());
        bitmap
    }

    /// Change what shows through where a picture is transparent.
    ///
    /// Called when the theme changes. Every fitted picture composited the old
    /// backdrop into its rows, so they are all stale; the decoded sources are
    /// not, and refitting from them is the cheap half of the work.
    pub fn set_background(&mut self, background: Color) {
        if self.background == background {
            return;
        }
        self.background = background;
        self.fitted.borrow_mut().clear();
    }

    fn source(&self, path: &str) -> Option<Rc<Source>> {
        if let Some(hit) = self.sources.borrow().get(path) {
            return hit.clone();
        }
        let loaded = load(path).map(Rc::new);
        self.sources.borrow_mut().insert(path.to_owned(), loaded.clone());
        loaded
    }
}

/// Render a source into an opaque bitmap, scaled to cover it and centred.
fn fit(source: &Source, width: i32, height: i32, background: Color) -> Option<Bitmap> {
    let mut pixmap = tiny_skia::Pixmap::new(width as u32, height as u32)?;
    // Composited onto the background rather than onto transparent black, so
    // what an image leaves uncovered is the same colour as everything else the
    // workspace shows through.
    pixmap.fill(tiny_skia::Color::from_rgba8(
        ((background >> 16) & 0xff) as u8,
        ((background >> 8) & 0xff) as u8,
        (background & 0xff) as u8,
        255,
    ));

    let (from_w, from_h) = match source {
        Source::Vector(tree) => (tree.size().width(), tree.size().height()),
        Source::Raster(image) => (image.width() as f32, image.height() as f32),
    };
    if from_w <= 0.0 || from_h <= 0.0 {
        return None;
    }
    // Cover: the larger of the two scales, so the shorter side fills exactly
    // and the longer side is cropped equally at both ends.
    let scale = (width as f32 / from_w).max(height as f32 / from_h);
    let dx = (width as f32 - from_w * scale) / 2.0;
    let dy = (height as f32 - from_h * scale) / 2.0;
    let transform = tiny_skia::Transform::from_scale(scale, scale).post_translate(dx, dy);

    match source {
        Source::Vector(tree) => resvg::render(tree, transform, &mut pixmap.as_mut()),
        Source::Raster(image) => {
            let paint = tiny_skia::PixmapPaint {
                quality: tiny_skia::FilterQuality::Bilinear,
                ..tiny_skia::PixmapPaint::default()
            };
            pixmap.draw_pixmap(0, 0, image.as_ref(), &paint, transform, None);
        }
    }

    // Opaque now, so the alpha channel is spent and only the colour remains.
    let data = pixmap.data();
    let mut rows = Vec::with_capacity((width * height) as usize);
    for pixel in data.chunks_exact(4) {
        rows.push(rgb(pixel[0], pixel[1], pixel[2]));
    }
    Some(Bitmap { width, height, pixels: rows })
}

fn load(path: &str) -> Option<Source> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_FILE {
        return None;
    }
    let mut data = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(MAX_FILE)
        .read_to_end(&mut data)
        .ok()?;

    if path.ends_with(".png") {
        return tiny_skia::Pixmap::decode_png(&data).ok().map(Source::Raster);
    }
    usvg::Tree::from_data(&data, &usvg::Options::default())
        .ok()
        .map(|tree| Source::Vector(Box::new(tree)))
}
