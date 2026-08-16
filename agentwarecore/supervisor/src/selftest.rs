//! Boot-time acceptance check, enabled with `agentware.selftest` on the kernel
//! command line.
//!
//! The real userland does not exist yet, so without this the service table, the
//! backoff logic, the readiness gate, the control socket, the descriptor handoff
//! and the teardown path would never run and never be tested. This substitutes
//! stand-in binaries with known behaviour and powers the machine off once
//! everything has reached a final state.
//!
//! QEMU exiting on its own is the pass signal. A hang means something in the
//! chain is stuck.

use awproto::ROLE_HAIMANAGER;

use crate::desk::Programs;
use crate::service::{RestartPolicy, Service};

const AWTEST: &str = "/bin/awtest";
const AWCTL: &str = "/bin/awctl";
const AWUI: &str = "/bin/awui";

/// True if the kernel command line asked for the self-test.
pub fn requested() -> bool {
    std::fs::read_to_string("/proc/cmdline")
        .map(|line| line.split_whitespace().any(|word| word == "agentware.selftest"))
        .unwrap_or(false)
}

/// Point every process kind the broker spawns at a stand-in.
///
/// The agent exits on its own after 200ms, which is what a completed turn looks
/// like from the supervisor's side.
pub fn programs() -> Programs {
    Programs {
        desk: (AWTEST.into(), vec!["desk".into()]),
        agent: (AWTEST.into(), vec!["exit".into(), "0".into(), "200".into()]),
        // The stand-in apps live in the real app directory in the real package
        // format, so the self-test exercises the same spawn path the system
        // uses: {app_dir}/{name}/exec.
        app_dir: "/apps".into(),
    }
}

/// Services that between them cover every branch of the restart logic, plus the
/// client that drives the control socket.
///
/// `flapper` is the interesting one for restarts. It fails instantly and
/// forever, so it exercises the whole backoff curve (250ms, 500ms, 1s) and then
/// the give-up path. Without backoff it would be an unkillable fork bomb, which
/// is exactly the failure this table is here to prove cannot happen.
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
        // Stands in for the compositor. It registers on the control socket,
        // which is what marks it ready, then receives one descriptor per process
        // that needs to reach the display. The sequence below creates two
        // workspaces, three apps and one agent, so it expects six.
        //
        // The real haimanager restarts forever; this one exits when it has seen
        // what it came for, so the self-test can finish.
        Service::new(ROLE_HAIMANAGER, AWUI, &["6"], RestartPolicy::Never),
        // Drives a full workspace lifecycle over the control socket. Held back
        // until the compositor has registered, so the workspaces it creates have
        // somewhere to be handed to.
        Service::new("control", AWCTL, &["selftest"], RestartPolicy::Never)
            .requires(ROLE_HAIMANAGER),
    ]
}
