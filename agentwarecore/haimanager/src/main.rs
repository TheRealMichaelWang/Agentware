//! The Human-Agent Interface Manager.
//!
//! Owns the display, the keyboard and the mouse, and is the only process in
//! Agentware that touches any of them. Everything else describes what it wants
//! shown as AWML and receives events back.
//!
//! Trees arrive from real processes over descriptors the supervisor pushes here,
//! each tagged with the workspace it belongs to. `screen.rs` decides what is
//! where; `client.rs` holds one tree and its ephemeral state per connection.
//!
//! This file is the event loop and nothing else. Everything it reacts to is a
//! file descriptor, so the process sleeps whenever nothing is happening.

mod awml;
mod client;
mod cursor;
mod document;
mod drm;
mod input;
mod paint;
mod screen;
mod ui;

use std::collections::VecDeque;
use std::io::{IoSliceMut, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, AsRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

use awproto::{Decoder, ROLE_HAIMANAGER, SOCKET_PATH, encode, read_frame};
use rustix::event::epoll;
use rustix::net::{RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, recvmsg};

use input::{Event, Input};
use paint::font::Fonts;
use paint::{Canvas, Rect};
use screen::{CompositorKey, Screen};

/// epoll tokens. The supervisor connection is fixed; input devices take one each
/// offset by their index, and client connections take one derived from their own
/// descriptor, which is unique for as long as it is open.
const TOKEN_SUPERVISOR: u64 = 1;
const TOKEN_INPUT_BASE: u64 = 0x100;
const TOKEN_CLIENT_BASE: u64 = 0x1000;

fn main() {
    log("starting");

    let supervisor = match register() {
        Ok(stream) => {
            log("registered with the supervisor");
            Some(stream)
        }
        Err(err) => {
            log(&format!("not registered ({err}); no clients can be handed over"));
            None
        }
    };

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

    let fonts = match Fonts::load() {
        Ok(fonts) => fonts,
        Err(err) => {
            log(&format!("FATAL: could not load fonts: {err}"));
            std::process::exit(1);
        }
    };

    let mut canvas = Canvas::new(width, height);
    let mut screen = Screen::new(Rect::new(0, 0, width as i32, height as i32));

    // Paint once before waiting, so a machine with nothing attached still shows
    // something rather than a blank screen.
    redraw(&mut display, &mut canvas, &fonts, &input, &mut screen);
    log(&format!("{} glyphs rasterized for the first frame", fonts.glyph_count()));

    run(&mut display, &mut canvas, &fonts, &mut input, &mut screen, supervisor);
}

/// Wait for something to happen and repaint when it does.
///
/// Every source is a file descriptor, so the process sleeps whenever nothing is
/// happening. Repainting is driven by events rather than by a frame clock: a
/// desktop that is not being touched should cost nothing, and an application
/// that resends a tree identical to the one on screen should cost nothing
/// either. The second of those is what the diff is for.
fn run(
    display: &mut drm::Display,
    canvas: &mut Canvas,
    fonts: &Fonts,
    input: &mut Input,
    screen: &mut Screen,
    supervisor: Option<UnixStream>,
) -> ! {
    let epoll = match epoll::create(epoll::CreateFlags::CLOEXEC) {
        Ok(epoll) => epoll,
        Err(err) => {
            log(&format!("FATAL: could not create epoll: {err}"));
            park();
        }
    };

    let mut handoffs = supervisor.map(Handoffs::new);
    if let Some(handoffs) = &handoffs
        && let Err(err) = epoll::add(
            &epoll,
            handoffs.borrow(),
            epoll::EventData::new_u64(TOKEN_SUPERVISOR),
            epoll::EventFlags::IN,
        )
    {
        log(&format!("could not watch the supervisor connection: {err}"));
    }

    let mut events = [epoll::Event {
        flags: epoll::EventFlags::empty(),
        data: epoll::EventData::new_u64(0),
    }; 16];

    // Devices are not all present at startup, so the loop wakes periodically to
    // look for new ones rather than enumerating once and hoping.
    let rescan_every = rustix::event::Timespec { tv_sec: 1, tv_nsec: 0 };

    // While the agent's cursor is travelling there is an animation to run, and
    // an animation is the one thing an event-driven loop cannot wait for. This
    // is the only case in which the compositor wakes without being asked to.
    let frame = rustix::event::Timespec { tv_sec: 0, tv_nsec: 16_000_000 };

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

        let waiting = if screen.wants_frame() { &frame } else { &rescan_every };
        let count = match epoll::wait(&epoll, &mut events, Some(waiting)) {
            Ok(count) => count,
            Err(rustix::io::Errno::INTR) => continue,
            Err(err) => {
                log(&format!("FATAL: epoll wait failed: {err}"));
                park();
            }
        };

        let mut dirty = screen.tick(fonts);
        for event in &events[..count] {
            let token = event.data.u64();

            match token {
                TOKEN_SUPERVISOR => {
                    let Some(inbox) = &mut handoffs else { continue };
                    let (arrivals, gone) = inbox.drain();

                    for (fields, fd) in arrivals {
                        dirty |= adopt(&epoll, screen, fonts, &fields, fd);
                    }

                    if gone {
                        // Nothing else can hand over a client, but the screen is
                        // still ours and the clients we already have still work.
                        log("the supervisor connection closed; no further handoffs");
                        let _ = epoll::delete(&epoll, inbox.borrow());
                        handoffs = None;
                    }
                }

                token if token >= TOKEN_CLIENT_BASE => {
                    let fd = (token - TOKEN_CLIENT_BASE) as RawFd;
                    let Some(progress) = screen.readable(fd, fonts) else { continue };

                    for line in progress.log {
                        log(&line);
                    }
                    dirty |= progress.dirty;

                    // Queries and intents. Only an agent connection produces
                    // any, and what it may see is decided by the workspace the
                    // supervisor said this descriptor belongs to.
                    if !progress.requests.is_empty() {
                        dirty |= screen.requests(fonts, fd, progress.requests);
                    }

                    // A first tree is worth printing whole: the reduced schema
                    // can then be read against the document that produced it,
                    // which is the claim the design makes about them.
                    if let Some(client) = screen.client_mut(fd)
                        && client.version() == 1
                        && progress.dirty
                        && let Some(view) = client.agent_view()
                    {
                        for line in view.lines() {
                            log(&format!("agent view | {line}"));
                        }
                    }

                    if progress.gone {
                        if let Some(client) = screen.client_mut(fd) {
                            let _ = epoll::delete(&epoll, client.borrow());
                        }
                        if let Some(label) = screen.remove(fd, fonts) {
                            log(&format!("{label} disconnected"));
                        }
                        dirty = true;
                    }
                }

                token if token >= TOKEN_INPUT_BASE => {
                    let index = (token - TOKEN_INPUT_BASE) as usize;
                    for event in input.read_device(index) {
                        dirty |= route(fonts, screen, event);
                    }
                }

                other => log(&format!("event on unknown epoll token {other}")),
            }
        }

        // Anything the compositor decided PID 1 should do goes out from here,
        // where blocking is survivable, rather than from inside a click handler.
        for request in screen.take_requests() {
            let fields: Vec<&str> = request.iter().map(String::as_str).collect();
            match &mut handoffs {
                Some(inbox) => inbox.request(&fields),
                None => log(&format!("cannot send {:?}: no supervisor", fields.join(" "))),
            }
        }
        for note in screen.take_notes() {
            log(&note);
        }

        // Any backlog a full socket left behind goes out now rather than waiting
        // for the next thing to happen to that client.
        screen.flush_all();

        for fd in screen.broken() {
            if let Some(client) = screen.client_mut(fd) {
                let _ = epoll::delete(&epoll, client.borrow());
            }
            if let Some(label) = screen.remove(fd, fonts) {
                log(&format!("{label} stopped reading its events and was dropped"));
            }
            dirty = true;
        }

        if dirty {
            redraw(display, canvas, fonts, input, screen);
        }
    }
}

