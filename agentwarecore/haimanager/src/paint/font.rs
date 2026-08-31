//! Real typography: outline fonts, rasterized and cached.
//!
//! Glyphs come out of `fontdue` as 8-bit coverage maps, which the canvas blends
//! rather than plots. That is the whole difference between this and the bitmap
//! font it replaces: a stem is no longer either on or off, so text stays
//! readable at sizes that are not multiples of anything.
//!
//! Rasterizing is expensive and repeats constantly, since a desktop draws the
//! same few hundred glyphs at the same few sizes on every frame. Every glyph is
//! therefore cached by face, size and character. The cache lives behind a
//! `RefCell` so painting can take `&Fonts`: a renderer that had to thread
//! `&mut` through every draw call would push that borrow into every signature
//! in the layout engine for no benefit.
//!
//! DejaVu is embedded under the Bitstream Vera licence, which permits
//! redistribution and embedding. See `assets/fonts/LICENSE.txt`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

const SANS: &[u8] = include_bytes!("../../assets/fonts/DejaVuSans.ttf");
const SANS_BOLD: &[u8] = include_bytes!("../../assets/fonts/DejaVuSans-Bold.ttf");
const MONO: &[u8] = include_bytes!("../../assets/fonts/DejaVuSansMono.ttf");

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum Family {
    #[default]
    Sans,
    Mono,
}

