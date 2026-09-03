//! The agentdesk registry.
//!
//! This is the dynamic half of the supervisor's process tracking. Where
//! `service.rs` holds a fixed table of boot-critical services with restart
//! policies, this holds workspaces that come and go while the machine runs.
//!
//! Three kinds of process live here, with three different lifetimes:
//!
//! * The **agentdesk** process, one per workspace, lives until the human closes
//!   it.
//! * **App** processes, one per app *per workspace*. The same app open in five
//!   agentdesks is five processes, each owned by exactly one workspace.
//! * The **agent** process, at most one per workspace, which exists only while
//!   a single turn is being executed and owns nothing durable.
//!
//! None of them restart. An agentdesk that dies takes its workspace with it, and
//! resurrecting an agent would silently re-run side effects the human did not
//! ask for a second time.

use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::time::{Duration, Instant};

use rustix::net;
use rustix::process::{Pid, Signal, kill_process};

use awproto::{DESK_FD_ENV, HAI_FD_ENV};

use crate::cgroup::{self, Cgroup};
use crate::klog::{kerr, kinfo, kwarn};
use crate::reaper::Exit;
use crate::signals;

/// How long a closing workspace gets between `SIGTERM` and `cgroup.kill`.
const CLOSE_GRACE: Duration = Duration::from_secs(3);

/// How long after `cgroup.kill` before the entry is dropped regardless.
const KILL_GRACE: Duration = Duration::from_secs(2);

/// Where the binaries for each process kind live.
///
/// Held as configuration rather than hardcoded so the self-test can point every
/// kind at a stand-in, and so none of the paths are scattered through the
/// spawning code.
pub struct Programs {
    pub desk: (String, Vec<String>),
    pub agent: (String, Vec<String>),
    /// Directory holding one folder per installed application.
    ///
    /// An app is a package, not a binary: `{app_dir}/{name}/` holds `exec`,
    /// the program itself, beside `icon.svg` and `description.txt`. The
    /// supervisor only ever touches `exec`; the icon is chrome and belongs to
    /// the haimanager, and the description belongs to whatever lists apps to
    /// people and agents. PID 1 stays out of content, images included.
    pub app_dir: String,
}

impl Programs {
    pub fn system() -> Self {
        Self {
            desk: ("/bin/agentdesk".into(), vec![]),
            agent: ("/bin/awagent".into(), vec![]),
            app_dir: "/apps".into(),
        }
    }
}

enum Stage {
    Open,
    /// SIGTERM sent, waiting for members to exit on their own.
    Terminating { until: Instant },
    /// cgroup.kill sent, waiting for the last reaps.
    Killing { until: Instant },
}

struct App {
    name: String,
    pid: i32,
}

pub struct Desk {
    id: u32,
    cgroup: Cgroup,
    desk_pid: Option<i32>,
    apps: Vec<App>,
    agent_pid: Option<i32>,
    stage: Stage,
}

impl Desk {
    /// Every process still alive in this workspace.
    fn pids(&self) -> Vec<i32> {
        let mut pids = Vec::new();
        pids.extend(self.desk_pid);
        pids.extend(self.apps.iter().map(|app| app.pid));
        pids.extend(self.agent_pid);
        pids
    }

    fn is_empty(&self) -> bool {
        self.desk_pid.is_none() && self.apps.is_empty() && self.agent_pid.is_none()
    }

    fn summary(&self) -> String {
        format!(
            "{}:{}:{}",
            self.id,
            self.apps.len(),
            if self.agent_pid.is_some() { "busy" } else { "idle" }
        )
    }
}

pub struct Desks {
    next_id: u32,
    entries: Vec<Desk>,
    programs: Programs,
}

