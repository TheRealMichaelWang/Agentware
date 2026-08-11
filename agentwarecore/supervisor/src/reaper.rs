//! The grim reaper.
//!
//! Every orphaned process on the system is re-parented to PID 1. If the
//! supervisor does not `waitpid` for them, they stay in the process table
//! forever as zombies, and the table is a finite resource. On a machine that
//! spawns a fresh agent and agentdesk for every prompt, that leak is not
//! theoretical.
//!
//! Reaping is deliberately separate from service supervision. Most of what the
//! supervisor reaps will be processes it never started: grandchildren orphaned
//! when some intermediate process died. Those still need collecting, they just
//! have no restart policy attached.

use rustix::process::{self, Pid, WaitOptions, WaitStatus};

use crate::klog::kinfo;

/// How a child ended.
pub enum Exit {
    Code(i32),
    Signal(i32),
}

impl std::fmt::Display for Exit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Exit::Code(0) => write!(f, "exited cleanly"),
            Exit::Code(code) => write!(f, "exited with status {code}"),
            Exit::Signal(sig) => write!(f, "killed by {}", crate::signals::name(*sig)),
        }
    }
}

/// Collect every child that has terminated, and return what was collected.
///
/// This must loop rather than reap once per `SIGCHLD`. Standard signals do not
/// queue: if three children die while one `SIGCHLD` is already pending, the
/// supervisor still sees a single signal. Looping until `waitpid` reports
/// nothing left is what makes the count irrelevant.
///
/// Returns the reaped children so the caller can match them against the service
/// table. For now nothing owns that table, so the caller only logs them.
pub fn reap_all() -> Vec<(Pid, Exit)> {
    let mut reaped = Vec::new();

    loop {
        match process::waitpid(None, WaitOptions::NOHANG) {
            // A child was collected.
            Ok(Some((pid, status))) => {
                if let Some(exit) = classify(status) {
                    reaped.push((pid, exit));
                }
                // Anything else is a stop or continue notification, not a
                // termination, so there is nothing to record.
            }
            // Children exist, but none of them have terminated.
            Ok(None) => break,
            // ECHILD: no children at all. Everything is collected.
            Err(_) => break,
        }
    }

    reaped
}

/// Wait for every child to disappear, or until `deadline` passes.
///
/// Used by the shutdown path between `SIGTERM` and `SIGKILL`. Returns true if
/// the process table drained on its own.
pub fn wait_for_all_to_exit(deadline: std::time::Instant) -> bool {
    loop {
        match process::waitpid(None, WaitOptions::NOHANG) {
            Ok(Some((pid, status))) => {
                if let Some(exit) = classify(status) {
                    kinfo!("pid {} {}", pid.as_raw_nonzero(), exit);
                }
            }
            // ECHILD. Nothing is left, so shutdown can proceed immediately.
            Err(_) => return true,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    return false;
                }
                // Children remain but none are ready. Poll rather than block,
                // so the deadline is actually enforced.
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }
}

fn classify(status: WaitStatus) -> Option<Exit> {
    if let Some(code) = status.exit_status() {
        Some(Exit::Code(code))
    } else {
        status.terminating_signal().map(Exit::Signal)
    }
}
