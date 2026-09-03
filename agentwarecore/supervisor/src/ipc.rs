//! The supervisor control socket.
//!
//! The compositor and the agentdesks do not fork processes themselves. They ask
//! PID 1 over this socket, which is what keeps every process in the system a
//! direct child of the supervisor with a known owner. See ARCHITECTURE.md for
//! why that matters more than it looks.
//!
//! What deliberately does *not* cross this socket is agent telemetry. The
//! stream of thoughts and tool calls filling an agentdesk's side pane is
//! high-volume application data and flows agent to agentdesk directly. Every
//! byte routed through PID 1 is a byte that can wedge the one process that must
//! never wedge.

use std::collections::HashMap;
use std::io::{self, IoSlice, Read, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};

use rustix::event::epoll;
use rustix::net::{self, SendAncillaryBuffer, SendAncillaryMessage};
use rustix::process::{Pid, Signal, kill_process};

use awproto::{self as proto, Decoder, ROLE_HAIMANAGER, SOCKET_PATH};

use crate::desk::Desks;
use crate::klog::{kerr, kinfo, kwarn};

/// epoll token for the listener. Connections use their own fd as their token,
/// which is unique for as long as the fd is open.
pub const TOKEN_LISTENER: u64 = 2;

/// Connection tokens start above the fixed ones so they cannot collide.
const TOKEN_CONN_BASE: u64 = 0x1000;

pub fn token_for(fd: RawFd) -> u64 {
    TOKEN_CONN_BASE + fd as u64
}

pub fn fd_for(token: u64) -> RawFd {
    (token - TOKEN_CONN_BASE) as RawFd
}

pub fn is_connection_token(token: u64) -> bool {
    token >= TOKEN_CONN_BASE
}

struct Conn {
    stream: UnixStream,
    decoder: Decoder,
    /// Bytes written but not yet accepted by the kernel. Almost always empty,
    /// since replies are tiny, but a blocking write in PID 1 is not an option so
    /// the slow path has to exist.
    pending: Vec<u8>,
    /// The role this connection claimed, if it registered as one.
    role: Option<String>,
}

pub struct Control {
    listener: UnixListener,
    conns: HashMap<RawFd, Conn>,
    /// Role name to the connection serving it. A service is "ready" exactly
    /// when it appears here: registering proves it is connected and listening,
    /// which is what dependents actually need to know. A separate readiness
    /// pipe would prove only that a process had been forked.
    roles: HashMap<String, RawFd>,
}

impl Control {
    pub fn bind() -> io::Result<Self> {
        // A stale socket from a previous boot cannot exist on a RAM-backed
        // filesystem, but removing it first costs nothing and makes the code
        // correct if the runtime directory ever becomes persistent.
        let _ = std::fs::remove_file(SOCKET_PATH);

        let listener = UnixListener::bind(SOCKET_PATH)?;
        listener.set_nonblocking(true)?;

        kinfo!("control socket listening on {SOCKET_PATH}");
        Ok(Self { listener, conns: HashMap::new(), roles: HashMap::new() })
    }

    /// True if something has registered as this role and is still connected.
    pub fn is_ready(&self, role: &str) -> bool {
        self.roles.contains_key(role)
    }