impl Desks {
    pub fn new(programs: Programs) -> Self {
        Self { next_id: 1, entries: Vec::new(), programs }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Create a workspace, optionally with an opening prompt and the backend
    /// configuration it should answer with.
    ///
    /// The prompt is the one the human typed into the start menu, and it is
    /// handed to the agentdesk process, not to an agent. This is the *only*
    /// user text the supervisor ever touches, and it exists solely because a
    /// brand new workspace has no other way to be told what it was created for.
    /// The backend rides along with it for the same reason and is read no more
    /// than the prompt is: what the name means is the agentdesk's business,
    /// and one it does not recognise falls back to the default there.
    ///
    /// Creating a workspace does not start a turn. The agentdesk reads its
    /// opening prompt and asks for an agent itself, via `start-agent`. That
    /// keeps the decision about when to run a turn with the process that owns
    /// the conversation.
    ///
    /// Returns the workspace id and the compositor's end of a socket already
    /// connected to the new agentdesk, which the caller hands to `haimanager`.
    pub fn create(
        &mut self,
        prompt: Option<&str>,
        backend: Option<&str>,
    ) -> Result<(u32, OwnedFd), String> {
        let id = self.next_id;

        let cgroup = Cgroup::create(&format!("desk-{id}"))
            .map_err(|err| format!("could not create cgroup for desk {id}: {err}"))?;

        let (desk_end, ui_end) = ui_socketpair()?;

        let (program, base_args) = &self.programs.desk;
        let mut args = base_args.clone();
        args.push(id.to_string());
        // Positional, so the prompt's slot is filled even when it is empty
        // rather than letting a backend name slide into it.
        if prompt.is_some() || backend.is_some() {
            args.push(prompt.unwrap_or_default().to_owned());
        }
        if let Some(backend) = backend {
            args.push(backend.to_owned());
        }

        let pid = spawn_in(&cgroup, program, &args, vec![(HAI_FD_ENV, desk_end)])
            .map_err(|err| format!("could not start agentdesk: {err}"))?;

        self.next_id += 1;
        self.entries.push(Desk {
            id,
            cgroup,
            desk_pid: Some(pid),
            apps: Vec::new(),
            agent_pid: None,
            stage: Stage::Open,
        });
        kinfo!("desk {id}: created as pid {pid}");

        Ok((id, ui_end))
    }

    /// Fork an app into an existing workspace.
    ///
    /// The app name is validated rather than trusted. Everything on this socket
    /// is a local process today, but PID 1 turning an arbitrary string into a
    /// path is exactly the sort of thing worth refusing on principle.
    /// Returns the app's pid and the haimanager's end of a socket already
    /// connected to it, which the caller hands over so the app is rendered into
    /// the right workspace.
    pub fn open_app(&mut self, id: u32, app: &str) -> Result<(i32, OwnedFd), String> {
        if !is_valid_app_name(app) {
            return Err(format!("invalid app name {app:?}"));
        }

        let app_dir = self.programs.app_dir.clone();
        let desk = self.open_desk_mut(id)?;

        let (app_end, ui_end) = ui_socketpair()?;

        let program = format!("{app_dir}/{app}/exec");
        let pid = spawn_in(&desk.cgroup, &program, &[], vec![(HAI_FD_ENV, app_end)])
            .map_err(|err| format!("could not start {program}: {err}"))?;

        desk.apps.push(App { name: app.to_owned(), pid });
        kinfo!("desk {id}: opened {app} as pid {pid}");
        Ok((pid, ui_end))
    }

    /// Fork an agent process for a workspace, on that workspace's request.
    ///
    /// No prompt and no conversation crosses this call. The supervisor is told
    /// *that* a turn should run, never what it is about. The agentdesk owns the
    /// conversation and hands the agent its context directly, over a connection
    /// PID 1 is not part of.
    ///
    /// One agent process per workspace at a time. That is a structural limit,
    /// not a queueing policy: two agents doing computer use in the same
    /// workspace would fight over the same cursor and the same DOM. What happens
    /// to a message that arrives mid-turn, whether it queues or is injected into
    /// the running agent, is decided by the agentdesk and is invisible from here.
    /// Returns the agent's pid, the haimanager's end of its interface
    /// connection, and the agentdesk's end of a private channel to it.
    ///
    /// Two sockets, because an agent talks to two different things. It reads
    /// workspace state and sends intents to the haimanager, and it receives
    /// conversation history from, and streams telemetry back to, the agentdesk
    /// that asked for it. Neither carries a byte through PID 1, and neither
    /// needs a path on the filesystem, which is what lets an agent be sandboxed
    /// into its own mount namespace later without losing either channel.
    pub fn start_agent(&mut self, id: u32) -> Result<(i32, OwnedFd, OwnedFd), String> {
        let (program, base_args) = self.programs.agent.clone();
        let desk = self.open_desk_mut(id)?;

        if let Some(pid) = desk.agent_pid {
            return Err(format!("desk {id} already has an agent process (pid {pid})"));
        }

        let (agent_ui, ui_end) = ui_socketpair()?;
        let (agent_desk, desk_end) = ui_socketpair()?;

        let mut args = base_args;
        args.push(id.to_string());

        let pid = spawn_in(
            &desk.cgroup,
            &program,
            &args,
            vec![(HAI_FD_ENV, agent_ui), (DESK_FD_ENV, agent_desk)],
        )
        .map_err(|err| format!("could not start agent: {err}"))?;

        desk.agent_pid = Some(pid);
        kinfo!("desk {id}: agent started as pid {pid}");
        Ok((pid, ui_end, desk_end))
    }

    /// Stop the running turn.
    ///
    /// This is the whole implementation of the "interrupt at any exact moment"
    /// promise. It is one signal to a process that owns nothing, because the
    /// workspace, the apps and the conversation all live elsewhere and are
    /// untouched by it.
    pub fn interrupt(&mut self, id: u32) -> Result<(), String> {
        let desk = self.open_desk_mut(id)?;
        let Some(pid) = desk.agent_pid else {
            return Err(format!("desk {id} has no agent running"));
        };

        signal(pid, Signal::TERM);
        kinfo!("desk {id}: interrupted agent pid {pid}");
        Ok(())
    }

    /// Begin closing a workspace.
    ///
    /// Returns as soon as the signals are away. Escalation to `cgroup.kill` and
    /// the final cleanup happen in [`Desks::tick`], so a workspace that refuses
    /// to die cannot stall the supervisor's event loop.
    pub fn close(&mut self, id: u32) -> Result<(), String> {
        let desk = self.open_desk_mut(id)?;

        // Reverse of how a workspace is built up: the agent first since its work
        // is disposable, then the apps so they can flush documents, then the
        // desk itself.
        for pid in desk.agent_pid.iter().chain(desk.apps.iter().map(|app| &app.pid)) {
            signal(*pid, Signal::TERM);
        }
        if let Some(pid) = desk.desk_pid {
            signal(pid, Signal::TERM);
        }

        desk.stage = Stage::Terminating { until: Instant::now() + CLOSE_GRACE };
        kinfo!("desk {id}: closing");
        Ok(())
    }

    pub fn close_all(&mut self) {
        let ids: Vec<u32> = self.entries.iter().map(|desk| desk.id).collect();
        for id in ids {
            let _ = self.close(id);
        }
    }

    pub fn list(&self) -> Vec<String> {
        self.entries.iter().map(Desk::summary).collect()
    }

    /// Drive closing workspaces forward. Returns the next deadline to wake for.
    pub fn tick(&mut self) -> Option<Instant> {
        let now = Instant::now();
        let mut next: Option<Instant> = None;
        let mut finished = Vec::new();

        for desk in &mut self.entries {
            match desk.stage {
                Stage::Open => {}

                Stage::Terminating { until } if now >= until => {
                    kwarn!(
                        "desk {}: {} process(es) still alive after {}s, killing the cgroup",
                        desk.id,
                        desk.pids().len(),
                        CLOSE_GRACE.as_secs()
                    );
                    if let Err(err) = desk.cgroup.kill() {
                        kerr!("desk {}: cgroup.kill failed: {err}", desk.id);
                    }
                    desk.stage = Stage::Killing { until: now + KILL_GRACE };
                    next = Some(next.map_or(now + KILL_GRACE, |at: Instant| at.min(now + KILL_GRACE)));
                }

                Stage::Killing { until } if now >= until => {
                    // Nothing survives SIGKILL in a cgroup except a task stuck
                    // in uninterruptible sleep. Drop the entry rather than let a
                    // wedged driver keep a workspace alive forever.
                    kerr!("desk {}: giving up on {} process(es)", desk.id, desk.pids().len());
                    finished.push(desk.id);
                }

                Stage::Terminating { until } | Stage::Killing { until } => {
                    next = Some(next.map_or(until, |at: Instant| at.min(until)));
                }
            }
        }

        for id in finished {
            self.retire(id);
        }

        next
    }

    /// Match a reaped pid against the registry. Returns true if it was ours.
    pub fn note_exit(&mut self, pid: i32, exit: &Exit) -> bool {
        let Some(desk) = self.entries.iter_mut().find(|d| d.pids().contains(&pid)) else {
            return false;
        };

        let id = desk.id;

        if desk.agent_pid == Some(pid) {
            desk.agent_pid = None;
            kinfo!("desk {id}: agent pid {pid} {exit}");
        } else if let Some(index) = desk.apps.iter().position(|app| app.pid == pid) {
            let app = desk.apps.remove(index);
            kinfo!("desk {id}: app {} pid {pid} {exit}", app.name);
        } else {
            desk.desk_pid = None;
            kinfo!("desk {id}: workspace process pid {pid} {exit}");

            // The workspace process dying takes the whole workspace with it,
            // however it happened. Nothing here restarts, so bring the rest of
            // the members down with it rather than leaving orphaned apps
            // rendering into a workspace that no longer exists.
            if matches!(desk.stage, Stage::Open) {
                kwarn!("desk {id}: workspace process gone, tearing down");
                let _ = self.close(id);
            }
        }

        // Once every member is reaped the entry has nothing left to track.
        if self.entries.iter().any(|d| d.id == id && d.is_empty() && !matches!(d.stage, Stage::Open))
        {
            self.retire(id);
        }

        true
    }

    /// Drop a workspace from the registry and remove its cgroup.
    fn retire(&mut self, id: u32) {
        let Some(index) = self.entries.iter().position(|desk| desk.id == id) else {
            return;
        };
        let desk = self.entries.remove(index);
        desk.cgroup.remove();
        kinfo!("desk {id}: closed");
    }

    fn open_desk_mut(&mut self, id: u32) -> Result<&mut Desk, String> {
        let desk = self
            .entries
            .iter_mut()
            .find(|desk| desk.id == id)
            .ok_or_else(|| format!("no such desk {id}"))?;

        if !matches!(desk.stage, Stage::Open) {
            return Err(format!("desk {id} is closing"));
        }
        Ok(desk)
    }
}

/// Fork a process directly into a workspace's cgroup, optionally handing it an
/// already open file descriptor.
///
/// The `cgroup.procs` handle is opened before the fork and the child enrols
/// itself before `exec`, so there is no window in which the process exists
/// outside the boundary meant to contain it.
///
/// The handoff descriptor is passed by *number* rather than being duplicated
/// onto a fixed slot such as fd 3. A fixed slot risks clobbering whatever the
/// spawn machinery is already using there, and the number is identical either
/// side of `fork` anyway, so telling the child which one to look at is both
/// simpler and safer. All the child needs is for `CLOEXEC` to be cleared so the
/// descriptor survives `exec`.
fn spawn_in(
    cgroup: &Cgroup,
    program: &str,
    args: &[String],
    handoffs: Vec<(&'static str, OwnedFd)>,
) -> std::io::Result<i32> {
    let procs: OwnedFd = cgroup.open_procs()?;

    let mut command = Command::new(program);
    command.args(args);

    let raws: Vec<i32> = handoffs.iter().map(|(_, fd)| fd.as_raw_fd()).collect();
    for (name, fd) in &handoffs {
        command.env(name, fd.as_raw_fd().to_string());
    }

    // SAFETY: the closure runs in the forked child before exec and calls only
    // sigprocmask, signal, setsid, getpid, write and fcntl, all
    // async-signal-safe.
    unsafe {
        command.pre_exec(move || {
            signals::reset_for_child()?;
            rustix::process::setsid().map_err(std::io::Error::from)?;
            cgroup::join_from_child(&procs)?;

            for &raw in &raws {
                // Clearing close-on-exec so the descriptor survives the exec.
                if libc::fcntl(raw, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }

            Ok(())
        });
    }

    let child = command.spawn()?;

    // The parent's copies go now that the child has its own. Holding one would
    // keep the socket alive after the process died, so the peer would never see
    // the hangup that tells it the process is gone.
    drop(handoffs);

    Ok(child.id() as i32)
}

/// One half of a connection for a process that has not been forked yet.
///
/// Created before either side knows the other exists. Neither process ever
/// opens a path, so there is no socket to race against, no filesystem
/// permission to get wrong, and nothing a sandboxed process can reach that it
/// was not explicitly handed.
fn ui_socketpair() -> Result<(OwnedFd, OwnedFd), String> {
    net::socketpair(
        net::AddressFamily::UNIX,
        net::SocketType::STREAM,
        net::SocketFlags::CLOEXEC,
        None,
    )
    .map_err(|err| format!("could not create a socketpair: {err}"))
}

fn signal(pid: i32, sig: Signal) {
    let Some(pid) = Pid::from_raw(pid) else { return };
    if let Err(err) = kill_process(pid, sig) {
        // ESRCH just means it exited between the decision and the signal.
        if err != rustix::io::Errno::SRCH {
            kwarn!("could not signal pid {}: {err}", pid.as_raw_nonzero());
        }
    }
}

/// App names become paths, so they are checked rather than trusted.
fn is_valid_app_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}