/// Take a descriptor the supervisor pushed and start watching it.
fn adopt(
    epoll: &impl AsFd,
    screen: &mut Screen,
    fonts: &Fonts,
    fields: &[String],
    fd: Option<OwnedFd>,
) -> bool {
    let Some(fd) = fd else {
        log(&format!("handoff {:?} arrived with no descriptor", fields.join(" ")));
        return false;
    };

    let raw = fd.as_raw_fd();
    let label = match screen.attach(fonts, fields, fd) {
        Ok(label) => label,
        Err(err) => {
            log(&format!("refused a handoff: {err}"));
            return false;
        }
    };

    // The client owns the descriptor now, so it is borrowed back out of the
    // registry rather than kept here.
    let Some(client) = screen.client_mut(raw) else { return false };
    let watched = epoll::add(
        epoll,
        client.borrow(),
        epoll::EventData::new_u64(TOKEN_CLIENT_BASE + raw as u64),
        epoll::EventFlags::IN,
    );

    if let Err(err) = watched {
        log(&format!("could not watch {label}: {err}"));
        screen.remove(raw, fonts);
        return false;
    }

    log(&format!("{label} attached"));
    true
}

/// Send one input event where it belongs.
///
/// A few keys never reach a client at all. They are the compositor's own, in the
/// same way the navigation bar is: a workspace must not be able to swallow the
/// way out of itself.
fn route(fonts: &Fonts, screen: &mut Screen, event: Event) -> bool {
    if let Event::KeyPressed(key) = event
        && let Some(reserved) = screen::compositor_key(key)
    {
        return match reserved {
            CompositorKey::CycleWorkspace => screen.cycle(),
            CompositorKey::ToggleDebug => {
                screen.debug = !screen.debug;
                true
            }
            CompositorKey::TogglePane => screen.toggle_pane(fonts),
        };
    }

    screen.handle(fonts, event)
}

