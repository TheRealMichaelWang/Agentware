//! One text box, wherever it appears.
//!
//! There were four of these. An application's `field` and `editor` had the
//! whole of it: a caret, a selection, the clipboard chords. The start menu's
//! prompt had `push` and `pop`, so it could not be selected in, copied out
//! of, pasted into, or even have its caret moved. The navigation bar's rename
//! field had the same two lines. Nobody would have designed that; it happened
//! because each of the three was written where it was needed, and a text box
//! is small enough that writing it again never feels like the mistake it is.
//!
//! So this is the text box, and the three of them are its callers. What is
//! *not* here is what differs between them: an application has to be told the
//! value changed, the start menu's prompt becomes a workspace, the rename
//! field commits a name. Those are decisions about what the words are for.
//! Everything about the words themselves is here.
//!
//! None of it ever reaches an application. A selection is the compositor's, a
//! paste arrives as an ordinary `type-text` carrying the value the control
//! ended up with, and no modifier exists in the event vocabulary at all.

use std::time::{Duration, Instant};

use crate::clipboard::Clipboard;
use crate::input::Key;

/// How long after a press a second one in the same place is a double rather
/// than two singles. The interval every desktop uses, near enough that nobody
/// has to think about it.
const DOUBLE: Duration = Duration::from_millis(400);

/// How far the pointer may wander between the two and still count. A hand
/// that moves a few pixels between clicks meant to click twice.
const DOUBLE_SLOP: i32 = 4;

/// What a keystroke did to a text box.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Edit {
    /// Nothing here answers to that key. The caller decides what else might.
    Ignored,
    /// The caret or the selection moved. The value is what it was.
    Moved,
    /// The value changed.
    Changed,
}

/// The compositor's copy of one text box: what is on screen, where the caret
/// is, and what is selected.
pub struct Editing {
    /// What is on screen, which may be ahead of what the application holds.
    pub value: String,
    /// Caret position, in characters.
    pub caret: usize,
    /// Where a selection began, when there is one. The selected run is
    /// everything between this and the caret, in either order, and it is the
    /// compositor's alone: an application is told what the value became, not
    /// which part of it was highlighted on the way.
    pub anchor: Option<usize>,
    /// Values sent to the application and not yet seen echoed back in a tree.
    /// Empty for the compositor's own boxes, which have nobody to tell.
    pub outstanding: Vec<String>,
}

impl Editing {
    pub fn new(value: impl Into<String>) -> Editing {
        let value = value.into();
        let caret = value.chars().count();
        Editing { value, caret, anchor: None, outstanding: Vec::new() }
    }

    /// The selected run, or `None` when the caret is a point.
    pub fn selected(&self) -> Option<(usize, usize)> {
        let anchor = self.anchor?;
        let (from, to) = (anchor.min(self.caret), anchor.max(self.caret));
        (from != to).then_some((from, to))
    }

    pub fn selected_text(&self) -> Option<String> {
        let (from, to) = self.selected()?;
        Some(self.value.chars().skip(from).take(to - from).collect())
    }

    /// Remove the selected run, leaving the caret where it was. Returns
    /// whether there was anything to remove, which is what tells a backspace
    /// whether it has already done its work.
    pub fn delete_selection(&mut self) -> bool {
        let Some((from, to)) = self.selected() else { return false };
        let start = byte_at(&self.value, from);
        let end = byte_at(&self.value, to);
        self.value.replace_range(start..end, "");
        self.caret = from;
        self.anchor = None;
        true
    }

    pub fn select_all(&mut self) {
        self.anchor = Some(0);
        self.caret = self.value.chars().count();
    }

    /// Put the caret down, ending whatever selection there was and anchoring
    /// where a drag would start.
    pub fn press(&mut self, at: usize) {
        self.caret = at.min(self.value.chars().count());
        self.anchor = Some(self.caret);
    }

    /// Carry the far end of a selection to here.
    pub fn drag(&mut self, at: usize) -> bool {
        let at = at.min(self.value.chars().count());
        if self.caret == at {
            return false;
        }
        self.anchor.get_or_insert(self.caret);
        self.caret = at;
        true
    }

    /// A press that came too soon after the last one in the same place: the
    /// second takes the word under it, the third takes the whole line. What
    /// every desktop does, and the reason it is here rather than in a click
    /// handler is that all three text boxes need it.
    pub fn press_again(&mut self, at: usize, count: u32) {
        let (from, to) = if count >= 3 { self.line_at(at) } else { self.word_at(at) };
        self.anchor = Some(from);
        self.caret = to;
    }

