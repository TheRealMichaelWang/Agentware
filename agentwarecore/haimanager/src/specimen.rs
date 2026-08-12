//! A live specimen sheet, for checking the rasterizer and input by eye.
//!
//! It shows every drawing primitive, the whole font, and a running readout of
//! what input has arrived. A font with one wrong glyph looks fine until the day
//! that letter matters, and an input path that decodes shift wrongly looks fine
//! until someone types a capital.
//!
//! This exists only until real clients are drawing. It is the rendering
//! equivalent of the supervisor's boot report.

use crate::cursor;
use crate::input::{Button, Event, Key};
use crate::paint::{Canvas, Color, Rect, rgb};

const BACKGROUND: Color = rgb(0x10, 0x12, 0x18);
const PANEL: Color = rgb(0x1c, 0x20, 0x2a);
const BORDER: Color = rgb(0x3a, 0x42, 0x54);
const TEXT: Color = rgb(0xe6, 0xe9, 0xef);
const MUTED: Color = rgb(0x8a, 0x93, 0xa6);
const ACCENT: Color = rgb(0x4f, 0x9c, 0xf5);
const DANGER: Color = rgb(0xe0, 0x5a, 0x5a);
const OK: Color = rgb(0x5a, 0xc8, 0x8a);

/// What input has been seen so far.
#[derive(Default)]
pub struct State {
    pointer: (i32, i32),
    buttons: [bool; 3],
    /// Text typed since boot, so shift, caps lock and the punctuation rows can
    /// all be checked by reading one line back.
    typed: String,
    last_key: Option<String>,
    scroll: i32,
    events: u64,
}

impl State {
    pub fn record(&mut self, event: Event) {
        self.events += 1;

        match event {
            Event::PointerMoved { x, y } => self.pointer = (x, y),

            Event::ButtonPressed { button, x, y } => {
                self.pointer = (x, y);
                self.buttons[index_of(button)] = true;
            }
            Event::ButtonReleased { button, x, y } => {
                self.pointer = (x, y);
                self.buttons[index_of(button)] = false;
            }

            Event::KeyPressed(key) => {
                self.last_key = Some(describe(key));
                match key {
                    Key::Char(c) => self.typed.push(c),
                    Key::Enter => self.typed.push_str(" | "),
                    Key::Backspace => {
                        self.typed.pop();
                    }
                    _ => {}
                }
                // Bounded, because this is a demo surface and the line it is
                // drawn on has an end.
                if self.typed.len() > 64 {
                    let overflow = self.typed.len() - 64;
                    self.typed.drain(..overflow);
                }
            }

            Event::KeyReleased(_) => {}
            Event::Scrolled(delta) => self.scroll += delta,
        }
    }
}

fn index_of(button: Button) -> usize {
    match button {
        Button::Left => 0,
        Button::Middle => 1,
        Button::Right => 2,
    }
}

fn describe(key: Key) -> String {
    match key {
        Key::Char(c) => format!("Char({c:?})"),
        Key::Enter => "Enter".into(),
        Key::Backspace => "Backspace".into(),
        Key::Tab => "Tab".into(),
        Key::Escape => "Escape".into(),
        Key::Left => "Left".into(),
        Key::Right => "Right".into(),
        Key::Up => "Up".into(),
        Key::Down => "Down".into(),
        Key::Other(code) => format!("Other({code})"),
    }
}

pub fn draw(canvas: &mut Canvas, state: &State) {
    canvas.clear(BACKGROUND);

    let margin = 24;
    let mut y = margin;

    canvas.draw_text("AGENTWARE HAIMANAGER", margin, y, 4, TEXT);
    y += Canvas::text_height(4) + 8;
    y = line(canvas, "rasterizer and input specimen", margin, y, MUTED, 2);
    y += 16;

    y = input_readout(canvas, margin, y, state);
    y += 20;

    y = font_sheet(canvas, margin, y);
    y += 16;

    y = primitives(canvas, margin, y);
    y += 16;

    clipping(canvas, margin, y);
    cursors(canvas, 940, 320);
}

