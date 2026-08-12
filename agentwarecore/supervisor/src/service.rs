//! Stage 4: the service table.
//!
//! A service is a process the supervisor started on purpose and has an opinion
//! about. That opinion is its [`RestartPolicy`], and the policies differ by kind
//! for reasons that come straight out of what Agentware is:
//!
//! * `ui-manager` and `desktop-main` restart forever. If the compositor dies,
//!   the machine is a brick until it comes back, so giving up is never the right
//!   answer.
//! * An `agentdesk` restarts a bounded number of times. It is one workspace
//!   among many, and a permanently broken one should not take the machine with
//!   it.
//! * An agent never restarts. A failed agent is a *result*, to be reported to
//!   the human who asked for it, not a process to resurrect. Restarting it would
//!   silently re-run whatever it was doing, which is the opposite of what the
//!   user wants.
//!
//! Restarts are spaced by exponential backoff. Without it, a service that fails
//! instantly at startup becomes a fork bomb on a machine that has no shell to
//! rescue you with.

use std::path::Path;
use std::process::Command;
use std::os::unix::process::CommandExt;
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, kill_process};

use crate::klog::{kerr, kinfo, kwarn};
use crate::reaper::Exit;
use crate::signals;

/// Delay before the first restart. Each subsequent failure doubles it.
const BACKOFF_BASE: Duration = Duration::from_millis(250);

/// Ceiling on the backoff. A compositor that cannot start should retry every
/// half minute forever rather than drift into retrying once an hour.
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// A service that stays up this long is considered healthy, and its backoff
/// resets to zero. Without this, a service that crashes once a day would
/// eventually be treated as if it were crash looping.
const STABLE_AFTER: Duration = Duration::from_secs(30);

/// Consecutive rapid failures before we say so loudly in the log.
const CRASH_LOOP_THRESHOLD: u32 = 5;

#[derive(Clone, Copy)]
pub enum RestartPolicy {
    /// Restart forever, whatever the exit status. For the graphical stack.
    Always,
    /// Restart at most `max` times, then give up and stay down. For agentdesks.
    Limited { max: u32 },
    /// Never restart. For agents and for anything that is meant to run once.
    Never,
}

enum State {
    /// Defined but not started yet.
    Pending,
    /// The executable does not exist on this system.
    Absent,
    Running { pid: i32, since: Instant },
    /// Waiting out a backoff delay before the next start attempt.
    Backoff { until: Instant },
    /// Exited, and policy says leave it alone.
    Finished,
    /// Gave up restarting.
    Failed,
}

pub struct Service {
    name: &'static str,
    program: &'static str,
    args: &'static [&'static str],
    policy: RestartPolicy,
    state: State,
    /// Consecutive failures since the service was last healthy. Drives backoff.
    attempt: u32,
    /// Total restarts performed, for [`RestartPolicy::Limited`].
    restarts: u32,
}

impl Service {
    pub const fn new(
        name: &'static str,
        program: &'static str,
        args: &'static [&'static str],
        policy: RestartPolicy,
    ) -> Self {
        Self {
            name,
            program,
            args,
            policy,
            state: State::Pending,
            attempt: 0,
            restarts: 0,
        }
    }

    /// True once the service has reached a state it will not leave on its own.
    pub fn is_settled(&self) -> bool {
        matches!(self.state, State::Absent | State::Finished | State::Failed)
    }

    /// Launch the process.
    ///
    /// Everything that has to happen between `fork` and `exec` happens in the
    /// `pre_exec` closure, and only async-signal-safe calls are allowed there.
    fn start(&mut self) {
        let mut command = Command::new(self.program);
        command.args(self.args);

        // SAFETY: the closure runs in the forked child before exec, and calls
        // only sigprocmask, signal, and setsid, all async-signal-safe.
        unsafe {
            command.pre_exec(|| {
                // Without this the child inherits the supervisor's blocked
                // signal mask and cannot be killed with SIGTERM.
                signals::reset_for_child()?;

                // Give the service its own session, so it is isolated from the
                // supervisor's terminal and its whole process group can be
                // signalled as a unit later.
                rustix::process::setsid().map_err(std::io::Error::from)?;

                Ok(())
            });
        }

        match command.spawn() {
            Ok(child) => {
                let pid = child.id() as i32;
                self.state = State::Running { pid, since: Instant::now() };
                kinfo!("{}: started as pid {pid}", self.name);
                // Dropping `Child` does not reap. The supervisor's own waitpid
                // loop collects it, which is what we want: one reaper, not two.
            }
            Err(err) => {
                kerr!("{}: could not start {}: {err}", self.name, self.program);
                // Treat a failed spawn exactly like a failed run, so a missing
                // or corrupt binary backs off instead of spinning.
                self.note_exit(&Exit::Code(-1));
            }
        }
    }

    /// Record that the process ended, and decide what happens next.
    fn note_exit(&mut self, exit: &Exit) {
        if let State::Running { since, .. } = self.state
            && since.elapsed() >= STABLE_AFTER
        {
            // It ran long enough to count as healthy, so this failure is not
            // part of a loop. Start the backoff from scratch.
            self.attempt = 0;
        }

        match self.policy {
            RestartPolicy::Never => {
                kinfo!("{}: {exit}, not restarting (policy is never)", self.name);
                self.state = State::Finished;
            }

            RestartPolicy::Limited { max } if self.restarts >= max => {
                kerr!(
                    "{}: {exit}, giving up after {} restart(s)",
                    self.name,
                    self.restarts
                );
                self.state = State::Failed;
            }

            _ => {
                self.attempt += 1;
                self.restarts += 1;

                if self.attempt == CRASH_LOOP_THRESHOLD {
                    kerr!(
                        "{}: crash looping, {} failures without staying up {}s",
                        self.name,
                        self.attempt,
                        STABLE_AFTER.as_secs()
                    );
                }

                let delay = backoff_for(self.attempt);
                kwarn!(
                    "{}: {exit}, restart {} in {}ms",
                    self.name,
                    self.restarts,
                    delay.as_millis()
                );
                self.state = State::Backoff { until: Instant::now() + delay };
            }
        }
    }

    fn pid(&self) -> Option<i32> {
        match self.state {
            State::Running { pid, .. } => Some(pid),
            _ => None,
        }
    }
}