    /// The run of word characters around an offset, or the run of spaces if
    /// that is what it landed in. Letters, digits and underscores are one
    /// word; everything else is punctuation and is taken one character at a
    /// time, which is what makes a double click on `foo.bar` take `foo`.
    pub fn word_at(&self, at: usize) -> (usize, usize) {
        word_at(&self.value, at)
    }

    /// The line around an offset: between the newlines either side of it, or
    /// the whole value when there are none.
    pub fn line_at(&self, at: usize) -> (usize, usize) {
        line_at(&self.value, at)
    }
}

/// The run of word characters around an offset in a string, or the run of
/// spaces if that is what it landed in.
///
/// Free functions as well as methods, because a run over static text is not
/// an `Editing` at all: there is nothing to edit in a paragraph on a page,
/// and a double click on one still has to take a word.
pub fn word_at(value: &str, at: usize) -> (usize, usize) {
    let chars: Vec<char> = value.chars().collect();
    if chars.is_empty() {
        return (0, 0);
    }
    // A caret at the very end belongs to the word before it.
    let at = at.min(chars.len() - 1);
    let kind = |c: char| {
        if c.is_alphanumeric() || c == '_' {
            2
        } else if c.is_whitespace() {
            1
        } else {
            0
        }
    };
    let here = kind(chars[at]);
    if here == 0 {
        return (at, at + 1);
    }
    let mut from = at;
    while from > 0 && kind(chars[from - 1]) == here {
        from -= 1;
    }
    let mut to = at;
    while to < chars.len() && kind(chars[to]) == here {
        to += 1;
    }
    (from, to)
}

pub fn line_at(value: &str, at: usize) -> (usize, usize) {
    let chars: Vec<char> = value.chars().collect();
    let at = at.min(chars.len());
    let mut from = at;
    while from > 0 && chars[from - 1] != '\n' {
        from -= 1;
    }
    let mut to = at;
    while to < chars.len() && chars[to] != '\n' {
        to += 1;
    }
    (from, to)
}

impl Editing {
    /// Put a string in at the caret, replacing whatever was selected.
    pub fn insert(&mut self, words: &str) {
        self.delete_selection();
        let at = byte_at(&self.value, self.caret);
        self.value.insert_str(at, words);
        self.caret += words.chars().count();
        self.anchor = None;
    }

    /// One keystroke against this box.
    ///
    /// `lines` says whether the value may hold newlines, which is what makes
    /// the vertical arrows and the two ends of a line mean anything: in a
    /// single-line box Home and End are the two ends of the value and up and
    /// down are somebody else's.
    ///
    /// Enter is deliberately not here. What it means is exactly the thing
    /// that differs between the callers, and there is no honest default.
    pub fn key(&mut self, key: Key, clipboard: &mut Clipboard, lines: bool) -> Edit {
        let length = self.value.chars().count();
        self.caret = self.caret.min(length);

        // Selection first: what the clipboard chords act on.
        match key {
            Key::SelectAll => {
                self.select_all();
                return Edit::Moved;
            }
            Key::Copy | Key::Cut => {
                let Some(words) = self.selected_text() else { return Edit::Ignored };
                clipboard.set_text(&words);
                if key == Key::Copy {
                    return Edit::Moved;
                }
                self.delete_selection();
                return Edit::Changed;
            }
            Key::Paste => {
                let Some(words) = clipboard.text().map(str::to_owned) else {
                    return Edit::Ignored;
                };
                self.insert(&words);
                return Edit::Changed;
            }
            _ => {}
        }

        // Moving with shift held extends the run instead of dropping it. The
        // anchor is where the run started, so it is set on the first such
        // keystroke and left alone after.
        let extend = matches!(
            key,
            Key::ShiftLeft
                | Key::ShiftRight
                | Key::ShiftUp
                | Key::ShiftDown
                | Key::ShiftHome
                | Key::ShiftEnd
        );
        if extend {
            self.anchor.get_or_insert(self.caret);
        }

        match key {
            Key::Char(character) => {
                self.delete_selection();
                self.value.insert(byte_at(&self.value, self.caret), character);
                self.caret += 1;
                self.anchor = None;
                Edit::Changed
            }
            Key::Backspace => {
                if self.delete_selection() {
                    Edit::Changed
                } else if self.caret == 0 {
                    Edit::Ignored
                } else {
                    self.caret -= 1;
                    self.value.remove(byte_at(&self.value, self.caret));
                    Edit::Changed
                }
            }
            Key::Delete => {
                if self.delete_selection() {
                    Edit::Changed
                } else if self.caret >= length {
                    Edit::Ignored
                } else {
                    self.value.remove(byte_at(&self.value, self.caret));
                    Edit::Changed
                }
            }

            // A run collapses to one of its ends rather than to the caret:
            // pressing left with words selected puts the caret at the start of
            // them, which is what every text box does and what makes a
            // selection a safe thing to have made by accident.
            Key::Left => {
                match self.selected() {
                    Some((from, _)) => self.caret = from,
                    None => self.caret = self.caret.saturating_sub(1),
                }
                self.anchor = None;
                Edit::Moved
            }
            Key::Right => {
                match self.selected() {
                    Some((_, to)) => self.caret = to,
                    None => self.caret = (self.caret + 1).min(length),
                }
                self.anchor = None;
                Edit::Moved
            }
            Key::ShiftLeft => {
                self.caret = self.caret.saturating_sub(1);
                Edit::Moved
            }
            Key::ShiftRight => {
                self.caret = (self.caret + 1).min(length);
                Edit::Moved
            }

            Key::Up | Key::Down if lines => {
                self.caret = move_line(&self.value, self.caret, key == Key::Down);
                self.anchor = None;
                Edit::Moved
            }
            Key::ShiftUp | Key::ShiftDown if lines => {
                self.caret = move_line(&self.value, self.caret, key == Key::ShiftDown);
                Edit::Moved
            }

            Key::Home | Key::ShiftHome => {
                self.caret = if lines { self.line_at(self.caret).0 } else { 0 };
                if key == Key::Home {
                    self.anchor = None;
                }
                Edit::Moved
            }
            Key::End | Key::ShiftEnd => {
                self.caret = if lines { self.line_at(self.caret).1 } else { length };
                if key == Key::End {
                    self.anchor = None;
                }
                Edit::Moved
            }

            _ => Edit::Ignored,
        }
    }
}

