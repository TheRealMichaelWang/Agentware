//! Stage 6: take the machine down without corrupting anything.
//!
//! The supervisor is the only process that can do this correctly, because it is
//! the only one that knows about every other process. The sequence is fixed:
//! ask nicely, wait, insist, flush, unmount, power off.

use std::time::{Duration, Instant};

use rustix::system::{RebootCommand, reboot};

use crate::klog::{kinfo, kwarn};
use crate::desk::Desks;
use crate::service::Services;
use crate::{early, reaper};

/// How long processes get between `SIGTERM` and `SIGKILL`.
const GRACE: Duration = Duration::from_secs(5);

#[derive(Clone, Copy)]
pub enum Action {
    PowerOff,
    Reboot,
}

impl Action {
    fn command(self) -> RebootCommand {
        match self {
            Action::PowerOff => RebootCommand::PowerOff,
            Action::Reboot => RebootCommand::Restart,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Action::PowerOff => "powering off",
            Action::Reboot => "rebooting",
        }
    }
}

/// Shut the system down. Never returns.
///
/// Named services are stopped first, in reverse table order, so each gets a
/// chance to save state and so `startmenu` goes down before the `haimanager`
/// it draws through. The blanket signal that follows catches everything else:
/// orphans, agent processes, anything the table does not know about.
pub fn shutdown(action: Action, services: &mut Services, desks: &mut Desks) -> ! {
    kinfo!("shutdown requested, {}", action.label());

    // Workspaces first. They hold the user's work, so they get the chance to
    // wind down before the stack they are drawing through goes away.
    desks.close_all();
    services.stop_all();

    // kill(-1) hits every process we have permission to signal. The kernel
    // exempts PID 1, so this cannot kill the supervisor.
    signal_everything(libc::SIGTERM);

    if reaper::wait_for_all_to_exit(Instant::now() + GRACE) {
        kinfo!("all processes exited within the grace period");
    } else {
        kwarn!("grace period expired, sending SIGKILL");
        signal_everything(libc::SIGKILL);
        // SIGKILL cannot be caught or blocked, but a process stuck in
        // uninterruptible sleep still will not die. Bound the wait so a wedged
        // driver cannot hang the shutdown indefinitely.
        if !reaper::wait_for_all_to_exit(Instant::now() + Duration::from_secs(2)) {
            kwarn!("some processes survived SIGKILL, continuing anyway");
        }
    }

    // Flush the page cache to disk. Nothing here is disk-backed yet, but this
    // is the step whose absence silently corrupts data the moment it is.
    rustix::fs::sync();

    early::unmount_all();

    kinfo!("{}", action.label());

    if let Err(err) = reboot(action.command()) {
        // Reaching here means the reboot syscall itself failed, which should be
        // impossible for PID 1. There is nothing left to try.
        kwarn!("reboot syscall failed: {err}");
        crate::park();
    }

    // reboot(2) does not return on success.
    crate::park();
}

fn signal_everything(sig: libc::c_int) {
    // SAFETY: kill with pid -1 and a valid signal number. The kernel refuses to
    // deliver it to PID 1, so the supervisor cannot signal itself here.
    let result = unsafe { libc::kill(-1, sig) };
    if result != 0 {
        let err = std::io::Error::last_os_error();
        // ESRCH just means there was nobody to signal.
        if err.raw_os_error() != Some(libc::ESRCH) {
            kwarn!("could not broadcast {}: {err}", crate::signals::name(sig));
        }
    }
}
