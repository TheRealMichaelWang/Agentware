//! Stage 3: take ownership of signals.
//!
//! PID 1 is special. The kernel installs no default dispositions for it, so any
//! signal without an explicit handler is silently discarded. The supervisor
//! must be explicit about every signal it cares about.
//!
//! Rather than use signal handlers, which run asynchronously and may only call
//! async-signal-safe functions, we block the signals we want and read them out
//! of a `signalfd`. That turns a signal into an ordinary readable file
//! descriptor, so it can sit in the same `epoll` set as every other event
//! source and be handled by normal code in the main loop.
//!
//! rustix deliberately does not wrap `signalfd` or `sigprocmask`, so this is
//! the one module that talks to libc directly.

use std::fs::File;
use std::io::{self, Read};
use std::mem;
use std::os::fd::{AsFd, BorrowedFd, FromRawFd};

/// The signals the supervisor handles. Every one of these is blocked for
/// delivery and read from the signalfd instead.
const HANDLED: &[libc::c_int] = &[
    // A child changed state. The trigger for the reaper.
    libc::SIGCHLD,
    // Orderly shutdown, from a `poweroff` request.
    libc::SIGTERM,
    // Ctrl-Alt-Del, once the kernel's own handling is disabled.
    libc::SIGINT,
    // Poweroff and reboot, for callers that would rather be explicit than rely
    // on SIGTERM's default meaning.
    libc::SIGUSR1,
    libc::SIGUSR2,
    // The kernel's "power is failing" notification.
    libc::SIGPWR,
    // Meaningless for PID 1, but blocked so it cannot be inherited unblocked.
    libc::SIGHUP,
];

pub struct SignalFd {
    file: File,
}

impl SignalFd {
    /// Read every signal currently queued on the fd.
    ///
    /// The fd is non-blocking, so this drains until the kernel says there is
    /// nothing left. Draining fully matters for `SIGCHLD`: standard signals do
    /// not queue, so several children exiting in quick succession may produce
    /// only one readable event. The reaper compensates by looping over
    /// `waitpid` regardless of how many signals it sees here.
    pub fn drain(&mut self) -> Vec<libc::c_int> {
        const SIZE: usize = mem::size_of::<libc::signalfd_siginfo>();
        let mut signals = Vec::new();
        let mut buf = [0u8; SIZE * 8];

        loop {
            match self.file.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    for chunk in buf[..n].chunks_exact(SIZE) {
                        // SAFETY: the kernel guarantees each SIZE-byte record
                        // on a signalfd is a valid `signalfd_siginfo`.
                        let info: libc::signalfd_siginfo =
                            unsafe { std::ptr::read_unaligned(chunk.as_ptr().cast()) };
                        signals.push(info.ssi_signo as libc::c_int);
                    }
                    if n < buf.len() {
                        break;
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }

        signals
    }
}

impl AsFd for SignalFd {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.file.as_fd()
    }
}

/// Block the handled signals and return a signalfd that delivers them.
///
/// Blocking must happen before any child is spawned, because the signal mask
/// survives both `fork` and `exec`. See [`reset_for_child`].
pub fn block_and_open_signalfd() -> io::Result<SignalFd> {
    // SAFETY: sigemptyset/sigaddset/sigprocmask/signalfd are called with a
    // properly sized, initialised sigset_t and valid arguments throughout.
    unsafe {
        let mut mask: libc::sigset_t = mem::zeroed();
        if libc::sigemptyset(&mut mask) != 0 {
            return Err(io::Error::last_os_error());
        }
        for &sig in HANDLED {
            if libc::sigaddset(&mut mask, sig) != 0 {
                return Err(io::Error::last_os_error());
            }
        }

        if libc::sigprocmask(libc::SIG_BLOCK, &mask, std::ptr::null_mut()) != 0 {
            return Err(io::Error::last_os_error());
        }

        let fd = libc::signalfd(-1, &mask, libc::SFD_CLOEXEC | libc::SFD_NONBLOCK);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(SignalFd { file: File::from_raw_fd(fd) })
    }
}

/// Restore the default signal state in a freshly forked child, before `exec`.
///
/// This is not optional housekeeping. The blocked signal mask is inherited
/// across `fork` and preserved across `exec`, so without this every service the
/// supervisor starts would run with `SIGTERM` blocked and would be unkillable
/// by anything short of `SIGKILL`. It is a classic init bug and the symptom
/// (shutdown hangs for the full grace period, every time) is hard to trace back
/// to its cause.
///
/// Must only be called between `fork` and `exec`, where it is async-signal-safe.
pub unsafe fn reset_for_child() -> io::Result<()> {
    // SAFETY: caller guarantees this runs in the pre-exec child.
    unsafe {
        let mut empty: libc::sigset_t = mem::zeroed();
        libc::sigemptyset(&mut empty);
        if libc::sigprocmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut()) != 0 {
            return Err(io::Error::last_os_error());
        }

        // Rust's runtime sets SIGPIPE to ignore on startup, which is also
        // inherited. Most programs expect the default.
        if libc::signal(libc::SIGPIPE, libc::SIG_DFL) == libc::SIG_ERR {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Human-readable signal name, for logs.
pub fn name(sig: libc::c_int) -> &'static str {
    match sig {
        libc::SIGCHLD => "SIGCHLD",
        libc::SIGTERM => "SIGTERM",
        libc::SIGINT => "SIGINT",
        libc::SIGUSR1 => "SIGUSR1",
        libc::SIGUSR2 => "SIGUSR2",
        libc::SIGPWR => "SIGPWR",
        libc::SIGHUP => "SIGHUP",
        libc::SIGKILL => "SIGKILL",
        libc::SIGSEGV => "SIGSEGV",
        libc::SIGABRT => "SIGABRT",
        _ => "signal",
    }
}
