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
mod icons;
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

    // The interface scale is fixed before anything is measured or drawn. The
    // default keeps 960 rows of logical space whatever the mode: a taller
    // monitor gets larger type and chrome rather than an emptier desk.
    // `agentware.scale=1.25` on the kernel command line overrides it.
    let scale = interface_scale(height as i32);
    ui::set_scale(scale);
    log(&format!("interface scale {scale}"));

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

    // Where the cursors were last stamped, so a pointer move can put the scene
    // back before stamping them anew.
    let mut overlay_rects: Vec<Rect> = Vec::new();

    // Presents of any kind are capped near 120Hz. Every flush is a host round
    // trip, and both a tablet and a window drag can produce work far faster
    // than a display shows it; flushing per event is how light work still
    // manages to feel heavy. Deferred work wakes the loop through the timeout
    // below instead of being dropped.
    const PRESENT_MIN: std::time::Duration = std::time::Duration::from_millis(8);
    let mut last_present = std::time::Instant::now();
    let mut scene_pending = false;
    let mut overlay_pending = false;

    // Frame cost accounting, reported once a second while frames happen. The
    // difference between "feels sluggish" and a fix is a number.
    let mut stat_paint = std::time::Duration::ZERO;
    let mut stat_blit = std::time::Duration::ZERO;
    let mut stat_worst = std::time::Duration::ZERO;
    let mut stat_frames: u32 = 0;
    let mut stat_since = std::time::Instant::now();

    // While the agent's cursor is travelling there is an animation to run, and
    // an animation is the one thing an event-driven loop cannot wait for. This
    // is the only case in which the compositor wakes without being asked to.
    let frame = rustix::event::Timespec { tv_sec: 0, tv_nsec: 16_000_000 };

    let stamped = compose_overlay(display, canvas, input, screen, &mut overlay_rects);
    let _ = display.flush_rects(&stamped);

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

        // Three tempos: animation frames while something moves, the next caret
        // blink while a text field waits, and the slow device rescan otherwise.
        // The blink deadline matters: without it the caret would only change
        // when something else happened to wake the loop.
        let blink;
        let defer;
        let waiting = if screen.wants_frame() {
            &frame
        } else if scene_pending || overlay_pending {
            let remaining = PRESENT_MIN.saturating_sub(last_present.elapsed());
            defer = rustix::event::Timespec {
                tv_sec: 0,
                tv_nsec: remaining.subsec_nanos().max(1_000_000) as _,
            };
            &defer
        } else if let Some(until) = screen.until_blink() {
            blink = rustix::event::Timespec {
                tv_sec: until.as_secs().min(1) as _,
                tv_nsec: until.subsec_nanos() as _,
            };
            &blink
        } else {
            &rescan_every
        };
        let count = match epoll::wait(&epoll, &mut events, Some(waiting)) {
            Ok(count) => count,
            Err(rustix::io::Errno::INTR) => continue,
            Err(err) => {
                log(&format!("FATAL: epoll wait failed: {err}"));
                park();
            }
        };

        let tick_dirty = screen.tick(fonts);
        let mut dirty = tick_dirty;
        let mut only_pointer = !tick_dirty;
        for event in &events[..count] {
            let token = event.data.u64();
            if token < TOKEN_INPUT_BASE {
                // The supervisor connection: handoffs change the scene.
                only_pointer = false;
            }

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
                    only_pointer = false;
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
                        if !matches!(event, Event::PointerMoved { .. }) {
                            only_pointer = false;
                        }
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

        // Presenting, atomically: everything for the frame goes into the
        // framebuffer first and the host is told once, so no in-between state
        // can flicker onto the screen. Scene work capped like the overlay; a
        // wake inside the window defers through the timeout above. A pure
        // window drag repaints only the region the window swept, shadows and
        // all, instead of the screen.
        dirty |= std::mem::take(&mut scene_pending);
        let overlay = screen.take_overlay_dirty() || std::mem::take(&mut overlay_pending);
        let ready = last_present.elapsed() >= PRESENT_MIN;

        if !ready {
            scene_pending = dirty;
            overlay_pending = overlay && !dirty;
        } else if dirty {
            let damage = screen.take_drag_damage();
            match damage.filter(|_| only_pointer) {
                Some(region) => {
                    let region = region.intersect(&canvas.bounds());
                    if let Some(region) = region {
                        let t0 = std::time::Instant::now();
                        canvas.clipped(region, |scene| screen.draw(scene, fonts, input.pointer()));
                        let t1 = std::time::Instant::now();
                        display.blit_region(canvas, region);
                        stat_paint += t1 - t0;
                        stat_blit += t1.elapsed();
                        stat_worst = stat_worst.max(t1 - t0);
                        stat_frames += 1;
                        let mut flushes =
                            compose_overlay(display, canvas, input, screen, &mut overlay_rects);
                        flushes.push(region);
                        if let Err(err) = display.flush_rects(&flushes) {
                            log(&format!("could not flush a drag frame: {err}"));
                        }
                    }
                }
                None => {
                    let t0 = std::time::Instant::now();
                    screen.draw(canvas, fonts, input.pointer());
                    let t1 = std::time::Instant::now();
                    display.blit_full(canvas);
                    stat_paint += t1 - t0;
                    stat_blit += t1.elapsed();
                    stat_worst = stat_worst.max(t1 - t0);
                    stat_frames += 1;
                    overlay_rects.clear();
                    compose_overlay(display, canvas, input, screen, &mut overlay_rects);
                    if let Err(err) = display.flush() {
                        log(&format!("could not present: {err}"));
                    }
                }
            }
            last_present = std::time::Instant::now();

            if stat_since.elapsed().as_secs() >= 1 && stat_frames > 0 {
                log(&format!(
                    "frames: {} in {}ms, paint avg {:.1}ms worst {:.1}ms, blit avg {:.1}ms",
                    stat_frames,
                    stat_since.elapsed().as_millis(),
                    stat_paint.as_secs_f32() * 1000.0 / stat_frames as f32,
                    stat_worst.as_secs_f32() * 1000.0,
                    stat_blit.as_secs_f32() * 1000.0 / stat_frames as f32,
                ));
                stat_paint = std::time::Duration::ZERO;
                stat_blit = std::time::Duration::ZERO;
                stat_worst = std::time::Duration::ZERO;
                stat_frames = 0;
                stat_since = std::time::Instant::now();
            }
        } else if overlay {
            let flushes = compose_overlay(display, canvas, input, screen, &mut overlay_rects);
            if let Err(err) = display.flush_rects(&flushes) {
                log(&format!("could not flush the overlay: {err}"));
            }
            last_present = std::time::Instant::now();
        }
    }
}

