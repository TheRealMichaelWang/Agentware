//! What the human last copied.
//!
//! One clipboard for the machine, held by the compositor, because the
//! compositor is the only process that sees both ends of a copy: it owns the
//! keyboard the chord arrives on and it owns the text being edited. Nothing
//! about it crosses the display protocol in either direction. A paste is an
//! ordinary `type-text` event carrying the value the control now has, which
//! is exactly what an application would have received had the human typed the
//! words out, so no application needs to know a clipboard exists and none can
//! read one it was not given.
//!
//! An agent may read it and may not write it. Reading is how it learns what
//! the human just copied, which is context it has no other way to get; there
//! is nothing to write, because an agent that wants text somewhere says so
//! with `type-text` rather than putting it down and picking it up again.
//!
//! ## Why this holds a kind rather than a string
//!
//! A clipboard that is a `String` is a clipboard that can only ever hold
//! words, and the day something copies a picture every reader of it has to be
//! found and changed. So what is held is a kind and its content, with text
//! the only kind anything produces today and images the shape the next one
//! will take: base64 in the content, `image` in the kind, and every reader
//! already written to say "I cannot read that" rather than to print the bytes
//! as if they were words.

use awproto::agent;

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum Held {
    /// Nothing has been copied yet.
    #[default]
    Empty,
    Text(String),
    /// A picture, as base64. Nothing puts one here yet.
    #[allow(dead_code)]
    Image(String),
}

#[derive(Default)]
pub struct Clipboard {
    held: Held,
}

impl Clipboard {
    /// Put words on it, replacing whatever was there.
    pub fn set_text(&mut self, text: &str) {
        self.held = if text.is_empty() { Held::Empty } else { Held::Text(text.to_owned()) };
    }

    /// The words on it, if what it holds is words.
    ///
    /// `None` for a picture as much as for nothing, because a caret cannot
    /// paste a picture into a text field and pretending otherwise would put
    /// base64 into someone's spreadsheet.
    pub fn text(&self) -> Option<&str> {
        match &self.held {
            Held::Text(text) => Some(text),
            _ => None,
        }
    }

    /// What kind of thing this is holding, in the agent surface's words.
    pub fn kind(&self) -> &'static str {
        match self.held {
            Held::Empty => agent::CLIPBOARD_NONE,
            Held::Text(_) => agent::CLIPBOARD_TEXT,
            Held::Image(_) => agent::CLIPBOARD_IMAGE,
        }
    }

    /// The content itself, whatever kind it is.
    pub fn content(&self) -> &str {
        match &self.held {
            Held::Empty => "",
            Held::Text(text) | Held::Image(text) => text,
        }
    }
}