/// Whether a press is the second or third of a run in the same place.
///
/// One counter rather than one per text box, because what makes a double
/// click is the hand rather than the thing under it: two presses close
/// together in time and place, wherever they landed.
#[derive(Default)]
pub struct MultiPress {
    last: Option<(Instant, i32, i32)>,
    count: u32,
}

impl MultiPress {
    /// Count this press. One for a fresh press, two for a double, three for a
    /// triple, and back to one after that: a fourth click starts again rather
    /// than selecting something nobody has a name for.
    pub fn press(&mut self, x: i32, y: i32) -> u32 {
        let near = self.last.is_some_and(|(when, at_x, at_y)| {
            when.elapsed() < DOUBLE
                && (at_x - x).abs() <= DOUBLE_SLOP
                && (at_y - y).abs() <= DOUBLE_SLOP
        });
        self.count = if near && self.count < 3 { self.count + 1 } else { 1 };
        self.last = Some((Instant::now(), x, y));
        self.count
    }
}

/// The byte offset of a character position.
pub fn byte_at(text: &str, caret: usize) -> usize {
    text.char_indices().nth(caret).map_or(text.len(), |(at, _)| at)
}

/// Move a caret one line up or down, keeping the column where it can.
fn move_line(value: &str, caret: usize, down: bool) -> usize {
    let (row, column) = row_and_column(value, caret);
    let target = if down { row + 1 } else { row.saturating_sub(1) };

    let mut at = 0;
    for (index, line) in value.split('\n').enumerate() {
        let length = line.chars().count();
        if index == target {
            return at + column.min(length);
        }
        at += length + 1;
    }
    caret
}

