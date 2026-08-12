//! Boot-time acceptance check, enabled with `agentware.selftest` on the kernel
//! command line.
//!
//! The real graphical stack does not exist yet, so without this the service
//! table, the backoff logic and the shutdown path never run and never get
//! tested. This substitutes a table of `awtest` processes with known behaviour,
//! and powers the machine off once they have all reached a final state.
//!
//! QEMU exiting on its own is the pass signal. A hang means something in the
//! chain is stuck.

use crate::service::{RestartPolicy, Service};

const AWTEST: &str = "/bin/awtest";

/// True if the kernel command line asked for the self-test.
pub fn requested() -> bool {
    std::fs::read_to_string("/proc/cmdline")
        .map(|line| line.split_whitespace().any(|word| word == "agentware.selftest"))
        .unwrap_or(false)
}

/// Services that between them cover every branch of the restart logic.
///
/// `flapper` is the interesting one. It fails instantly and forever, so it
/// exercises the whole backoff curve (250ms, 500ms, 1s) and then the give-up
/// path. Without backoff it would be an unkillable fork bomb, which is exactly
/// the failure this table is here to prove cannot happen.
pub fn services() -> Vec<Service> {
    vec![
        // Fails immediately, every time. Should back off with a doubling delay
        // and stop after three restarts.
        Service::new("flapper", AWTEST, &["exit", "1"], RestartPolicy::Limited { max: 3 }),
        // Dies by signal rather than exit status, so the reaper's
        // classification of SIGABRT gets exercised too.
        Service::new("crasher", AWTEST, &["abort"], RestartPolicy::Limited { max: 1 }),
        // Runs briefly and exits cleanly. Should be left alone, not restarted.
        Service::new("oneshot", AWTEST, &["exit", "0", "100"], RestartPolicy::Never),
    ]
}
