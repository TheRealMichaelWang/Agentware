//! Application icons: read from the app's package, rasterized, cached.
//!
//! An app is a folder, and `/apps/<name>/icon.svg` is the one piece of it the
//! compositor reads. SVG because chrome renders at whatever scale the display
//! chose, and the dock, the title bar and whatever lists apps next all want
//! different sizes; one vector file serves them all without anyone shipping a
//! bitmap for every case.
//!
//! Everything here happens off the paint path. An icon is parsed once when its
//! app first attaches and rasterized once per size ever asked for, so drawing
//! is a hash lookup and a blend. A failed load is cached too: an app without an
//! icon must not cost a filesystem probe per frame for the rest of its life.
//!
//! The file is untrusted input into the one process that owns the screen, so it
//! is read with a size ceiling. resvg is safe Rust throughout; the ceiling is
//! about memory, not memory safety.

use std::collections::HashMap;

use resvg::usvg;

/// Where installed applications live. The same directory the supervisor forks
/// `exec` from; the compositor reads the icon beside it and nothing else.
const APP_DIR: &str = "/apps";

/// Longest icon file that will be read. Generous for any hand-drawn icon,
/// far too small for a bomb.
const MAX_SVG: u64 = 64 * 1024;

pub struct Icons {
    /// Parsed trees by app name. `None` records a load that failed, so the
    /// answer "no icon" is as cached as any icon.
    trees: HashMap<String, Option<usvg::Tree>>,
    /// Rasterizations by app name and square size in physical pixels.
    rendered: HashMap<(String, i32), Option<tiny_skia::Pixmap>>,
}

impl Icons {
    pub fn new() -> Icons {
        Icons { trees: HashMap::new(), rendered: HashMap::new() }
    }

    /// Rasterize an app's icon at the given sizes, so later lookups are reads.
    ///
    /// Called when an app attaches, which is the moment the compositor learns
    /// the name. Doing the work here keeps [`Self::get`] callable from the
    /// paint path without mutation.
    pub fn prepare(&mut self, app: &str, sizes: &[i32]) {
        for &size in sizes {
            let key = (app.to_owned(), size);
            if self.rendered.contains_key(&key) {
                continue;
            }
            let pixmap = self.rasterize(app, size);
            self.rendered.insert(key, pixmap);
        }
    }

    /// A previously prepared rasterization, if the app has an icon.
    pub fn get(&self, app: &str, size: i32) -> Option<&tiny_skia::Pixmap> {
        self.rendered.get(&(app.to_owned(), size))?.as_ref()
    }

    /// Install an icon from SVG data under a name, for the compositor's own
    /// marks: things that are not applications and have no package to be read
    /// from. Later `prepare` calls rasterize it like any other.
    pub fn install(&mut self, name: &str, svg: &[u8]) {
        let tree = usvg::Tree::from_data(svg, &usvg::Options::default()).ok();
        self.trees.insert(name.to_owned(), tree);
    }

    fn rasterize(&mut self, app: &str, size: i32) -> Option<tiny_skia::Pixmap> {
        if size <= 0 {
            return None;
        }
        let tree = self.tree(app)?;

        let mut pixmap = tiny_skia::Pixmap::new(size as u32, size as u32)?;
        let from = tree.size();
        if from.width() <= 0.0 || from.height() <= 0.0 {
            return None;
        }
        // Fit inside the square, preserving aspect, centred. Icons are usually
        // square already; one that is not should not be stretched into lying
        // about its shape.
        let scale = (size as f32 / from.width()).min(size as f32 / from.height());
        let dx = (size as f32 - from.width() * scale) / 2.0;
        let dy = (size as f32 - from.height() * scale) / 2.0;
        let transform = tiny_skia::Transform::from_scale(scale, scale).post_translate(dx, dy);
        resvg::render(tree, transform, &mut pixmap.as_mut());
        Some(pixmap)
    }

    /// The parsed tree for an app, loading it on first ask.
    ///
    /// Borrow gymnastics aside, this is: read `/apps/<name>/icon.svg`, parse
    /// it, remember the outcome either way.
    fn tree(&mut self, app: &str) -> Option<&usvg::Tree> {
        if !self.trees.contains_key(app) {
            let loaded = load(app);
            self.trees.insert(app.to_owned(), loaded);
        }
        self.trees.get(app)?.as_ref()
    }
}

fn load(app: &str) -> Option<usvg::Tree> {
    let path = format!("{APP_DIR}/{app}/icon.svg");
    let meta = std::fs::metadata(&path).ok()?;
    if meta.len() > MAX_SVG {
        return None;
    }
    let data = std::fs::read(&path).ok()?;
    usvg::Tree::from_data(&data, &usvg::Options::default()).ok()
}
