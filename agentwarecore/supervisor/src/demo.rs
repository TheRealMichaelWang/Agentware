//! Boot-time demonstration, enabled with `agentware.demo` on the kernel command
//! line.
//!
//! `startmenu` does not exist, so nothing asks the broker to create a
//! workspace, so the haimanager comes up owning a display with no clients on it.
//! The whole graphical half of the system would then be unobservable, and
//! graphics cannot be checked from a serial log.
//!
//! This substitutes the two pieces that are missing and nothing else. The
//! agentdesk becomes `awapp desk`, the reference client wearing the workspace's
//! face, and a small control-socket client stands in for the start menu by
//! asking for one workspace with one application in it. Everything between those
//! two ends is the real thing: PID 1 forks both processes into a cgroup, hands
//! each a socketpair to the compositor, and pushes the other end over the control
//! socket tagged with the workspace it belongs to.
//!
//! It goes away when `startmenu` and a real agentdesk exist.

use awproto::ROLE_HAIMANAGER;

use crate::desk::Programs;
use crate::service::{RestartPolicy, Service};

const AWCTL: &str = "/bin/awctl";
const AWAPP: &str = "/bin/awapp";
const AWAGENT: &str = "/bin/awagent";
const HAIMANAGER: &str = "/bin/haimanager";

/// True if the kernel command line asked for the demonstration.
pub fn requested() -> bool {
    std::fs::read_to_string("/proc/cmdline")
        .map(|line| line.split_whitespace().any(|word| word == "agentware.demo"))
        .unwrap_or(false)
}

/// The workspace process and the agent are both stand-ins.
///
/// The agent is forked on request from the agentdesk, exactly as a real one
/// would be, with a descriptor to the compositor and a private channel back to
/// the workspace that asked for it.
pub fn programs() -> Programs {
    Programs {
        desk: (AWAPP.into(), vec!["desk".into()]),
        agent: (AWAGENT.into(), vec![]),
        app_dir: "/bin".into(),
    }
}

/// The real compositor, plus a stand-in for the start menu.
///
/// The stand-in waits for the compositor to have registered rather than merely
/// to have been forked, or it would create a workspace with nowhere to hand the
/// descriptor to and the window would never appear.
pub fn services() -> Vec<Service> {
    vec![
        Service::new(ROLE_HAIMANAGER, HAIMANAGER, &[], RestartPolicy::Always),
        Service::new("demo", AWCTL, &["demo"], RestartPolicy::Never).requires(ROLE_HAIMANAGER),
    ]
}
