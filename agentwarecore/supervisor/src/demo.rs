//! Boot-time demonstration, enabled with `agentware.demo` on the kernel command
//! line.
//!
//! There is no agent with a model behind it yet, so on a plain boot a message
//! sent from a workspace has nobody to answer it. This substitutes the one
//! piece that is missing and nothing else: the agent becomes `awagent`, a
//! scripted stand-in that opens the calculator and adds two numbers on it
//! whatever it is asked. Everything else is the real thing: the real
//! agentdesk, the real applications, PID 1 forking each into a cgroup and
//! handing the compositor its descriptor. The machine boots to one blank
//! agentdesk either way; the turn runs when someone sends a message.
//!
//! It goes away when a real agent exists.

use awproto::ROLE_HAIMANAGER;

use crate::desk::Programs;
use crate::service::{RestartPolicy, Service};

const AGENTDESK: &str = "/bin/agentdesk";
const AWAGENT: &str = "/bin/awagent";
const HAIMANAGER: &str = "/bin/haimanager";

/// True if the kernel command line asked for the demonstration.
pub fn requested() -> bool {
    std::fs::read_to_string("/proc/cmdline")
        .map(|line| line.split_whitespace().any(|word| word == "agentware.demo"))
        .unwrap_or(false)
}

/// The real workspace process and a stand-in agent.
///
/// The agent is forked on request from the agentdesk, exactly as a real one
/// would be, with a descriptor to the compositor and a private channel back to
/// the workspace that asked for it.
pub fn programs() -> Programs {
    Programs {
        desk: (AGENTDESK.into(), vec![]),
        agent: (AWAGENT.into(), vec![]),
        app_dir: "/apps".into(),
    }
}

/// The real compositor, and nothing else: the first workspace is the
/// compositor's own request, and the demonstration turn is a message away.
pub fn services() -> Vec<Service> {
    vec![Service::new(ROLE_HAIMANAGER, HAIMANAGER, &[], RestartPolicy::Always)]
}