/// Exponential backoff, capped.
///
/// Attempt 1 waits [`BACKOFF_BASE`], and each further attempt doubles it. The
/// shift is clamped before it is applied, because shifting a u32 by 32 or more
/// is undefined in C and a panic in debug Rust.
fn backoff_for(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(16);
    BACKOFF_BASE.saturating_mul(1u32 << shift).min(BACKOFF_MAX)
}

/// The set of services the supervisor manages.
pub struct Services {
    entries: Vec<Service>,
    /// Set during shutdown, so exiting services are not restarted underneath us.
    stopping: bool,
}

impl Services {
    pub fn new(entries: Vec<Service>) -> Self {
        Self { entries, stopping: false }
    }

    /// Start every service, in table order.
    ///
    /// Services whose executable is missing are skipped rather than retried.
    /// That is what lets the supervisor boot today, before `ui-manager` and
    /// `desktop-main` exist: it says so and carries on instead of crash looping
    /// against a binary that was never built.
    pub fn start_all(&mut self) {
        for service in &mut self.entries {
            if !Path::new(service.program).exists() {
                kwarn!("{}: {} not installed, skipping", service.name, service.program);
                service.state = State::Absent;
                continue;
            }
            service.start();
        }
    }

    /// Start any service whose backoff has expired.
    ///
    /// Returns the earliest deadline still outstanding, which the main loop uses
    /// as its `epoll` timeout. Returning `None` means nothing is waiting on a
    /// clock and the loop can block indefinitely.
    pub fn tick(&mut self) -> Option<Instant> {
        let now = Instant::now();
        let mut next: Option<Instant> = None;

        for service in &mut self.entries {
            let State::Backoff { until } = service.state else {
                continue;
            };

            if now >= until {
                service.start();
            } else {
                next = Some(next.map_or(until, |soonest: Instant| soonest.min(until)));
            }
        }

        next
    }

    /// Match a reaped pid against the table. Returns true if it was ours.
    ///
    /// A false return is normal and not an error: it means an orphaned
    /// grandchild was re-parented to PID 1 and collected. Those have no policy
    /// attached, they just needed reaping.
    pub fn note_exit(&mut self, pid: i32, exit: &Exit) -> bool {
        let Some(service) = self.entries.iter_mut().find(|s| s.pid() == Some(pid)) else {
            return false;
        };

        if self.stopping {
            // Expected during shutdown. Recording a "failure" here would be
            // noise, and restarting would fight the shutdown.
            service.state = State::Finished;
            return true;
        }

        service.note_exit(exit);
        true
    }

    /// True when every service has reached a state it will not leave on its own.
    pub fn all_settled(&self) -> bool {
        self.entries.iter().all(Service::is_settled)
    }

    /// Ask every running service to stop, in reverse table order.
    ///
    /// Reverse order matters: `desktop-main` should go down before the
    /// `ui-manager` it draws through, so it is not writing to a socket that has
    /// just been closed. This only sends the signal; the caller waits.
    pub fn stop_all(&mut self) {
        self.stopping = true;

        for service in self.entries.iter_mut().rev() {
            let Some(raw) = service.pid() else { continue };
            let Some(pid) = Pid::from_raw(raw) else { continue };

            kinfo!("{}: stopping pid {raw}", service.name);
            if let Err(err) = kill_process(pid, Signal::TERM) {
                kwarn!("{}: could not signal pid {raw}: {err}", service.name);
            }
        }
    }
}
