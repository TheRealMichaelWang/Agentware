//! The status strip along the bottom of the screen.
//!
//! Not part of the interface Agentware is meant to have; it is the thing that
//! makes the client protocol checkable from a screenshot. Graphics cannot be
//! verified from a serial log, so what a milestone claims has to be visible.
//!
//! It shows every attached connection with the version of the tree the
//! compositor is holding for it, which client input is going to, and the last
//! thing that happened. Between them those answer the questions this milestone
//! raises: did the handoff arrive, did the tree parse, did the resend cost a
//! repaint, and does the caret sit where it was left.
//!
//! It goes away when the navigation bar arrives in milestone 6.

use crate::client::Clients;
use crate::paint::font::{Family, Fonts, Style};
use crate::paint::{Canvas, Rect};
use crate::ui;

pub const HEIGHT: i32 = 96;

fn mono(size: f32) -> Style {
    Style { family: Family::Mono, size, ..Style::default() }
}

pub fn draw(canvas: &mut Canvas, fonts: &Fonts, clients: &Clients) {
    let panel = Rect::new(0, canvas.height() - HEIGHT, canvas.width(), HEIGHT);
    canvas.fill_rect(panel, ui::SURFACE);
    canvas.stroke_rect(panel, 1, ui::BORDER);

    let x = 16;
    let mut y = panel.y + 12;
    let style = mono(14.0);

    let Some(front) = clients.front() else {
        canvas.draw_text(fonts, "no clients attached", x, y, &style, ui::MUTED);
        return;
    };

    // The front client is starred, because which one input reaches is otherwise
    // impossible to tell from a still image.
    let mut roster = String::new();
    for (at, client) in clients.iter().enumerate() {
        if at > 0 {
            roster.push_str("   ");
        }
        if at == clients.front_index() {
            roster.push('*');
        }
        roster.push_str(&format!("{} v{}", client.label(), client.version()));
    }
    canvas.draw_text(fonts, &roster, x, y, &style, ui::ACCENT);
    y += 22;

    canvas.draw_text(fonts, &format!("last: {}", front.note), x, y, &style, ui::TEXT);
    y += 22;

    canvas.draw_text(fonts, &front.focus_summary(), x, y, &style, ui::MUTED);
}

/// What the screen says while nothing has drawn into it yet.
pub fn draw_idle(canvas: &mut Canvas, fonts: &Fonts, clients: &Clients) {
    canvas.clear(ui::BACKGROUND);

    let waiting = match clients.front() {
        Some(client) => format!("{} is connected but has not rendered yet", client.label()),
        None => "waiting for a client".to_owned(),
    };

    let style = mono(18.0);
    let width = fonts.measure(&waiting, &style);
    canvas.draw_text(
        fonts,
        &waiting,
        (canvas.width() - width) / 2,
        canvas.height() / 2,
        &style,
        ui::MUTED,
    );
}