/// Whether two rectangles touch or overlap, for merging damage.
fn touching(a: Rect, b: Rect) -> bool {
    a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h
}

fn union(a: Rect, b: Rect) -> Rect {
    let x0 = a.x.min(b.x);
    let y0 = a.y.min(b.y);
    let x1 = (a.x + a.w).max(b.x + b.w);
    let y1 = (a.y + a.h).max(b.y + b.h);
    Rect::new(x0, y0, x1 - x0, y1 - y0)
}

/// Move the cursors from wherever they were to wherever they are, atomically.
///
/// The old and new cursor rectangles are merged into damage regions, each
/// region is composed *complete* off screen (scene, then every cursor that
/// intersects it), blitted, and only then is the host told anything changed,
/// in one call carrying every clip. The first version erased and stamped as
/// separate flushes, and the host was free to present the instant in between:
/// a cursor that existed in every composed frame and still flickered.
fn compose_overlay(
    display: &mut drm::Display,
    scene: &Canvas,
    input: &Input,
    screen: &Screen,
    prev: &mut Vec<Rect>,
) -> Vec<Rect> {
    let mut cursors: Vec<(i32, i32, cursor::Kind, cursor::Shape)> = Vec::new();
    if let Some((x, y)) = screen.agent_pointer() {
        cursors.push((x, y, cursor::Kind::Agent, cursor::Shape::Arrow));
    }
    let (x, y) = input.pointer();
    cursors.push((x, y, cursor::Kind::Human, screen.pointer_shape(x, y)));

    let mut fresh: Vec<Rect> = Vec::new();
    for &(x, y, kind, shape) in &cursors {
        if let Some(rect) = cursor::bounds(x, y, kind, shape).intersect(&scene.bounds()) {
            fresh.push(rect);
        }
    }

    // Damage is everything a cursor is leaving plus everything one is entering,
    // merged where they touch so a small move is one region.
    let mut regions: Vec<Rect> = prev.drain(..).chain(fresh.iter().copied()).collect();
    loop {
        let mut merged = false;
        'outer: for a in 0..regions.len() {
            for b in a + 1..regions.len() {
                if touching(regions[a], regions[b]) {
                    let joined = union(regions[a], regions[b]);
                    regions.swap_remove(b);
                    regions[a] = joined;
                    merged = true;
                    break 'outer;
                }
            }
        }
        if !merged {
            break;
        }
    }

    for region in &regions {
        let Some(region) = region.intersect(&scene.bounds()) else { continue };
        let mut patch = Canvas::new(region.w as usize, region.h as usize);
        patch.copy_from(scene, region.x, region.y);
        for &(x, y, kind, shape) in &cursors {
            if touching(cursor::bounds(x, y, kind, shape), region) {
                cursor::draw(&mut patch, x - region.x, y - region.y, kind, shape);
            }
        }
        display.blit_patch(&patch, region.x, region.y);
    }

    *prev = fresh;
    regions
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

/// The interface scale for a display this many rows tall.
///
/// Quarter steps, because scaled metrics land on cleaner pixel boundaries than
/// an arbitrary ratio and nobody can see the difference between 1.5 and 1.47.
fn interface_scale(height: i32) -> f32 {
    if let Ok(cmdline) = std::fs::read_to_string("/proc/cmdline")
        && let Some(word) = cmdline.split_whitespace().find_map(|word| word.strip_prefix("agentware.scale="))
        && let Ok(asked) = word.parse::<f32>()
    {
        return asked.clamp(1.0, 3.0);
    }

    ((height as f32 / 960.0) * 4.0).round().max(4.0) / 4.0
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
