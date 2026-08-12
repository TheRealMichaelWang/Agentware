//! The Human-Agent Interface Manager.
//!
//! Owns the display, the keyboard and the mouse, and is the only process in
//! Agentware that touches any of them. Everything else describes what it wants
//! shown as AWML and receives events back.
//!
//! Milestone 2: the display is up and there is a software rasterizer on top of
//! it. What is drawn is still a fixed specimen sheet rather than anything a
//! client asked for.

mod drm;
mod paint;
mod specimen;

use std::io::Write;
use std::os::unix::net::UnixStream;

use awproto::{ROLE_HAIMANAGER, SOCKET_PATH, encode, read_frame};

fn main() {
    log("starting");

    // Registering marks the compositor ready, which is what releases anything
    // waiting on it. Do it before touching hardware so a slow bring-up does not
    // look like a failure to start.
    match register() {
        Ok(()) => log("registered with the supervisor"),
        // Not fatal. Running outside a booted system is how this gets debugged.
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

    let mut canvas = paint::Canvas::new(width, height);
    specimen::draw(&mut canvas);

    if let Err(err) = display.present_canvas(&canvas) {
        log(&format!("FATAL: could not present: {err}"));
        std::process::exit(1);
    }
    log("specimen sheet presented");

    // Nothing to do yet, but the display only lives as long as this process: the
    // `Display` drop handler restores the previous mode, so exiting here would
    // blank the screen we just brought up.
    park();
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

/// Sleep forever without burning a core.
fn park() -> ! {
    loop {
        // SAFETY: pause takes no arguments and only ever blocks.
        unsafe { libc::pause() };
    }
}

/// Log to the kernel ring buffer, which reaches the serial console and `dmesg`.
///
/// The display is about to be taken over, so writing progress to it is not an
/// option, and stdout goes to a console this process is in the middle of
/// replacing.
fn log(message: &str) {
    let line = format!("<6>haimanager: {message}\n");
    if let Ok(mut kmsg) = std::fs::OpenOptions::new().write(true).open("/dev/kmsg") {
        let _ = kmsg.write_all(line.as_bytes());
    } else {
        eprintln!("haimanager: {message}");
    }
}