fn redraw(
    display: &mut drm::Display,
    canvas: &mut Canvas,
    fonts: &Fonts,
    input: &Input,
    screen: &mut Screen,
) {
    screen.draw(canvas, fonts, input.pointer());

    if let Err(err) = display.present_canvas(canvas) {
        log(&format!("could not present: {err}"));
    }
}

/// The supervisor connection, once registration is done with it.
///
/// After the reply to `register` it carries nothing but handoffs: a frame naming
/// what a process is, with that process's descriptor attached to the same
/// message. The two arrive together deliberately, so a descriptor is never
/// holding an unexplained connection.
/// One frame from the supervisor, with the descriptor it named if it named one.
type Handoff = (Vec<String>, Option<OwnedFd>);

struct Handoffs {
    stream: UnixStream,
    decoder: Decoder,
    /// Descriptors received but not yet matched to the frame that names them.
    /// A stream socket may coalesce, so pairing is by arrival order rather than
    /// by assuming one message per read.
    fds: VecDeque<OwnedFd>,
}

impl Handoffs {
    fn new(stream: UnixStream) -> Self {
        let _ = stream.set_nonblocking(true);
        Handoffs { stream, decoder: Decoder::default(), fds: VecDeque::new() }
    }

    fn borrow(&self) -> std::os::fd::BorrowedFd<'_> {
        self.stream.as_fd()
    }

    /// Ask the supervisor for something.
    ///
    /// Used for exactly one thing today: the stop button, which routes here
    /// rather than through the agentdesk so that it works when the agentdesk
    /// does not. The reply arrives back down the same connection and is logged
    /// with everything else.
    fn request(&mut self, fields: &[&str]) {
        if let Err(err) = self.stream.write_all(&encode(fields)) {
            log(&format!("could not reach the supervisor: {err}"));
        }
    }

    /// Everything that has arrived, and whether the supervisor hung up.
    fn drain(&mut self) -> (Vec<Handoff>, bool) {
        let mut gone = false;

        loop {
            let mut buf = [0u8; 4096];
            let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(4))];
            let mut control = RecvAncillaryBuffer::new(&mut space);

            match recvmsg(
                self.stream.as_fd(),
                &mut [IoSliceMut::new(&mut buf)],
                &mut control,
                RecvFlags::empty(),
            ) {
                Ok(received) if received.bytes == 0 => {
                    gone = true;
                    break;
                }
                Ok(received) => {
                    for message in control.drain() {
                        if let RecvAncillaryMessage::ScmRights(fds) = message {
                            self.fds.extend(fds);
                        }
                    }
                    self.decoder.feed(&buf[..received.bytes]);
                }
                Err(rustix::io::Errno::INTR) => continue,
                Err(rustix::io::Errno::AGAIN) => break,
                Err(err) => {
                    log(&format!("supervisor connection read failed: {err}"));
                    gone = true;
                    break;
                }
            }
        }

        let mut arrivals = Vec::new();
        loop {
            match self.decoder.next_frame() {
                Ok(Some(fields)) => {
                    let attached = fields
                        .first()
                        .is_some_and(|verb| verb.ends_with("-attached"))
                        .then(|| self.fds.pop_front())
                        .flatten();
                    if attached.is_none() && !fields.first().is_some_and(|v| v.ends_with("-attached")) {
                        // A reply to something the compositor asked for.
                        log(&format!("supervisor says: {}", fields.join(" ")));
                        continue;
                    }
                    arrivals.push((fields, attached));
                }
                Ok(None) => break,
                Err(err) => {
                    log(&format!("supervisor protocol error: {err}"));
                    gone = true;
                    break;
                }
            }
        }

        (arrivals, gone)
    }
}

/// Register as the compositor and keep the connection.
///
/// Registering is the readiness signal the supervisor gates dependents on, and
/// the same connection is what descriptors are pushed down afterwards. Dropping
/// it would tell the supervisor the compositor had gone away.
fn register() -> Result<UnixStream, String> {
    let mut stream =
        UnixStream::connect(SOCKET_PATH).map_err(|err| format!("connect: {err}"))?;

    stream
        .write_all(&encode(&["register", ROLE_HAIMANAGER]))
        .map_err(|err| format!("write: {err}"))?;

    let reply = read_frame(&mut stream).map_err(|err| format!("reply: {err}"))?;
    if reply.first().map(String::as_str) != Some("ok") {
        return Err(format!("refused: {}", reply.join(" ")));
    }

    Ok(stream)
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
