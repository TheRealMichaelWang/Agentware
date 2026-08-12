//! The Human-Agent Interface Manager.
//!
//! Owns the display, the keyboard and the mouse, and is the only process in
//! Agentware that touches any of them. Everything else describes what it wants
//! shown as AWML and receives events back.
//!
//! Milestone 3: the display is up, there is a rasterizer, and input is read
//! from evdev and drawn. What is shown is still generated here rather than by
//! any client.

mod cursor;
mod drm;
mod input;
mod paint;
mod specimen;

use std::io::Write;
use std::os::unix::net::UnixStream;

use awproto::{ROLE_HAIMANAGER, SOCKET_PATH, encode, read_frame};
use rustix::event::epoll;

use input::Input;
use paint::Canvas;

/// epoll tokens. Input devices take one each, offset by their index.
const TOKEN_INPUT_BASE: u64 = 0x100;

fn main() {
    log("starting");

    match register() {
        Ok(()) => log("registered with the supervisor"),
        Err(err) => log(&format!("not registered ({err}); continuing standalone")),
    }

    let mut display = match drm::Display::open() {
        Ok(display) => display,
        Err(err) => {
            log(&format!("FATAL: could not bring up a display: {err}"));
            std::process::exit(1);
        }
    };

    let (width, height) = {
        let fb = display.framebuffer();
        (fb.width(), fb.height())
    };
    log(&format!("display up: {width}x{height} mode {:?}", display.mode_name()));

    let mut input = Input::new(width as i32, height as i32);

    let mut canvas = Canvas::new(width, height);
    let mut state = specimen::State::default();

    // Paint once before waiting, so a machine with no input at all still shows
    // something rather than a blank screen.
    redraw(&mut display, &mut canvas, &input, &state);

    run(&mut display, &mut canvas, &mut input, &mut state);
}

/// Wait for input and repaint when something changes.
///
/// Every device is an epoll source, so the process sleeps whenever nothing is
/// happening. Repainting is driven by events rather than by a frame clock: a
/// desktop that is not being touched should cost nothing.
fn run(
    display: &mut drm::Display,
    canvas: &mut Canvas,
    input: &mut Input,
    state: &mut specimen::State,
) -> ! {
    let epoll = match epoll::create(epoll::CreateFlags::CLOEXEC) {
        Ok(epoll) => epoll,
        Err(err) => {
            log(&format!("FATAL: could not create epoll: {err}"));
            park();
        }
    };

    let mut events = [epoll::Event {
        flags: epoll::EventFlags::empty(),
        data: epoll::EventData::new_u64(0),
    }; 16];

    // Devices are not all present at startup, so the loop wakes periodically to
    // look for new ones rather than enumerating once and hoping.
    let rescan_every = rustix::event::Timespec { tv_sec: 1, tv_nsec: 0 };

    loop {
        for index in input.rescan() {
            let Some(device) = input.device(index) else { continue };
            match epoll::add(
                &epoll,
                device,
                epoll::EventData::new_u64(TOKEN_INPUT_BASE + index as u64),
                epoll::EventFlags::IN,
            ) {
                Ok(()) => log(&format!("input: watching device {index}")),
                Err(err) => log(&format!("could not watch input device {index}: {err}")),
            }
        }

        let count = match epoll::wait(&epoll, &mut events, Some(&rescan_every)) {
            Ok(count) => count,
            Err(rustix::io::Errno::INTR) => continue,
            Err(err) => {
                log(&format!("FATAL: epoll wait failed: {err}"));
                park();
            }
        };

        let mut dirty = false;
        for event in &events[..count] {
            let token = event.data.u64();
            if token < TOKEN_INPUT_BASE {
                continue;
            }

            let index = (token - TOKEN_INPUT_BASE) as usize;
            for event in input.read_device(index) {
                state.record(event);
                dirty = true;
            }
        }

        if dirty {
            redraw(display, canvas, input, state);
        }
    }
}

fn redraw(
    display: &mut drm::Display,
    canvas: &mut Canvas,
    input: &Input,
    state: &specimen::State,
) {
    specimen::draw(canvas, state);

    let (x, y) = input.pointer();
    cursor::draw(canvas, x, y, cursor::Kind::Human);

    if let Err(err) = display.present_canvas(canvas) {
        log(&format!("could not present: {err}"));
    }
}

fn register() -> Result<(), String> {
    let mut stream =
        UnixStream::connect(SOCKET_PATH).map_err(|err| format!("connect: {err}"))?;

    stream
        .write_all(&encode(&["register", ROLE_HAIMANAGER]))
        .map_err(|err| format!("write: {err}"))?;

    let reply = read_frame(&mut stream).map_err(|err| format!("reply: {err}"))?;
    if reply.first().map(String::as_str) != Some("ok") {
        return Err(format!("refused: {}", reply.join(" ")));
    }

    // The connection has to outlive this function: the supervisor treats a
    // closed connection as the role going away, and would stop considering the
    // compositor ready the moment we hung up.
    std::mem::forget(stream);
    Ok(())
}

/// Sleep forever without burning a core. Only reached on a fatal error, where
/// exiting would restore the console and destroy the message explaining why.
fn park() -> ! {
    loop {
        // SAFETY: pause takes no arguments and only ever blocks.
        unsafe { libc::pause() };
    }
}

/// Log to the kernel ring buffer, which reaches the serial console and `dmesg`.
///
/// The display is owned by this process, so writing progress to it is not an
/// option, and stdout goes to a console it is in the middle of replacing.
fn log(message: &str) {
    let line = format!("<6>haimanager: {message}\n");
    if let Ok(mut kmsg) = std::fs::OpenOptions::new().write(true).open("/dev/kmsg") {
        let _ = kmsg.write_all(line.as_bytes());
    } else {
        eprintln!("haimanager: {message}");
    }
}
