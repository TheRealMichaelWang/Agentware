//! Agentware supervisor: PID 1.
//!
//! The supervisor is the root of the Agentware userland and the one process
//! that is not allowed to die. If it exits for any reason, the kernel panics
//! with "Attempted to kill init". Everything here is shaped by that constraint:
//! no `unwrap`, no unwinding out of `main`, and no work that belongs to a
//! process that is permitted to crash.
//!
//! Boot proceeds in stages:
//!
//!   0. survive      install the panic guard, confirm we really are PID 1
//!   1. filesystems  mount proc, sysfs, devtmpfs and friends
//!   2. machine      console log level, Ctrl-Alt-Del, hostname
//!   3. signals      block them and route them through a signalfd
//!   4. services     start ui-manager and desktop-main        (not yet)
//!   5. main loop    epoll over signals and the control socket
//!   6. shutdown     stop everything, flush, unmount, power off
//!
//! Stages 4 and the control socket half of stage 5 land with the service table.

mod early;
mod klog;
mod reaper;
mod selftest;
mod service;
mod shutdown;
mod signals;

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use rustix::event::epoll;

use klog::{kerr, kinfo, kwarn};
use service::{RestartPolicy, Service, Services};
use signals::SignalFd;

/// Set once we have confirmed we are PID 1, so the panic hook knows whether it
/// is allowed to let the process exit.
static IS_INIT: AtomicBool = AtomicBool::new(false);

/// epoll token for the signalfd. Further sources (the control socket, service
/// readiness pipes) get their own.
const TOKEN_SIGNALS: u64 = 1;

/// The services that make up the Agentware userland.
///
/// `ui-manager` comes first: `desktop-main` draws through it, so starting them
/// the other way round means the desktop fails against a compositor that is not
/// listening yet. Neither binary exists today, and the service table logs and
/// skips what is not installed rather than crash looping against it.
///
/// Ordering here is a stand-in for a real readiness protocol, which arrives with
/// fd passing in milestone 4.
fn system_services() -> Vec<Service> {
    vec![
        Service::new("ui-manager", "/bin/ui-manager", &[], RestartPolicy::Always),
        Service::new("desktop-main", "/bin/desktop-main", &[], RestartPolicy::Always),
    ]
}

fn main() {
    install_panic_hook();

    if std::process::id() != 1 {
        eprintln!(
            "supervisor: this is an init system and must run as PID 1.\n\
             It mounts filesystems and signals every process on the machine.\n\
             Boot it with `make run` instead of running it directly."
        );
        std::process::exit(1);
    }
    IS_INIT.store(true, Ordering::SeqCst);

    // Stage 1. Until this succeeds there is no /dev, so klog can only reach
    // stderr, which the kernel has already pointed at /dev/console for us.
    if let Err(err) = early::mount_virtual_filesystems() {
        kerr!("fatal: {err}");
        park();
    }

    // /dev/kmsg exists now, so everything from here lands in the kernel ring
    // buffer and on every console. Lift the rate limit first, or the eleventh
    // message of a busy boot and every message after it is discarded.
    early::unrestrict_kmsg();
    klog::open();
    kinfo!("Agentware supervisor starting");

    // Stage 2.
    //
    // Level 6 keeps kernel and supervisor info messages on screen, which is
    // what we want while there is no graphical shell to look at. Drop this to
    // `klog::WARN` at the point ui-manager takes over the display, or kernel
    // messages will draw straight over the compositor's output.
    early::set_console_loglevel(klog::INFO);
    early::disable_ctrl_alt_del();
    early::set_hostname("agentware");
    early::boot_report();

    // Stage 3. This has to happen before the first child is spawned: the signal
    // mask survives fork and exec, so anything started beforehand would inherit
    // a blocked SIGTERM and become unkillable.
    let signalfd = match signals::block_and_open_signalfd() {
        Ok(fd) => fd,
        Err(err) => {
            kerr!("fatal: could not set up signal handling: {err}");
            park();
        }
    };

    // Stage 4.
    let selftest = selftest::requested();
    let mut services = Services::new(if selftest {
        selftest::services()
    } else {
        system_services()
    });
    services.start_all();

    kinfo!("supervisor ready");

    // Stage 5.
    main_loop(signalfd, services, selftest)
}

