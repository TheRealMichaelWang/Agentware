//! The Human-Agent Interface Manager.
//!
//! Owns the display, the keyboard and the mouse, and is the only process in
//! Agentware that touches any of them. Everything else describes what it wants
//! shown as AWML and receives events back.
//!
//! Milestone 5: trees arrive from real processes. The compositor registers with
//! the supervisor, receives one descriptor per process that draws, holds a tree
//! per connection, diffs each new one against the one it is holding, carries the
//! ephemeral state applications deliberately do not track, and sends events back
//! stamped with the version of the tree they were generated against.
//!
//! Which connection appears where is milestone 6. For now the newest is in
//! front and F1 cycles, which is enough to see that several clients are held
//! independently, with independent focus and independent scroll.

mod awml;
mod client;
mod cursor;
mod document;
mod drm;
mod input;
mod paint;
mod status;
mod ui;

use std::collections::VecDeque;
use std::io::{IoSliceMut, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, AsRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

use awproto::{Decoder, ROLE_HAIMANAGER, SOCKET_PATH, encode, read_frame};
use rustix::event::epoll;
use rustix::net::{RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, recvmsg};

use client::Clients;
use input::{Event, Input, Key};
use paint::font::Fonts;
use paint::{Canvas, Rect};

/// epoll tokens. The supervisor connection is fixed; input devices take one each
/// offset by their index, and client connections take one derived from their own
/// descriptor, which is unique for as long as it is open.
const TOKEN_SUPERVISOR: u64 = 1;
const TOKEN_INPUT_BASE: u64 = 0x100;
const TOKEN_CLIENT_BASE: u64 = 0x1000;

/// Cycles which client is in front. A stand-in for window management, which
/// arrives with workspace compositing.
const KEY_F1: u16 = 59;

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

    // The status readout sits along the bottom, so clients get the rest.
    let area = Rect::new(0, 0, width as i32, height as i32 - status::HEIGHT);
    let mut clients = Clients::new(area);

    // Paint once before waiting, so a machine with nothing attached still shows
    // something rather than a blank screen.
    redraw(&mut display, &mut canvas, &fonts, &input, &clients);
    log(&format!("{} glyphs rasterized for the first frame", fonts.glyph_count()));

    run(&mut display, &mut canvas, &fonts, &mut input, &mut clients, supervisor);
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
    clients: &mut Clients,
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

            match token {
                TOKEN_SUPERVISOR => {
                    let Some(inbox) = &mut handoffs else { continue };
                    let (arrivals, gone) = inbox.drain();

                    for (fields, fd) in arrivals {
                        dirty |= adopt(&epoll, clients, &fields, fd);
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
                    let Some(client) = clients.get_mut(fd) else { continue };

                    let progress = client.readable(fonts);
                    for line in progress.log {
                        log(&line);
                    }
                    dirty |= progress.dirty;

                    // A first tree is worth printing whole: the reduced schema
                    // can then be read against the document that produced it,
                    // which is the claim the design makes about them.
                    if progress.dirty
                        && let Some(view) = client.agent_view()
                        && client.version() == 1
                    {
                        for line in view.lines() {
                            log(&format!("agent view | {line}"));
                        }
                    }

                    if progress.gone {
                        let _ = epoll::delete(&epoll, client.borrow());
                        if let Some(label) = clients.remove(fd) {
                            log(&format!("{label} disconnected"));
                        }
                        dirty = true;
                    }
                }

                token if token >= TOKEN_INPUT_BASE => {
                    let index = (token - TOKEN_INPUT_BASE) as usize;
                    for event in input.read_device(index) {
                        dirty |= route(fonts, clients, event);
                    }
                }

                other => log(&format!("event on unknown epoll token {other}")),
            }
        }

        // Any backlog a full socket left behind goes out now rather than waiting
        // for the next thing to happen to that client.
        clients.flush_all();

        for fd in clients.broken() {
            if let Some(client) = clients.get_mut(fd) {
                let _ = epoll::delete(&epoll, client.borrow());
            }
            if let Some(label) = clients.remove(fd) {
                log(&format!("{label} stopped reading its events and was dropped"));
            }
            dirty = true;
        }

        if dirty {
            redraw(display, canvas, fonts, input, clients);
        }
    }
}

/// Take a descriptor the supervisor pushed and start watching it.
fn adopt(epoll: &impl AsFd, clients: &mut Clients, fields: &[String], fd: Option<OwnedFd>) -> bool {
    let Some(fd) = fd else {
        log(&format!("handoff {:?} arrived with no descriptor", fields.join(" ")));
        return false;
    };

    let raw = fd.as_raw_fd();
    let label = match clients.attach(fields, fd) {
        Ok(label) => label,
        Err(err) => {
            log(&format!("refused a handoff: {err}"));
            return false;
        }
    };

    // The client owns the descriptor now, so it is borrowed back out of the
    // registry rather than kept here.
    let Some(client) = clients.get_mut(raw) else { return false };
    let watched = epoll::add(
        epoll,
        client.borrow(),
        epoll::EventData::new_u64(TOKEN_CLIENT_BASE + raw as u64),
        epoll::EventFlags::IN,
    );

    if let Err(err) = watched {
        log(&format!("could not watch {label}: {err}"));
        clients.remove(raw);
        return false;
    }

    log(&format!("{label} attached"));
    true
}

/// Send one input event where it belongs.
///
/// Everything goes to the client in front, except the pointer, which belongs to
/// the compositor and only ever moves the cursor. Routing by workspace region
/// comes with compositing.
fn route(fonts: &Fonts, clients: &mut Clients, event: Event) -> bool {
    match event {
        // The cursor is drawn by the compositor, so a move is a repaint and
        // nothing else. No client is told the pointer went past it.
        Event::PointerMoved { .. } => true,

        Event::KeyPressed(Key::Other(KEY_F1)) => clients.cycle(),

        other => clients
            .front_mut()
            .map(|client| client.handle(fonts, other))
            .unwrap_or(false),
    }
}

fn redraw(
    display: &mut drm::Display,
    canvas: &mut Canvas,
    fonts: &Fonts,
    input: &Input,
    clients: &Clients,
) {
    match clients.front() {
        Some(client) if client.has_document() => client.draw(canvas, fonts),
        _ => status::draw_idle(canvas, fonts, clients),
    }
    status::draw(canvas, fonts, clients);

    let (x, y) = input.pointer();
    cursor::draw(canvas, x, y, cursor::Kind::Human);

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