impl Family {
    pub fn parse(name: &str) -> Option<Family> {
        match name {
            "sans" => Some(Family::Sans),
            "mono" => Some(Family::Mono),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum Weight {
    #[default]
    Normal,
    Bold,
}

impl Weight {
    pub fn parse(name: &str) -> Option<Weight> {
        match name {
            "normal" | "regular" => Some(Weight::Normal),
            "bold" => Some(Weight::Bold),
            _ => None,
        }
    }
}

/// How a run of text should be drawn. Colour is deliberately not in here: it is
/// a paint decision, while these three choose which outlines to rasterize.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Style {
    pub family: Family,
    pub weight: Weight,
    pub italic: bool,
    /// Em size in pixels.
    pub size: f32,
}

impl Default for Style {
    fn default() -> Self {
        Self { family: Family::Sans, weight: Weight::Normal, italic: false, size: 15.0 }
    }
}

impl Style {
    pub fn sized(size: f32) -> Self {
        Self { size, ..Self::default() }
    }

    /// The cache key. Size is quantised to a tenth of a pixel, because caching
    /// on a raw float would miss on rounding noise and grow without bound.
    fn key(&self, character: char) -> GlyphKey {
        GlyphKey {
            face: self.face(),
            italic: self.italic,
            size: (self.size * 10.0).round() as u32,
            character,
        }
    }

    fn face(&self) -> usize {
        match (self.family, self.weight) {
            (Family::Sans, Weight::Normal) => 0,
            (Family::Sans, Weight::Bold) => 1,
            // One mono face. Bold monospace is synthesized by the caller
            // drawing twice rather than by shipping a fourth megabyte.
            (Family::Mono, _) => 2,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct GlyphKey {
    face: usize,
    italic: bool,
    size: u32,
    character: char,
}

/// Which broken lines are being asked for: a string, the face it is set in,
/// and the width it has to fit. The text is owned because the point of the
/// cache is not to walk it again, and because the same few hundred lines come
/// back unchanged every time an agentdesk re-renders its transcript.
#[derive(PartialEq, Eq, Hash)]
struct WrapKey {
    face: usize,
    italic: bool,
    size: u32,
    width: i32,
    text: String,
}

/// How many broken strings to remember.
///
/// Past this the cache is dropped whole rather than evicted an entry at a
/// time: the next layout refills exactly what it needs, and keeping a
/// least-recently-used order would cost more bookkeeping than the measuring
/// it saves. Generous enough that a long conversation never reaches it.
const WRAPS_KEPT: usize = 8192;

/// Where each line of a broken string starts and ends, as byte offsets into
/// it. Shared, so asking twice costs a lookup rather than a copy.
type Lines = Rc<[(u32, u32)]>;

/// A rasterized glyph: a coverage map and where to put it.
pub struct Glyph {
    pub width: usize,
    pub height: usize,
    /// Offset from the pen position to the left edge of the coverage map.
    pub left: i32,
    /// Offset from the baseline up to the top edge of the coverage map.
    pub top: i32,
    /// How far the pen moves after drawing this glyph.
    pub advance: f32,
    /// One byte of coverage per pixel, row major.
    pub coverage: Vec<u8>,
}

/// Vertical metrics for a line of text at a given style.
#[derive(Clone, Copy)]
pub struct LineMetrics {
    pub ascent: f32,
    pub descent: f32,
    pub height: f32,
}

pub struct Fonts {
    faces: Vec<fontdue::Font>,
    glyphs: RefCell<HashMap<GlyphKey, Glyph>>,
    /// Where each line of a wrapped string starts and ends, by string and
    /// width. See [`Fonts::break_lines`].
    wraps: RefCell<HashMap<WrapKey, Lines>>,
}

impl Fonts {
    pub fn load() -> Result<Self, String> {
        let settings = fontdue::FontSettings::default();
        let mut faces = Vec::new();

        for (name, bytes) in [("sans", SANS), ("sans-bold", SANS_BOLD), ("mono", MONO)] {
            let face = fontdue::Font::from_bytes(bytes, settings)
                .map_err(|err| format!("could not load the {name} face: {err}"))?;
            faces.push(face);
        }

        Ok(Self {
            faces,
            glyphs: RefCell::new(HashMap::new()),
            wraps: RefCell::new(HashMap::new()),
        })
    }

    pub fn line_metrics(&self, style: &Style) -> LineMetrics {
        let face = &self.faces[style.face()];
        match face.horizontal_line_metrics(style.size) {
            Some(metrics) => LineMetrics {
                ascent: metrics.ascent,
                descent: metrics.descent,
                height: metrics.new_line_size,
            },
            // A face without line metrics is malformed. Falling back on the em
            // size keeps text on screen rather than collapsing every line to
            // zero height, which would look like a layout bug.
            None => LineMetrics {
                ascent: style.size * 0.8,
                descent: -style.size * 0.2,
                height: style.size * 1.2,
            },
        }
    }

    /// Rasterize a glyph, or return the one already rasterized.
    ///
    /// The closure form keeps the borrow of the cache inside this function.
    /// Handing out a reference would either leak the `RefCell` guard into the
    /// caller or force a copy of the coverage map on every glyph drawn.
    pub fn with_glyph<T>(&self, style: &Style, character: char, use_it: impl FnOnce(&Glyph) -> T) -> T {
        let key = style.key(character);

        if let Some(glyph) = self.glyphs.borrow().get(&key) {
            return use_it(glyph);
        }

        let (metrics, coverage) =
            self.faces[style.face()].rasterize(character, style.size);

        let glyph = Glyph {
            width: metrics.width,
            height: metrics.height,
            left: metrics.xmin,
            // fontdue reports ymin as the distance from the baseline to the
            // bottom of the bitmap, positive upward. The top edge is therefore
            // that plus the height.
            top: metrics.height as i32 + metrics.ymin,
            advance: metrics.advance_width,
            coverage,
        };

        self.glyphs.borrow_mut().insert(key, glyph);
        let cache = self.glyphs.borrow();
        use_it(&cache[&key])
    }

    /// How far the pen moves after drawing one character.
    ///
    /// Exposed because measuring a string one character longer than the last
    /// one is the inner loop of line breaking, and re-measuring the whole
    /// prefix to do it is quadratic. A caller that is walking forward keeps a
    /// running total out of these.
    pub fn advance(&self, style: &Style, character: char) -> f32 {
        let mut width = self.with_glyph(style, character, |glyph| glyph.advance);
        if style.weight == Weight::Bold && style.family == Family::Mono {
            // Synthesized bold is one extra pixel wide per glyph.
            width += 1.0;
        }
        width
    }

    /// How wide a string is when drawn.
    pub fn measure(&self, text: &str, style: &Style) -> i32 {
        let mut width = 0.0;
        for character in text.chars() {
            width += self.advance(style, character);
        }
        width.ceil() as i32
    }

    pub fn line_height(&self, style: &Style) -> i32 {
        self.line_metrics(style).height.ceil() as i32
    }

    pub fn glyph_count(&self) -> usize {
        self.glyphs.borrow().len()
    }

    /// Where each line starts and ends when a string is broken to a width.
    ///
    /// Line breaking is the expensive half of laying out text, and layout has
    /// to do it for content nobody can see: a `scroll` container is only as
    /// tall as its content, so the pane cannot know its own height without
    /// measuring every line of the conversation, including the thousand that
    /// have scrolled off the top. Painting was taught to skip what is out of
    /// view; measuring cannot be, so it is remembered instead. A transcript
    /// re-rendered for its clock breaks no lines at all the second time.
    ///
    /// Returned as ranges rather than slices so the answer can outlive the
    /// borrow of the string it describes, and shared so that asking twice
    /// costs a lookup rather than a copy.
    pub fn break_lines(&self, text: &str, style: &Style, width: i32) -> Lines {
        let key = WrapKey {
            face: style.face(),
            italic: style.italic,
            size: (style.size * 10.0).round() as u32,
            width,
            text: text.to_owned(),
        };
        if let Some(hit) = self.wraps.borrow().get(&key) {
            return hit.clone();
        }

        let lines: Lines = Rc::from(self.compute_breaks(text, style, width));
        let mut cache = self.wraps.borrow_mut();
        if cache.len() >= WRAPS_KEPT {
            cache.clear();
        }
        cache.insert(key, lines.clone());
        lines
    }

    /// How many lines a string breaks into, which is all that measuring one
    /// needs to know.
    pub fn line_count(&self, text: &str, style: &Style, width: i32) -> usize {
        self.break_lines(text, style, width).len()
    }

    /// Break a string to a width: paragraphs on newlines, and within one, at
    /// the last space that fits, or mid-word when a word does not.
    ///
    /// The width of the line so far is carried forward rather than measured
    /// again at every character. Measuring the whole prefix per step is
    /// quadratic in the line's length, and it was: a transcript of four
    /// hundred telemetry lines took 147ms to lay out, which is a sixth of a
    /// second of the compositor's thread every time the agentdesk re-rendered,
    /// and it re-renders once a second while a turn is running.
    fn compute_breaks(&self, text: &str, style: &Style, width: i32) -> Vec<(u32, u32)> {
        let mut lines: Vec<(u32, u32)> = Vec::new();
        let mut base = 0usize;

        for paragraph in text.split('\n') {
            let put = |lines: &mut Vec<(u32, u32)>, from: usize, to: usize| {
                lines.push(((base + from) as u32, (base + to) as u32));
            };

            if paragraph.is_empty() {
                put(&mut lines, 0, 0);
                base += 1;
                continue;
            }

            let mut start = 0;
            let mut last_space: Option<usize> = None;
            let mut at = 0;
            let mut run = 0.0f32;

            while at < paragraph.len() {
                let character = paragraph[at..].chars().next().unwrap_or(' ');
                let next = at + character.len_utf8();
                let grown = run + self.advance(style, character);
                if grown.ceil() as i32 > width && next > start {
                    // Over the edge. Break at the last space if there was one,
                    // else right here, but always make progress by at least
                    // one character.
                    let (line_end, resume) = match last_space {
                        Some(space) if space > start => (space, space + 1),
                        _ if at > start => (at, at),
                        _ => (next, next),
                    };
                    put(&mut lines, start, line_end);
                    start = resume;
                    last_space = None;
                    at = start;
                    run = 0.0;
                    continue;
                }
                run = grown;
                if character == ' ' {
                    last_space = Some(at);
                }
                at = next;
            }

            if start < paragraph.len() || lines.is_empty() {
                put(&mut lines, start, paragraph.len());
            }
            base += paragraph.len() + 1;
        }

        lines
    }
}