/// Wait for events and dispatch them, forever.
///
/// Everything the supervisor reacts to becomes a file descriptor so it can live
/// in one epoll set: signals via signalfd today, the control socket and service
/// readiness pipes later. There is no polling and no busy loop, the process is
/// asleep in `epoll_wait` whenever nothing is happening.
fn main_loop(mut signalfd: SignalFd, mut services: Services, selftest: bool) -> ! {
    let epoll = match epoll::create(epoll::CreateFlags::CLOEXEC) {
        Ok(fd) => fd,
        Err(err) => {
            kerr!("fatal: could not create epoll: {err}");
            park();
        }
    };

    if let Err(err) = epoll::add(
        &epoll,
        &signalfd,
        epoll::EventData::new_u64(TOKEN_SIGNALS),
        epoll::EventFlags::IN,
    ) {
        kerr!("fatal: could not register signalfd with epoll: {err}");
        park();
    }

    let mut events = [epoll::Event {
        flags: epoll::EventFlags::empty(),
        data: epoll::EventData::new_u64(0),
    }; 16];

    loop {
        // Start anything whose backoff has expired, and find out when the next
        // one is due. That deadline becomes the epoll timeout, so waiting out a
        // backoff costs no extra file descriptor and no polling: the loop simply
        // sleeps until either an event arrives or a restart falls due.
        let deadline = services.tick();

        if selftest && services.all_settled() {
            kinfo!("selftest: every service reached a final state");
            shutdown::shutdown(shutdown::Action::PowerOff, &mut services);
        }

        let timeout = deadline.map(|at| to_timespec(at.saturating_duration_since(Instant::now())));

        let count = match epoll::wait(&epoll, &mut events, timeout.as_ref()) {
            Ok(count) => count,
            // Interrupted before any event was ready. Go around again.
            Err(rustix::io::Errno::INTR) => continue,
            Err(err) => {
                kerr!("fatal: epoll wait failed: {err}");
                park();
            }
        };

        // A count of zero means the timeout expired, which is not an event: the
        // next tick at the top of the loop is what handles it.
        for event in &events[..count] {
            // `epoll::Event` is packed on x86_64, so read the field out by
            // value rather than borrowing it.
            let token = event.data.u64();
            match token {
                TOKEN_SIGNALS => {
                    for signal in signalfd.drain() {
                        handle_signal(signal, &mut services);
                    }
                }
                other => kwarn!("event on unknown epoll token {other}"),
            }
        }
    }
}

fn to_timespec(duration: std::time::Duration) -> rustix::event::Timespec {
    rustix::event::Timespec {
        tv_sec: duration.as_secs() as _,
        tv_nsec: duration.subsec_nanos() as _,
    }
}

fn handle_signal(signal: libc::c_int, services: &mut Services) {
    match signal {
        // A child terminated. This is almost always a process the supervisor
        // never started: an orphaned grandchild re-parented to PID 1. Once the
        // service table exists, the reaped pids get matched against it here and
        // restart policy applied.
        libc::SIGCHLD => {
            for (pid, exit) in reaper::reap_all() {
                let pid = pid.as_raw_nonzero().get();

                // Anything the table does not claim is an orphaned grandchild
                // that was re-parented to PID 1. It needed reaping, but it has
                // no restart policy attached.
                if !services.note_exit(pid, &exit) {
                    kinfo!("reaped orphan pid {pid}: {exit}");
                }
            }
        }

        // Ctrl-Alt-Del, courtesy of disable_ctrl_alt_del turning the kernel's
        // immediate reset into a signal we can act on.
        libc::SIGINT => shutdown::shutdown(shutdown::Action::Reboot, services),

        libc::SIGUSR2 => shutdown::shutdown(shutdown::Action::Reboot, services),

        // Orderly poweroff. SIGPWR is the kernel telling us the power supply is
        // about to fail, so it gets the same treatment with more urgency.
        libc::SIGTERM | libc::SIGUSR1 | libc::SIGPWR => {
            shutdown::shutdown(shutdown::Action::PowerOff, services)
        }

        libc::SIGHUP => kinfo!("ignoring {}", signals::name(signal)),

        other => kwarn!("ignoring unexpected {}", signals::name(other)),
    }
}

/// Turn a panic into a survivable state instead of a kernel panic.
///
/// A panic that unwinds out of PID 1 takes the whole kernel down and reboots
/// immediately, which destroys the message explaining what went wrong. Parking
/// instead keeps the machine alive and the diagnostics on screen. Ctrl-Alt-Del
/// is handed back to the kernel first, so the user still has a way to reboot.
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        if !IS_INIT.load(Ordering::SeqCst) {
            eprintln!("supervisor panicked: {info}");
            return;
        }

        early::enable_ctrl_alt_del();
        kerr!("PANIC: {info}");
        kerr!("the supervisor cannot continue. Press Ctrl-Alt-Del to reboot.");
        park();
    }));
}

/// Sleep forever without burning a core.
///
/// This is the fail-safe state. `pause` blocks until a signal is delivered, and
/// every signal the supervisor cares about is blocked, so it never returns.
/// It is here rather than a `loop {}` because a bare spin would peg a CPU at
/// 100% for as long as the machine stays powered on.
pub fn park() -> ! {
    loop {
        // SAFETY: pause takes no arguments and only ever blocks.
        unsafe { libc::pause() };
    }
}
