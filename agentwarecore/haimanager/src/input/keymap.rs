//! Linux key codes to characters, US layout.
//!
//! Only the main typing block is mapped. Everything outside it becomes
//! [`Key::Other`], which is enough for a compositor: the keys that need naming
//! are the ones that move a cursor or edit text, and the rest are for
//! applications to interpret if they ever care.
//!
//! Layouts beyond US are a real gap, deliberately left until there is a
//! settings surface to choose one from.

use super::Key;

/// Unshifted and shifted characters, indexed by key code.
///
/// A null means the code produces no text: a modifier, a function key, or
/// something handled by name below.
#[rustfmt::skip]
const PRINTABLE: [(char, char); 59] = [
    ('\0', '\0'), // 0  reserved
    ('\0', '\0'), // 1  escape
    ('1', '!'), ('2', '@'), ('3', '#'), ('4', '$'), ('5', '%'),
    ('6', '^'), ('7', '&'), ('8', '*'), ('9', '('), ('0', ')'),
    ('-', '_'), ('=', '+'),
    ('\0', '\0'), // 14 backspace
    ('\0', '\0'), // 15 tab
    ('q', 'Q'), ('w', 'W'), ('e', 'E'), ('r', 'R'), ('t', 'T'),
    ('y', 'Y'), ('u', 'U'), ('i', 'I'), ('o', 'O'), ('p', 'P'),
    ('[', '{'), (']', '}'),
    ('\0', '\0'), // 28 enter
    ('\0', '\0'), // 29 left control
    ('a', 'A'), ('s', 'S'), ('d', 'D'), ('f', 'F'), ('g', 'G'),
    ('h', 'H'), ('j', 'J'), ('k', 'K'), ('l', 'L'),
    (';', ':'), ('\'', '"'), ('`', '~'),
    ('\0', '\0'), // 42 left shift
    ('\\', '|'),
    ('z', 'Z'), ('x', 'X'), ('c', 'C'), ('v', 'V'), ('b', 'B'),
    ('n', 'N'), ('m', 'M'),
    (',', '<'), ('.', '>'), ('/', '?'),
    ('\0', '\0'), // 54 right shift
    ('*', '*'),   // 55 keypad asterisk
    ('\0', '\0'), // 56 left alt
    (' ', ' '),   // 57 space
    ('\0', '\0'), // 58 caps lock
];

const KEY_ESC: u16 = 1;
const KEY_BACKSPACE: u16 = 14;
const KEY_TAB: u16 = 15;
const KEY_ENTER: u16 = 28;
const KEY_KPENTER: u16 = 96;
const KEY_UP: u16 = 103;
const KEY_LEFT: u16 = 105;
const KEY_RIGHT: u16 = 106;
const KEY_DOWN: u16 = 108;

/// Turn a key code into a key, applying shift and caps lock.
pub fn decode(code: u16, shift: bool, caps: bool) -> Key {
    match code {
        KEY_ESC => return Key::Escape,
        KEY_BACKSPACE => return Key::Backspace,
        KEY_TAB => return Key::Tab,
        KEY_ENTER | KEY_KPENTER => return Key::Enter,
        KEY_UP => return Key::Up,
        KEY_DOWN => return Key::Down,
        KEY_LEFT => return Key::Left,
        KEY_RIGHT => return Key::Right,
        _ => {}
    }

    let Some(&(lower, upper)) = PRINTABLE.get(code as usize) else {
        return Key::Other(code);
    };
    if lower == '\0' {
        return Key::Other(code);
    }

    // Caps lock applies to letters only, which is why it cannot simply be
    // folded into shift: shifted `1` is `!` whether or not caps lock is on.
    let upper_case = if lower.is_ascii_alphabetic() { shift ^ caps } else { shift };
    Key::Char(if upper_case { upper } else { lower })
}
