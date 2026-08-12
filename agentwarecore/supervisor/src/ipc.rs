//! The supervisor control socket.
//!
//! `desktop-main` and the agentdesks do not fork processes themselves. They ask
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
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};

use rustix::event::epoll;

use crate::desk::Desks;
use crate::klog::{kerr, kinfo, kwarn};
use crate::proto::{self, Decoder};

pub const SOCKET_PATH: &str = "/run/agentware/sup.sock";

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
}

pub struct Control {
    listener: UnixListener,
    conns: HashMap<RawFd, Conn>,
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
        Ok(Self { listener, conns: HashMap::new() })
    }

    pub fn listener_fd(&self) -> BorrowedFd<'_> {
        self.listener.as_fd()
    }

    /// Accept every connection currently pending.
    pub fn accept_ready(&mut self, epoll: &impl AsFd) {
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if let Err(err) = self.register(stream, epoll) {
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

    fn register(&mut self, stream: UnixStream, epoll: &impl AsFd) -> io::Result<()> {
        stream.set_nonblocking(true)?;
        let fd = stream.as_raw_fd();

        epoll::add(
            epoll,
            &stream,
            epoll::EventData::new_u64(token_for(fd)),
            epoll::EventFlags::IN,
        )?;

        self.conns.insert(fd, Conn { stream, decoder: Decoder::default(), pending: Vec::new() });
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
                        let reply = dispatch(&fields, desks);
                        let borrowed: Vec<&str> = reply.iter().map(String::as_str).collect();
                        self.send(fd, &proto::encode(&borrowed), epoll);
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
        }
    }
}

/// Turn one request into one reply.
///
/// Every failure is an error frame rather than a panic or an exit. A malformed
/// request from a confused client must never be able to take PID 1 down.
fn dispatch(fields: &[String], desks: &mut Desks) -> Vec<String> {
    let verb = fields.first().map(String::as_str).unwrap_or("");
    let arg = |index: usize| fields.get(index).map(String::as_str);

    let parse_id = |index: usize| -> Result<u32, String> {
        arg(index)
            .ok_or_else(|| "missing desk id".to_owned())?
            .parse::<u32>()
            .map_err(|_| "desk id is not a number".to_owned())
    };

    let result: Result<Vec<String>, String> = match verb {
        "create-desk" => {
            let prompt = arg(1).filter(|text| !text.is_empty());
            desks.create(prompt).map(|id| vec![id.to_string()])
        }

        "open-app" => {
            let id = parse_id(1);
            let app = arg(2).ok_or_else(|| "missing app name".to_owned());
            id.and_then(|id| app.and_then(|app| desks.open_app(id, app)))
                .map(|pid| vec![pid.to_string()])
        }

        "start-agent" => parse_id(1)
            .and_then(|id| desks.start_agent(id))
            .map(|pid| vec![pid.to_string()]),

        "interrupt" => parse_id(1).and_then(|id| desks.interrupt(id)).map(|()| vec![]),

        "close-desk" => parse_id(1).and_then(|id| desks.close(id)).map(|()| vec![]),

        "list-desks" => Ok(desks.list()),

        other => Err(format!("unknown request {other:?}")),
    };

    match result {
        Ok(mut data) => {
            let mut reply = vec!["ok".to_owned()];
            reply.append(&mut data);
            reply
        }
        Err(message) => {
            kerr!("control request {verb:?} failed: {message}");
            vec!["err".to_owned(), message]
        }
    }
}
