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
//!   4. services     bind the control socket, start the service table
//!   5. main loop    epoll over signals and the control socket
//!   6. shutdown     stop everything, flush, unmount, power off

mod cgroup;
mod demo;
mod desk;
mod early;
mod ipc;
mod klog;
mod reaper;
mod selftest;
mod service;
mod shutdown;
mod signals;

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use awproto::ROLE_HAIMANAGER;
use rustix::event::epoll;

use desk::{Desks, Programs};
use ipc::Control;
use klog::{kerr, kinfo, kwarn};
use service::{RestartPolicy, Service, Services};
use signals::SignalFd;

/// Set once we have confirmed we are PID 1, so the panic hook knows whether it
/// is allowed to let the process exit.
static IS_INIT: AtomicBool = AtomicBool::new(false);

/// epoll token for the signalfd. The control socket listener is
/// `ipc::TOKEN_LISTENER`, and each accepted connection gets a token derived from
/// its own file descriptor.
const TOKEN_SIGNALS: u64 = 1;

/// The services that make up the Agentware userland.
///
/// One entry. The compositor asks the broker for the first workspace itself
/// when it comes up, and every workspace after that is created from the
/// navigation bar's plus or a workspace's own taskbar, so there is no start
/// menu process to list. What is still missing is an agent with a model behind
/// it: `Programs::system` names `/bin/agent`, and the service table's habit of
/// logging and skipping what is not installed extends to the broker refusing
/// `start-agent` with a clear error rather than crash looping.
fn system_services() -> Vec<Service> {
    vec![Service::new(ROLE_HAIMANAGER, "/bin/haimanager", &[], RestartPolicy::Always)]
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
    // `klog::WARN` at the point haimanager takes over the display, or kernel
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

    let selftest = selftest::requested();
    let demo = !selftest && demo::requested();

    // Stage 4a. Both of these come up before any service does, so nothing can
    // start, try to reach the broker, and lose a race it did not know it was in.
    if let Err(err) = cgroup::Cgroup::init_root() {
        kwarn!("could not create the cgroup root: {err} (workspace teardown will be degraded)");
    }

    let control = match Control::bind() {
        Ok(control) => control,
        Err(err) => {
            kerr!("fatal: could not bind the control socket: {err}");
            park();
        }
    };

    let desks = Desks::new(match (selftest, demo) {
        (true, _) => selftest::programs(),
        (_, true) => demo::programs(),
        _ => Programs::system(),
    });

    // Stage 4b.
    let mut services = Services::new(match (selftest, demo) {
        (true, _) => selftest::services(),
        (_, true) => demo::services(),
        _ => system_services(),
    });
    services.start_all();

    kinfo!("supervisor ready");

    // Stage 5.
    main_loop(signalfd, control, services, desks, selftest)
}

/// Wait for events and dispatch them, forever.
///
/// Everything the supervisor reacts to is a file descriptor, so it all lives in
/// one epoll set: signals via signalfd, the control socket listener, and every
/// accepted connection. There is no polling and no busy loop; the process is
/// asleep in `epoll_wait` whenever nothing is happening, and wakes on either an
/// event or the soonest pending deadline.
fn main_loop(
    mut signalfd: SignalFd,
    mut control: Control,
    mut services: Services,
    mut desks: Desks,
    selftest: bool,
) -> ! {
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

    if let Err(err) = epoll::add(
        &epoll,
        control.listener_fd(),
        epoll::EventData::new_u64(ipc::TOKEN_LISTENER),
        epoll::EventFlags::IN,
    ) {
        kerr!("fatal: could not register the control socket with epoll: {err}");
        park();
    }

    let mut events = [epoll::Event {
        flags: epoll::EventFlags::empty(),
        data: epoll::EventData::new_u64(0),
    }; 16];

    loop {
        // Drive both tables forward: services whose backoff has expired or whose
        // dependency just became ready, and workspaces partway through teardown.
        // Each reports when it next needs attention, and the sooner of the two
        // becomes the epoll timeout. Waiting on a deadline therefore costs no
        // extra file descriptor and no polling; the loop just sleeps until an
        // event arrives or the deadline falls due.
        let deadline = soonest(services.tick(|role| control.is_ready(role)), desks.tick());

        if selftest && services.all_settled() && desks.is_empty() {
            kinfo!("selftest: every service settled and every workspace closed");
            shutdown::shutdown(shutdown::Action::PowerOff, &mut services, &mut desks);
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
            let flags = event.flags;

            match token {
                TOKEN_SIGNALS => {
                    for signal in signalfd.drain() {
                        handle_signal(signal, &mut services, &mut desks);
                    }
                }

                ipc::TOKEN_LISTENER => control.accept_ready(&epoll),

                _ if ipc::is_connection_token(token) => {
                    let fd = ipc::fd_for(token);
                    if flags.contains(epoll::EventFlags::OUT) {
                        control.handle_writable(fd, &epoll);
                    }
                    if flags.intersects(
                        epoll::EventFlags::IN | epoll::EventFlags::HUP | epoll::EventFlags::ERR,
                    ) {
                        control.handle_readable(fd, &mut desks, &epoll);
                    }
                }

                other => kwarn!("event on unknown epoll token {other}"),
            }
        }
    }
}

/// The earlier of two optional deadlines.
fn soonest(a: Option<Instant>, b: Option<Instant>) -> Option<Instant> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (only, None) | (None, only) => only,
    }
}

fn to_timespec(duration: std::time::Duration) -> rustix::event::Timespec {
    rustix::event::Timespec {
        tv_sec: duration.as_secs() as _,
        tv_nsec: duration.subsec_nanos() as _,
    }
}

fn handle_signal(signal: libc::c_int, services: &mut Services, desks: &mut Desks) {
    match signal {
        // A child terminated. This is almost always a process the supervisor
        // never started: an orphaned grandchild re-parented to PID 1. Once the
        // service table exists, the reaped pids get matched against it here and
        // restart policy applied.
        libc::SIGCHLD => {
            for (pid, exit) in reaper::reap_all() {
                let pid = pid.as_raw_nonzero().get();

                // Anything neither table claims is an orphaned grandchild that
                // was re-parented to PID 1. It needed reaping, but it has no
                // policy and no owner attached.
                if !services.note_exit(pid, &exit) && !desks.note_exit(pid, &exit) {
                    kinfo!("reaped orphan pid {pid}: {exit}");
                }
            }
        }

        // Ctrl-Alt-Del, courtesy of disable_ctrl_alt_del turning the kernel's
        // immediate reset into a signal we can act on.
        libc::SIGINT => shutdown::shutdown(shutdown::Action::Reboot, services, desks),

        libc::SIGUSR2 => shutdown::shutdown(shutdown::Action::Reboot, services, desks),

        // Orderly poweroff. SIGPWR is the kernel telling us the power supply is
        // about to fail, so it gets the same treatment with more urgency.
        libc::SIGTERM | libc::SIGUSR1 | libc::SIGPWR => {
            shutdown::shutdown(shutdown::Action::PowerOff, services, desks)
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