fn line(canvas: &mut Canvas, text: &str, x: i32, y: i32, color: Color, scale: i32) -> i32 {
    canvas.draw_text(text, x, y, scale, color);
    y + Canvas::text_height(scale) + 6
}

/// Everything the input path produced, so a wrong keymap or a stuck button is
/// visible without attaching a debugger to a machine that has no shell.
fn input_readout(canvas: &mut Canvas, x: i32, y: i32, state: &State) -> i32 {
    let y = line(canvas, "INPUT", x, y, ACCENT, 2);

    let panel = Rect::new(x, y, 880, 108);
    canvas.fill_rect(panel, PANEL);
    canvas.stroke_rect(panel, 1, BORDER);

    let (px, py) = state.pointer;
    let mut row = y + 12;
    let label = x + 12;
    let value = x + 200;

    canvas.draw_text("pointer", label, row, 2, MUTED);
    canvas.draw_text(&format!("{px}, {py}"), value, row, 2, TEXT);
    row += 20;

    canvas.draw_text("buttons", label, row, 2, MUTED);
    for (index, name) in ["left", "middle", "right"].iter().enumerate() {
        let held = state.buttons[index];
        canvas.draw_text(
            name,
            value + index as i32 * 90,
            row,
            2,
            if held { OK } else { MUTED },
        );
    }
    row += 20;

    canvas.draw_text("last key", label, row, 2, MUTED);
    canvas.draw_text(
        state.last_key.as_deref().unwrap_or("none"),
        value,
        row,
        2,
        TEXT,
    );
    row += 20;

    canvas.draw_text("typed", label, row, 2, MUTED);
    canvas.draw_text(&state.typed, value, row, 2, OK);
    row += 20;

    canvas.draw_text("events / scroll", label, row, 2, MUTED);
    canvas.draw_text(&format!("{} / {}", state.events, state.scroll), value, row, 2, TEXT);

    y + panel.h + 8
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

/// Fills and outlines, in the arrangement a real interface is made of.
fn primitives(canvas: &mut Canvas, x: i32, y: i32) -> i32 {
    let y = line(canvas, "PRIMITIVES", x, y, ACCENT, 2);

    let panel = Rect::new(x, y, 520, 60);
    canvas.fill_rect(panel, PANEL);
    canvas.stroke_rect(panel, 1, BORDER);

    button(canvas, Rect::new(x + 16, y + 14, 120, 32), "Send", ACCENT, TEXT);
    button(canvas, Rect::new(x + 152, y + 14, 120, 32), "Discard", DANGER, TEXT);
    // A disabled control: same parts, distinguished only by colour, exactly as
    // a real `disabled` button will be.
    button(canvas, Rect::new(x + 288, y + 14, 120, 32), "Locked", PANEL, MUTED);

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

/// Text deliberately drawn past the edge of its region. If clipping works the
/// sentence is cut mid-glyph; if it does not, it runs across the screen, which
/// is the failure that would otherwise appear later as one app drawing over
/// another.
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

/// Both pointers side by side, so the difference between them can be judged
/// rather than assumed.
fn cursors(canvas: &mut Canvas, x: i32, y: i32) {
    let y = line(canvas, "POINTERS", x, y, ACCENT, 2);

    let panel = Rect::new(x, y, 300, 90);
    canvas.fill_rect(panel, PANEL);
    canvas.stroke_rect(panel, 1, BORDER);

    canvas.draw_text("human", x + 60, y + 24, 2, MUTED);
    cursor::draw(canvas, x + 24, y + 16, cursor::Kind::Human);

    canvas.draw_text("agent", x + 200, y + 24, 2, MUTED);
    cursor::draw(canvas, x + 164, y + 16, cursor::Kind::Agent);
}
