//! Milestone 1 acceptance check, enabled with `agentware.selftest` on the
//! kernel command line.
//!
//! Until the service table exists the supervisor has no children, which means
//! the reaper and the shutdown path never run. This spawns one throwaway child
//! and powers the machine off when it is reaped, so a single headless boot
//! exercises the whole chain: fork, exec, SIGCHLD, reap, SIGTERM broadcast,
//! sync, unmount, poweroff. QEMU exiting on its own is the pass signal.
//!
//! It stays useful after the service table lands, as a smoke test that does not
//! depend on any of the graphical stack being present.

use std::os::unix::process::CommandExt;
use std::process::Command;
use std::sync::atomic::{AtomicI32, Ordering};

use crate::klog::{kerr, kinfo};
use crate::reaper::Exit;
use crate::signals;

/// Set in the child's environment so it can recognise itself.
const CHILD_ENV: &str = "AGENTWARE_SELFTEST_CHILD";

/// An arbitrary non-zero status. Non-zero so we also prove the supervisor reads
/// the exit code correctly rather than defaulting to success.
pub const CHILD_STATUS: i32 = 7;

static CHILD_PID: AtomicI32 = AtomicI32::new(0);

/// True if this process is the spawned child rather than the supervisor.
///
/// Checked before the PID 1 guard in `main`, because the child is deliberately
/// not PID 1 and would otherwise be turned away by it.
pub fn is_child() -> bool {
    std::env::var_os(CHILD_ENV).is_some()
}

/// True if the kernel command line asked for the self-test.
pub fn requested() -> bool {
    std::fs::read_to_string("/proc/cmdline")
        .map(|line| line.split_whitespace().any(|word| word == "agentware.selftest"))
        .unwrap_or(false)
}

/// Fork and exec a child that exits immediately.
///
/// The child is this same binary, re-executed through `/proc/self/exe`, because
/// the initramfs contains no other executable to run.
pub fn spawn() {
    kinfo!("selftest: spawning a child to exercise the reaper");

    let mut command = Command::new("/proc/self/exe");
    command.env(CHILD_ENV, "1");

    // SAFETY: the closure runs between fork and exec, and only calls
    // sigprocmask and signal, both of which are async-signal-safe.
    unsafe {
        command.pre_exec(|| signals::reset_for_child());
    }

    match command.spawn() {
        Ok(child) => {
            CHILD_PID.store(child.id() as i32, Ordering::SeqCst);
            kinfo!("selftest: child is pid {}", child.id());
        }
        Err(err) => {
            kerr!("selftest: could not spawn child: {err}");
            crate::shutdown::shutdown(crate::shutdown::Action::PowerOff);
        }
    }
}

/// Called for each reaped process. Returns true if this was the self-test
/// child, meaning the run is over.
pub fn is_finished(pid: i32, exit: &Exit) -> bool {
    if CHILD_PID.load(Ordering::SeqCst) != pid {
        return false;
    }

    match exit {
        Exit::Code(CHILD_STATUS) => {
            kinfo!("selftest: PASS, child reaped with the expected status");
        }
        other => {
            kerr!("selftest: FAIL, child {other}, expected status {CHILD_STATUS}");
        }
    }

    true
}
