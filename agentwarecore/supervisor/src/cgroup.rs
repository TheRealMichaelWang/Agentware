//! One cgroup per agentdesk.
//!
//! A workspace has to be killable as a unit: closing an agentdesk must take its
//! apps with it, or every closed workspace leaks processes until the machine
//! reboots.
//!
//! A process group would almost work, but a process can leave one on its own
//! with `setpgid` or `setsid`, so an app can escape the thing that owns it. It
//! cannot leave its cgroup. `cgroup.kill` then terminates every member at once,
//! including anything that double-forked, with no traversal and no races.
//!
//! The same cgroup is where per-workspace memory accounting and resource limits
//! will go, which is what a runaway agent gets capped by and what decides which
//! idle agentdesk to suspend first.

use std::fs;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::PathBuf;

use crate::klog::kwarn;

/// Where the supervisor puts its own hierarchy, under the cgroup2 mount made at
/// boot.
const ROOT: &str = "/sys/fs/cgroup/agentware";

pub struct Cgroup {
    path: PathBuf,
}

impl Cgroup {
    /// Create the hierarchy root. Called once at boot.
    pub fn init_root() -> io::Result<()> {
        fs::create_dir_all(ROOT)
    }

    pub fn create(name: &str) -> io::Result<Self> {
        let path = PathBuf::from(ROOT).join(name);
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    /// Open this cgroup's `cgroup.procs` for writing.
    ///
    /// Opened in the parent *before* forking, so the child can enrol itself
    /// between fork and exec. Doing it that way closes the window in which a
    /// child exists outside the boundary that is supposed to contain it.
    pub fn open_procs(&self) -> io::Result<OwnedFd> {
        let file = fs::OpenOptions::new().write(true).open(self.path.join("cgroup.procs"))?;
        Ok(file.into())
    }

    /// Kill every process in the cgroup, atomically.
    ///
    /// This is the backstop after `SIGTERM` and a grace period, not the first
    /// resort. Requires Linux 5.14 or later.
    pub fn kill(&self) -> io::Result<()> {
        fs::OpenOptions::new()
            .write(true)
            .open(self.path.join("cgroup.kill"))?
            .write_all(b"1")
    }

    /// Remove the cgroup directory. Only succeeds once it is empty, which is
    /// why it is called after the last member has been reaped.
    pub fn remove(&self) {
        if let Err(err) = fs::remove_dir(&self.path) {
            kwarn!("could not remove cgroup {}: {err}", self.path.display());
        }
    }
}

/// Enrol the calling process in the cgroup behind `procs_fd`.
///
/// Runs between `fork` and `exec`, so it may only use async-signal-safe calls.
/// That rules out `format!` and anything that allocates, hence the hand-rolled
/// integer formatting into a stack buffer.
///
/// # Safety
/// Must only be called in a forked child before `exec`.
pub unsafe fn join_from_child(procs_fd: &OwnedFd) -> io::Result<()> {
    // SAFETY: getpid is async-signal-safe and takes no arguments.
    let pid = unsafe { libc::getpid() };

    let mut buf = [0u8; 24];
    let text = format_pid(pid, &mut buf);

    // SAFETY: writing a short, initialised slice to a valid fd we own.
    let written = unsafe {
        libc::write(procs_fd.as_raw_fd(), text.as_ptr().cast(), text.len())
    };

    if written < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Render a pid as decimal bytes without allocating.
fn format_pid(pid: libc::pid_t, buf: &mut [u8; 24]) -> &[u8] {
    if pid == 0 {
        buf[0] = b'0';
        return &buf[..1];
    }

    let mut digits = [0u8; 24];
    let mut n = pid as u64;
    let mut count = 0;
    while n > 0 {
        digits[count] = b'0' + (n % 10) as u8;
        n /= 10;
        count += 1;
    }

    for i in 0..count {
        buf[i] = digits[count - 1 - i];
    }
    &buf[..count]
}
