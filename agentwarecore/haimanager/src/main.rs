//! The Human-Agent Interface Manager.
//!
//! Owns the display, the keyboard and the mouse, and is the only process in
//! Agentware that touches any of them. Everything else describes what it wants
//! shown as AWML and receives events back.
//!
//! Milestone 1 is display bring-up only: take the card, set a mode, and paint a
//! test pattern that makes rendering mistakes visible at a glance.

mod drm;

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

    let fb = display.framebuffer();
    log(&format!(
        "display up: {}x{} mode {:?} stride {}px",
        fb.width(),
        fb.height(),
        display.mode_name(),
        display.framebuffer().stride()
    ));

    test_pattern(display.framebuffer());
    if let Err(err) = display.flush() {
        log(&format!("FATAL: could not flush the framebuffer: {err}"));
        std::process::exit(1);
    }
    log("test pattern painted");

    // Nothing to do yet, but the display only lives as long as this process: the
    // `Display` drop handler restores the previous mode, so exiting here would
    // blank the screen we just brought up.
    park();
}

/// A pattern chosen so the three ways this can go wrong are visible instantly.
///
/// * Four colour bars, in order red, green, blue, white. Wrong channel order
///   shows up as the wrong colour in the wrong place, which a gradient would
///   hide.
/// * A one-pixel white border. If pitch is being confused with width, the right
///   edge shears diagonally instead of running straight down.
/// * A white diagonal from corner to corner, which is straight only if the
///   geometry is right.
fn test_pattern(fb: &mut drm::Framebuffer) {
    const RED: u32 = 0x00FF_0000;
    const GREEN: u32 = 0x0000_FF00;
    const BLUE: u32 = 0x0000_00FF;
    const WHITE: u32 = 0x00FF_FFFF;

    let (width, height) = (fb.width(), fb.height());

    fb.fill(|x, y| {
        if x == 0 || y == 0 || x == width - 1 || y == height - 1 {
            return WHITE;
        }
        if x * height == y * width {
            return WHITE;
        }
        match x * 4 / width {
            0 => RED,
            1 => GREEN,
            2 => BLUE,
            _ => WHITE,
        }
    });
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