    pub fn listener_fd(&self) -> BorrowedFd<'_> {
        self.listener.as_fd()
    }

    /// Accept every connection currently pending.
    pub fn accept_ready(&mut self, epoll: &impl AsFd) {
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if let Err(err) = self.attach(stream, epoll) {
                        kwarn!("could not register control connection: {err}");
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => {
                    kwarn!("control socket accept failed: {err}");
                    break;
                }
            }
        }
    }

    fn attach(&mut self, stream: UnixStream, epoll: &impl AsFd) -> io::Result<()> {
        stream.set_nonblocking(true)?;
        let fd = stream.as_raw_fd();

        epoll::add(
            epoll,
            &stream,
            epoll::EventData::new_u64(token_for(fd)),
            epoll::EventFlags::IN,
        )?;

        self.conns.insert(
            fd,
            Conn { stream, decoder: Decoder::default(), pending: Vec::new(), role: None },
        );
        Ok(())
    }

    /// Claim a role for a connection.
    ///
    /// One connection per role. A second claimant is refused rather than
    /// silently replacing the first, because a compositor quietly losing every
    /// future workspace handoff to an impostor is not a failure anyone would
    /// diagnose quickly.
    fn register(&mut self, fd: RawFd, role: &str) -> Result<(), String> {
        if let Some(existing) = self.roles.get(role) {
            return Err(format!("role {role:?} is already held by connection {existing}"));
        }

        let Some(conn) = self.conns.get_mut(&fd) else {
            return Err("connection has gone away".to_owned());
        };

        if let Some(held) = &conn.role {
            return Err(format!("connection already registered as {held:?}"));
        }

        conn.role = Some(role.to_owned());
        self.roles.insert(role.to_owned(), fd);
        kinfo!("{role} registered and ready");
        Ok(())
    }

    /// Hand an open file descriptor to a registered role.
    ///
    /// The descriptor is sent as ancillary data alongside a normal frame, so the
    /// receiver learns what it has been given in the same message that gives it.
    /// This is done as a single direct `sendmsg` rather than being queued: a
    /// descriptor belongs to one specific message, and appending it to a pending
    /// byte buffer would detach it from the frame that explains it.
    pub fn hand_over(&mut self, role: &str, fields: &[&str], fd: BorrowedFd<'_>) -> Result<(), String> {
        let target = *self.roles.get(role).ok_or_else(|| format!("no {role} is registered"))?;
        self.send_with_fd(target, fields, fd)
    }

    /// Send one frame with a descriptor attached to a specific connection.
    fn send_with_fd(
        &mut self,
        target: RawFd,
        fields: &[&str],
        fd: BorrowedFd<'_>,
    ) -> Result<(), String> {
        let conn = self.conns.get(&target).ok_or_else(|| "connection has gone away".to_owned())?;

        let frame = proto::encode(fields);

        let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
        let mut control = SendAncillaryBuffer::new(&mut space);
        let fds = [fd];
        if !control.push(SendAncillaryMessage::ScmRights(&fds)) {
            return Err("could not build the ancillary message".to_owned());
        }

        net::sendmsg(&conn.stream, &[IoSlice::new(&frame)], &mut control, net::SendFlags::empty())
            .map_err(|err| format!("could not send descriptor: {err}"))?;

        Ok(())
    }

    /// A connection became readable. Parse whatever arrived and answer it.
    pub fn handle_readable(&mut self, fd: RawFd, desks: &mut Desks, epoll: &impl AsFd) {
        let mut buf = [0u8; 4096];

        loop {
            let Some(conn) = self.conns.get_mut(&fd) else { return };

            match conn.stream.read(&mut buf) {
                // Peer hung up.
                Ok(0) => {
                    self.drop_conn(fd, epoll);
                    return;
                }
                Ok(n) => conn.decoder.feed(&buf[..n]),
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => {
                    kwarn!("control connection read failed: {err}");
                    self.drop_conn(fd, epoll);
                    return;
                }
            }

            // Drain every complete frame this read produced.
            loop {
                let Some(conn) = self.conns.get_mut(&fd) else { return };

                match conn.decoder.next_frame() {
                    Ok(Some(fields)) => {
                        let (reply, attached) = self.dispatch(fd, &fields, desks);
                        let borrowed: Vec<&str> = reply.iter().map(String::as_str).collect();

                        match attached {
                            // A descriptor belongs to one specific frame, so a
                            // reply carrying one is sent directly rather than
                            // appended to the pending byte buffer.
                            Some(handed) => {
                                if let Err(err) =
                                    self.send_with_fd(fd, &borrowed, handed.as_fd())
                                {
                                    kwarn!("could not deliver reply descriptor: {err}");
                                }
                            }
                            None => self.send(fd, &proto::encode(&borrowed), epoll),
                        }
                    }
                    Ok(None) => break,
                    Err(err) => {
                        // The stream can no longer be resynchronised, so the
                        // connection goes rather than the supervisor guessing.
                        kwarn!("control connection protocol error: {err}");
                        self.drop_conn(fd, epoll);
                        return;
                    }
                }
            }
        }
    }

    /// A connection became writable. Flush whatever is queued.
    pub fn handle_writable(&mut self, fd: RawFd, epoll: &impl AsFd) {
        self.send(fd, &[], epoll);
    }

    /// Queue bytes and push as much as the kernel will take.
    fn send(&mut self, fd: RawFd, bytes: &[u8], epoll: &impl AsFd) {
        let Some(conn) = self.conns.get_mut(&fd) else { return };
        conn.pending.extend_from_slice(bytes);

        while !conn.pending.is_empty() {
            match conn.stream.write(&conn.pending) {
                Ok(0) => break,
                Ok(n) => {
                    conn.pending.drain(..n);
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) => {
                    kwarn!("control connection write failed: {err}");
                    self.drop_conn(fd, epoll);
                    return;
                }
            }
        }

        // Only ask to be told about writability while there is something left to
        // write, otherwise the loop spins on a permanently writable socket.
        let flags = if conn.pending.is_empty() {
            epoll::EventFlags::IN
        } else {
            epoll::EventFlags::IN | epoll::EventFlags::OUT
        };

        if let Err(err) =
            epoll::modify(epoll, &conn.stream, epoll::EventData::new_u64(token_for(fd)), flags)
        {
            kwarn!("could not update epoll interest: {err}");
        }
    }

    fn drop_conn(&mut self, fd: RawFd, epoll: &impl AsFd) {
        if let Some(conn) = self.conns.remove(&fd) {
            let _ = epoll::delete(epoll, &conn.stream);

            // A role is only held for as long as its holder is connected.
            // Leaving a dead compositor registered would make dependents believe
            // a service is ready when nothing is behind it.
            if let Some(role) = conn.role {
                self.roles.remove(&role);
                kwarn!("{role} disconnected and is no longer ready");
            }
        }
    }