/// Which line a caret is on and how far along it, counting the newlines the
/// value carries.
fn row_and_column(value: &str, caret: usize) -> (usize, usize) {
    let mut at = 0;
    for (row, line) in value.split('\n').enumerate() {
        let length = line.chars().count();
        if caret <= at + length {
            return (row, caret - at);
        }
        at += length + 1;
    }
    (0, caret)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn box_of(value: &str, caret: usize) -> Editing {
        let mut state = Editing::new(value);
        state.caret = caret;
        state
    }

    /// What a double click takes. Letters, digits and underscores are one
    /// word; a run of spaces is one thing too, so a double click in the gap
    /// between two words takes the gap rather than nothing; and punctuation
    /// is one character at a time, which is what makes `foo.bar` three words
    /// rather than one.
    #[test]
    fn a_word_is_what_a_double_click_takes() {
        assert_eq!(word_at("the quick brown", 5), (4, 9));
        assert_eq!(word_at("the quick brown", 0), (0, 3));
        // In the gap.
        assert_eq!(word_at("the quick", 3), (3, 4));
        // Punctuation on its own.
        assert_eq!(word_at("foo.bar", 3), (3, 4));
        assert_eq!(word_at("foo.bar", 5), (4, 7));
        // Past the end belongs to the last word.
        assert_eq!(word_at("hello", 5), (0, 5));
        assert_eq!(word_at("", 0), (0, 0));

        // A line is between the newlines either side.
        assert_eq!(line_at("one\ntwo\nthree", 5), (4, 7));
        assert_eq!(line_at("one line only", 4), (0, 13));
    }

    /// Holding shift turns a movement into a selection, and letting go of it
    /// again does not throw the selection away: the arrow keys without shift
    /// are how you stop having one, and they collapse to the end you were
    /// moving toward rather than to wherever the caret happened to be.
    #[test]
    fn shift_and_the_arrows_drag_a_selection_out() {
        let mut clipboard = Clipboard::default();
        let mut state = box_of("hello world", 11);

        for _ in 0..5 {
            assert_eq!(state.key(Key::ShiftLeft, &mut clipboard, false), Edit::Moved);
        }
        assert_eq!(state.selected_text().as_deref(), Some("world"));

        // Copy takes it, and the run is still there afterwards.
        assert_eq!(state.key(Key::Copy, &mut clipboard, false), Edit::Moved);
        assert_eq!(clipboard.text(), Some("world"));
        assert_eq!(state.selected_text().as_deref(), Some("world"));

        // Left without shift collapses to the start of the run rather than
        // one character back from the caret.
        assert_eq!(state.key(Key::Left, &mut clipboard, false), Edit::Moved);
        assert_eq!(state.caret, 6);
        assert!(state.selected().is_none());

        // Home and End are the two ends of a single-line box.
        assert_eq!(state.key(Key::ShiftHome, &mut clipboard, false), Edit::Moved);
        assert_eq!(state.selected_text().as_deref(), Some("hello "));
        assert_eq!(state.key(Key::End, &mut clipboard, false), Edit::Moved);
        assert_eq!(state.caret, 11);
        assert!(state.selected().is_none());
    }

    /// Cut takes the words away, paste puts them in at the caret, and delete
    /// is a backspace facing the other way.
    #[test]
    fn the_clipboard_chords_act_on_the_run() {
        let mut clipboard = Clipboard::default();
        let mut state = box_of("hello world", 0);

        state.select_all();
        assert_eq!(state.key(Key::Cut, &mut clipboard, false), Edit::Changed);
        assert_eq!(state.value, "");
        assert_eq!(clipboard.text(), Some("hello world"));

        assert_eq!(state.key(Key::Paste, &mut clipboard, false), Edit::Changed);
        assert_eq!(state.value, "hello world");
        assert_eq!(state.caret, 11);

        // Paste over a run replaces it.
        state.press(0);
        assert!(state.drag(5));
        clipboard.set_text("goodbye");
        assert_eq!(state.key(Key::Paste, &mut clipboard, false), Edit::Changed);
        assert_eq!(state.value, "goodbye world");

        // Delete eats forward; backspace eats back; neither runs off the end.
        let mut state = box_of("ab", 1);
        assert_eq!(state.key(Key::Delete, &mut clipboard, false), Edit::Changed);
        assert_eq!(state.value, "a");
        assert_eq!(state.key(Key::Delete, &mut clipboard, false), Edit::Ignored);
        assert_eq!(state.key(Key::Backspace, &mut clipboard, false), Edit::Changed);
        assert_eq!(state.value, "");
        assert_eq!(state.key(Key::Backspace, &mut clipboard, false), Edit::Ignored);
    }

    /// Up and down mean a line only where there are lines to move between.
    /// A field has none, and the vertical arrows in one belong to whatever is
    /// around it: in a spreadsheet's cell, to the grid.
    #[test]
    fn the_vertical_arrows_are_only_a_multiline_boxs() {
        let mut clipboard = Clipboard::default();
        let mut state = box_of("one\ntwo", 5);
        assert_eq!(state.key(Key::Up, &mut clipboard, true), Edit::Moved);
        assert_eq!(state.caret, 1);
        assert_eq!(state.key(Key::Up, &mut clipboard, false), Edit::Ignored);
    }

    /// Two presses close together in the same place are a double click; far
    /// apart in either time or space they are two presses.
    #[test]
    fn a_double_click_is_close_in_time_and_place() {
        let mut presses = MultiPress::default();
        assert_eq!(presses.press(10, 10), 1);
        assert_eq!(presses.press(10, 10), 2);
        assert_eq!(presses.press(11, 10), 3);
        // A fourth starts again rather than selecting something nobody has a
        // name for.
        assert_eq!(presses.press(10, 10), 1);
        // Somewhere else is a fresh press.
        assert_eq!(presses.press(200, 10), 1);
    }
}
