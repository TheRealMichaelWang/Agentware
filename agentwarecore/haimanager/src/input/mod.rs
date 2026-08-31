//! Keyboard and pointer, read straight from evdev.
//!
//! There is no libinput and no X server here. `/dev/input/event*` delivers
//! fixed-size records describing what the hardware did, and this module turns
//! them into the events the rest of the compositor reasons about.
//!
//! Devices are not probed for their capabilities. Every `event*` node is opened
//! and interpreted by what it actually reports: relative motion means a
//! pointer, key codes below the button range mean a keyboard. A device that
//! does both works without a special case, and one that appears later is picked
//! up by reopening rather than by a hotplug protocol we would have to invent.

mod keymap;

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::{AsFd, BorrowedFd};
use std::path::PathBuf;

/// A key, named where naming it helps and numbered where it does not.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    /// A key that produces text. The character already accounts for shift.
    Char(char),
    Enter,
    /// Enter with shift held: the one modifier chord that means something,
    /// because chat convention says Enter sends and Shift+Enter breaks the
    /// line, and a compositor that cannot tell them apart cannot offer it.
    ShiftEnter,
    Backspace,
    Tab,
    Escape,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    Delete,
    /// The same four, and the two ends of a line, with shift held: the
    /// keyboard's way of dragging out a selection.
    ///
    /// Named rather than carried as a modifier for the reason every other
    /// chord is: nothing downstream should be reasoning about which keys are
    /// down, and an application must never be able to find out. What the
    /// compositor does with them never leaves it.
    ShiftLeft,
    ShiftRight,
    ShiftUp,
    ShiftDown,
    ShiftHome,
    ShiftEnd,
    /// The clipboard chords, named by what they do rather than by the keys
    /// that produce them.
    ///
    /// A modifier never leaves this module. Control is tracked here exactly
    /// as shift is, and what comes out is the intention, so nothing
    /// downstream reasons about held keys and no application can be told one
    /// was held: cut, copy and paste happen against the compositor's own copy
    /// of a text control, and what an application hears is the value it ended
    /// up with.
    Copy,
    Cut,
    Paste,
    SelectAll,
    Other(u16),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Button {
    Left,
    Right,
    Middle,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Event {
    PointerMoved { x: i32, y: i32 },
    ButtonPressed { button: Button, x: i32, y: i32 },
    ButtonReleased { button: Button, x: i32, y: i32 },
    KeyPressed(Key),
    KeyReleased(Key),
    /// Positive scrolls up, away from the user. Carries the pointer position,
    /// because which container scrolls is decided by what the wheel is over.
    Scrolled { delta: i32, x: i32, y: i32 },
}

/// Record layout of `struct input_event` on 64-bit Linux.
///
/// Declared here rather than taken from a crate for the same reason as the DRM
/// structures: it is stable uapi, and reading it wrong produces nonsense input
/// rather than an error.
#[repr(C)]
#[derive(Clone, Copy)]
struct Record {
    seconds: i64,
    microseconds: i64,
    kind: u16,
    code: u16,
    value: i32,
}

const RECORD_SIZE: usize = size_of::<Record>();

// Event types.
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const EV_ABS: u16 = 0x03;

// Relative axes.
const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const REL_WHEEL: u16 = 0x08;

// Absolute axes, reported by tablet devices.
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;

/// The range QEMU's tablets report absolute positions in. Fixed by QEMU rather
/// than negotiated; a physical tablet would need `EVIOCGABS` to learn its real
/// range, and this constant is the honest record of the assumption.
const QEMU_ABS_MAX: i32 = 32767;

// Buttons live above the keyboard's range in the same code space as keys.
const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;

const KEY_LEFTCTRL: u16 = 29;
const KEY_RIGHTCTRL: u16 = 97;
const KEY_LEFTSHIFT: u16 = 42;
const KEY_RIGHTSHIFT: u16 = 54;
const KEY_CAPSLOCK: u16 = 58;

pub struct Input {
    devices: Vec<File>,
    /// Which nodes are already open, so a rescan adds only what is new.
    opened: HashSet<PathBuf>,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    shift: bool,
    ctrl: bool,
    caps: bool,
}

impl Input {
    /// Start with the pointer in the middle of the screen and no devices; call
    /// [`Input::rescan`] to pick them up.
    pub fn new(width: i32, height: i32) -> Self {
        Self {
            devices: Vec::new(),
            opened: HashSet::new(),
            x: width / 2,
            y: height / 2,
            width,
            height,
            shift: false,
            ctrl: false,
            caps: false,
        }
    }

    /// Open any `event*` node not already open, and return the indices of the
    /// ones just added so the caller can watch them.
    ///
    /// Devices do not all exist when the compositor starts. On a QEMU guest the
    /// PS/2 mouse is probed roughly 300ms after `/dev/input` first has entries
    /// in it, so enumerating once at startup finds a keyboard and no pointer.
    /// The same is true of anything hotplugged later. Rescanning is a `readdir`
    /// and costs nothing at the rate it runs.
    pub fn rescan(&mut self) -> Vec<usize> {
        let mut added = Vec::new();

        let Ok(entries) = std::fs::read_dir("/dev/input") else {
            return added;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if !name.starts_with("event") || self.opened.contains(&path) {
                continue;
            }

            // Read-only: the compositor consumes input, it never injects any.
            let Ok(device) = OpenOptions::new().read(true).open(&path) else {
                // Not fatal, and not remembered: a node that is not ready yet
                // should be retried on the next pass.
                continue;
            };
            if set_nonblocking(device.as_fd()).is_err() {
                continue;
            }

            self.opened.insert(path);
            added.push(self.devices.len());
            self.devices.push(device);
        }

        added
    }

    pub fn device(&self, index: usize) -> Option<BorrowedFd<'_>> {
        self.devices.get(index).map(|device| device.as_fd())
    }

    pub fn pointer(&self) -> (i32, i32) {
        (self.x, self.y)
    }

    /// Drain one device and translate whatever it had to say.
    ///
    /// `index` is a position in [`Input::devices`], which is how the caller
    /// maps an epoll wakeup back to a device.
    pub fn read_device(&mut self, index: usize) -> Vec<Event> {
        let mut events = Vec::new();
        let mut buffer = [0u8; RECORD_SIZE * 32];

        loop {
            let read = {
                let Some(device) = self.devices.get_mut(index) else {
                    return events;
                };
                match device.read(&mut buffer) {
                    Ok(0) => return events,
                    Ok(n) => n,
                    Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                    // WouldBlock is the normal end of a burst.
                    Err(_) => return events,
                }
            };

            for chunk in buffer[..read].chunks_exact(RECORD_SIZE) {
                // SAFETY: the kernel guarantees each RECORD_SIZE-byte chunk on
                // an evdev node is one `struct input_event`.
                let record: Record =
                    unsafe { std::ptr::read_unaligned(chunk.as_ptr().cast()) };
                self.translate(record, &mut events);
            }

            if read < buffer.len() {
                return events;
            }
        }
    }

    fn translate(&mut self, record: Record, events: &mut Vec<Event>) {
        match record.kind {
            // A tablet says where the pointer *is*, not how it moved. This is
            // what keeps the host cursor and the guest cursor as one thing: the
            // host hands over its position and the compositor draws exactly
            // there, instead of integrating deltas that drift from the moment
            // the window is scaled or the pointer leaves and re-enters.
            EV_ABS => match record.code {
                ABS_X => {
                    self.x = (record.value.clamp(0, QEMU_ABS_MAX) * (self.width - 1)
                        / QEMU_ABS_MAX)
                        .clamp(0, self.width - 1);
                    events.push(Event::PointerMoved { x: self.x, y: self.y });
                }
                ABS_Y => {
                    self.y = (record.value.clamp(0, QEMU_ABS_MAX) * (self.height - 1)
                        / QEMU_ABS_MAX)
                        .clamp(0, self.height - 1);
                    events.push(Event::PointerMoved { x: self.x, y: self.y });
                }
                _ => {}
            },

            EV_REL => match record.code {
                REL_X => {
                    // Clamping rather than wrapping: a pointer that leaves one
                    // edge and appears at the other is disorienting, and the
                    // agent's fake cursor shares this coordinate space.
                    self.x = (self.x + record.value).clamp(0, self.width - 1);
                    events.push(Event::PointerMoved { x: self.x, y: self.y });
                }
                REL_Y => {
                    self.y = (self.y + record.value).clamp(0, self.height - 1);
                    events.push(Event::PointerMoved { x: self.x, y: self.y });
                }
                REL_WHEEL => events.push(Event::Scrolled {
                    delta: record.value,
                    x: self.x,
                    y: self.y,
                }),
                _ => {}
            },

            EV_KEY => {
                let pressed = record.value != 0;

                if let Some(button) = button_for(record.code) {
                    let (x, y) = (self.x, self.y);
                    events.push(if pressed {
                        Event::ButtonPressed { button, x, y }
                    } else {
                        Event::ButtonReleased { button, x, y }
                    });
                    return;
                }

                // Modifiers change how later keys decode, so they are tracked
                // rather than reported. Autorepeat (value 2) counts as held.
                match record.code {
                    KEY_LEFTSHIFT | KEY_RIGHTSHIFT => {
                        self.shift = pressed;
                        return;
                    }
                    KEY_LEFTCTRL | KEY_RIGHTCTRL => {
                        self.ctrl = pressed;
                        return;
                    }
                    KEY_CAPSLOCK => {
                        if record.value == 1 {
                            self.caps = !self.caps;
                        }
                        return;
                    }
                    _ => {}
                }

                let key = keymap::decode(record.code, self.shift, self.caps, self.ctrl);
                events.push(if pressed {
                    Event::KeyPressed(key)
                } else {
                    Event::KeyReleased(key)
                });
            }

            // EV_SYN and everything else carry no information we act on.
            _ => {}
        }
    }
}

fn button_for(code: u16) -> Option<Button> {
    match code {
        BTN_LEFT => Some(Button::Left),
        BTN_RIGHT => Some(Button::Right),
        BTN_MIDDLE => Some(Button::Middle),
        _ => None,
    }
}

/// Input must never block the compositor: a device with nothing to say would
/// otherwise stall every other event source behind it.
fn set_nonblocking(fd: BorrowedFd<'_>) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: reading and then setting the file status flags of a descriptor we
    // own, which is what fcntl is for.
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: as above.
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