/// Turn one request into one reply.
///
/// Every failure is an error frame rather than a panic or an exit. A malformed
/// request from a confused client must never be able to take PID 1 down.
    fn dispatch(
        &mut self,
        from: RawFd,
        fields: &[String],
        desks: &mut Desks,
    ) -> (Vec<String>, Option<OwnedFd>) {
        let mut attach: Option<OwnedFd> = None;

    let verb = fields.first().map(String::as_str).unwrap_or("");
    let arg = |index: usize| fields.get(index).map(String::as_str);

    let parse_id = |index: usize| -> Result<u32, String> {
        arg(index)
            .ok_or_else(|| "missing desk id".to_owned())?
            .parse::<u32>()
            .map_err(|_| "desk id is not a number".to_owned())
    };

    let result: Result<Vec<String>, String> = match verb {
        "register" => arg(1)
            .ok_or_else(|| "missing role".to_owned())
            .and_then(|role| self.register(from, role))
            .map(|()| vec![]),

        "create-desk" => {
            let prompt = arg(1).filter(|text| !text.is_empty());
            let backend = arg(2).filter(|text| !text.is_empty());
            desks.create(prompt, backend).map(|(id, ui_end)| {
                self.attach_to_display(&["desk-attached", &id.to_string()], ui_end, id);
                vec![id.to_string()]
            })
        }

        "open-app" => {
            let id = parse_id(1);
            let app = arg(2).ok_or_else(|| "missing app name".to_owned());
            id.and_then(|id| app.and_then(|app| desks.open_app(id, app).map(|r| (id, app, r))))
                .map(|(id, app, (pid, ui_end))| {
                    // The workspace id travels with the descriptor so the
                    // haimanager knows which workspace to render the app into
                    // and which agent is allowed to see it.
                    let fields =
                        ["app-attached", &id.to_string(), app, &pid.to_string()].map(String::from);
                    let borrowed: Vec<&str> = fields.iter().map(String::as_str).collect();
                    self.attach_to_display(&borrowed, ui_end, id);
                    vec![pid.to_string()]
                })
        }

        "start-agent" => parse_id(1).and_then(|id| desks.start_agent(id)).map(
            |(pid, ui_end, desk_end)| {
                let fields = ["agent-attached", &id_text(fields), &pid.to_string()].map(String::from);
                let borrowed: Vec<&str> = fields.iter().map(String::as_str).collect();
                self.attach_to_display(&borrowed, ui_end, 0);

                // The agentdesk's private channel to its agent goes back on this
                // reply. Conversation history flows down it and telemetry back
                // up it, so neither ever passes through PID 1.
                attach = Some(desk_end);
                vec![pid.to_string()]
            },
        ),

        // There is deliberately no verb here for closing an application. The
        // cross on a window asks the application, which exits when it is ready,
        // so nothing needs PID 1 to end an app's process and nothing may: a
        // request that can kill an application at any instant is one more way
        // for work to be lost, and the only reason to have kept it was for
        // applications written badly enough to ignore being asked.
        "interrupt" => parse_id(1).and_then(|id| desks.interrupt(id)).map(|()| vec![]),

        "close-desk" => parse_id(1).and_then(|id| desks.close(id)).map(|()| vec![]),

        "list-desks" => Ok(desks.list()),

        // The power controls, from the start menu. The work does not happen
        // here: the verb becomes the signal the machine's own power button
        // would send, picked up by the signalfd in the main loop, where the
        // orderly shutdown runs with the service table in hand. The reply
        // still goes out first, so the compositor is answered rather than
        // hung up on.
        "poweroff" => signal_self(Signal::USR1).map(|()| vec![]),
        "reboot" => signal_self(Signal::USR2).map(|()| vec![]),

        other => Err(format!("unknown request {other:?}")),
    };

    match result {
        Ok(mut data) => {
            let mut reply = vec!["ok".to_owned()];
            reply.append(&mut data);
            (reply, attach)
        }
        Err(message) => {
            kerr!("control request {verb:?} failed: {message}");
            (vec!["err".to_owned(), message], None)
        }
    }
    }

    /// Hand a freshly created process's descriptor to the compositor.
    ///
    /// Failing is not fatal: there is no haimanager yet, and a workspace with no
    /// display is still better than no workspace. It becomes an error worth
    /// acting on once the graphical stack exists.
    fn attach_to_display(&mut self, fields: &[&str], fd: OwnedFd, desk: u32) {
        if let Err(err) = self.hand_over(ROLE_HAIMANAGER, fields, fd.as_fd()) {
            kwarn!("desk {desk}: not attached to a display: {err}");
        }
    }
}

/// The desk id out of a request that has already been parsed once.
fn id_text(fields: &[String]) -> String {
    fields.get(1).cloned().unwrap_or_default()
}

/// Queue a signal to PID 1 itself.
///
/// The handled signals are blocked and read from a signalfd, so this does not
/// interrupt anything: the signal waits in the kernel until the main loop's
/// next pass, which is after the reply to the request that asked for it.
fn signal_self(sig: Signal) -> Result<(), String> {
    let pid = Pid::from_raw(1).ok_or_else(|| "PID 1 is not a pid".to_owned())?;
    kill_process(pid, sig).map_err(|err| format!("could not signal the supervisor: {err}"))
}
