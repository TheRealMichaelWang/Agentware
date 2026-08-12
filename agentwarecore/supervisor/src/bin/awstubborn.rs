//! A stand-in app that refuses to shut down politely.
//!
//! It ignores `SIGTERM` and runs forever, so closing an agentdesk that contains
//! one has to escalate to `cgroup.kill`. Without something like this in the
//! self-test, the escalation path is code that only ever runs in production, on
//! the day something has already gone wrong.

use std::time::Duration;

fn main() {
    // SAFETY: setting a signal disposition to SIG_IGN with a valid signal
    // number. No handler function is installed, so there is nothing for the
    // kernel to call back into.
    unsafe {
        libc::signal(libc::SIGTERM, libc::SIG_IGN);
        libc::signal(libc::SIGINT, libc::SIG_IGN);
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
    }

    eprintln!("awstubborn: ignoring SIGTERM, waiting to be killed the hard way");

    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}
