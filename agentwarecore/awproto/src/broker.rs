//! The supervisor's control socket, from the asking side.
//!
//! Nothing in Agentware forks a process itself. The start menu and the
//! agentdesks ask PID 1, which keeps every process in the system a direct child
//! of the supervisor with a known owner, and keeps the privilege that spawning
//! needs out of everything that is not PID 1.
//!
//! This is the one connection in the system made by path rather than handed over
//! at spawn. It has to be: a process that has not been forked yet cannot be given
//! a descriptor to the thing that will fork it.
//!
//! It lives in the shared crate so that the agentdesk, the start menu and the
//! control client all speak one implementation. The alternative is three, and
//! the third one to be written is the one that gets a field order wrong.

use std::io::{self, IoSliceMut, Write};
use std::mem::MaybeUninit;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;

use rustix::net::{self, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags};

use crate::{HEADER, MAX_FRAME, SOCKET_PATH, encode, read_frame};

pub struct Broker {
    stream: UnixStream,
}

impl Broker {
    pub fn connect() -> io::Result<Broker> {
        Ok(Broker { stream: UnixStream::connect(SOCKET_PATH)? })
    }

    /// Send one request and wait for its reply.
    ///
    /// Blocking, deliberately. Every caller is a process that has just decided
    /// to do something and has nothing else to be getting on with, and the
    /// supervisor answers without doing any work of its own.
    pub fn request(&mut self, fields: &[&str]) -> Result<Vec<String>, String> {
        self.stream
            .write_all(&encode(fields))
            .map_err(|err| format!("write: {err}"))?;
        read_frame(&mut self.stream).map_err(|err| format!("read reply: {err}"))
    }

    /// A request whose reply must be `ok`, returning the fields after it.
    fn ask(&mut self, fields: &[&str]) -> Result<Vec<String>, String> {
        let reply = self.request(fields)?;
        match reply.first().map(String::as_str) {
            Some("ok") => Ok(reply[1..].to_vec()),
            _ => Err(reply.join(" ")),
        }
    }

    /// Create a workspace, with the prompt the human typed or with nothing.
    ///
    /// Creating one does not start a turn. The agentdesk reads its opening
    /// prompt and asks for an agent itself, which keeps the decision about when
    /// to run a turn with the process that owns the conversation.
    pub fn create_desk(&mut self, prompt: Option<&str>) -> Result<u32, String> {
        let reply = match prompt {
            Some(text) => self.ask(&["create-desk", text])?,
            None => self.ask(&["create-desk"])?,
        };
        reply
            .first()
            .and_then(|id| id.parse().ok())
            .ok_or_else(|| "no workspace id in the reply".to_owned())
    }

    pub fn open_app(&mut self, desk: u32, app: &str) -> Result<i32, String> {
        let reply = self.ask(&["open-app", &desk.to_string(), app])?;
        reply
            .first()
            .and_then(|pid| pid.parse().ok())
            .ok_or_else(|| "no pid in the reply".to_owned())
    }

    /// Close one application, leaving the rest of the workspace alone.
    pub fn close_app(&mut self, desk: u32, pid: i32) -> Result<(), String> {
        self.ask(&["close-app", &desk.to_string(), &pid.to_string()]).map(|_| ())
    }

    /// Ask for an agent process to run one turn.
    ///
    /// No prompt and no conversation crosses this call. The supervisor is told
    /// *that* a turn should run, never what it is about.
    ///
    /// The reply carries the agentdesk's end of a private channel to the agent,
    /// attached with `SCM_RIGHTS`, so it is read with `recvmsg` rather than an
    /// ordinary read: a descriptor belongs to the message that explains it, and
    /// reading the bytes without the ancillary data would close it unseen. That
    /// channel is how conversation history goes down and telemetry comes back
    /// up; see [`crate::turn`].
    pub fn start_agent(&mut self, desk: u32) -> Result<(i32, UnixStream), String> {
        self.stream
            .write_all(&encode(&["start-agent", &desk.to_string()]))
            .map_err(|err| format!("write: {err}"))?;

        let (reply, attached) = self.read_reply_with_fd()?;
        match reply.first().map(String::as_str) {
            Some("ok") => {}
            _ => return Err(reply.join(" ")),
        }
        let pid = reply
            .get(1)
            .and_then(|pid| pid.parse().ok())
            .ok_or_else(|| "no pid in the reply".to_owned())?;
        let channel = attached.ok_or_else(|| "no channel to the agent in the reply".to_owned())?;
        Ok((pid, UnixStream::from(channel)))
    }

    /// One reply frame, with the descriptor attached to it if there was one.
    ///
    /// The supervisor sends a frame and its descriptor in one `sendmsg`, and
    /// replies are read one at a time, so the descriptor arriving with the
    /// first bytes of the frame is the descriptor for this frame. Reading the
    /// header separately from the body would still see it on the header.
    fn read_reply_with_fd(&mut self) -> Result<(Vec<String>, Option<OwnedFd>), String> {
        let mut received: Option<OwnedFd> = None;
        let mut buf = Vec::new();
        let mut want = HEADER;

        while buf.len() < want {
            let mut chunk = vec![0u8; want - buf.len()];
            let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
            let mut control = RecvAncillaryBuffer::new(&mut space);
            let got = match net::recvmsg(
                &self.stream,
                &mut [IoSliceMut::new(&mut chunk)],
                &mut control,
                RecvFlags::empty(),
            ) {
                Ok(got) => got,
                Err(rustix::io::Errno::INTR) => continue,
                Err(err) => return Err(format!("read reply: {err}")),
            };
            if got.bytes == 0 {
                return Err("read reply: the supervisor closed the connection".to_owned());
            }
            for message in control.drain() {
                if let RecvAncillaryMessage::ScmRights(fds) = message {
                    for fd in fds {
                        // One descriptor per reply. A second would be a bug on
                        // the sending side, and closing it here is the honest
                        // response.
                        if received.is_none() {
                            received = Some(fd);
                        }
                    }
                }
            }
            buf.extend_from_slice(&chunk[..got.bytes]);
            if buf.len() == HEADER && want == HEADER {
                let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
                if len > MAX_FRAME {
                    return Err(format!("read reply: frame of {len} bytes exceeds the limit"));
                }
                want = HEADER + len;
            }
        }

        let text = String::from_utf8(buf[HEADER..].to_vec())
            .map_err(|_| "read reply: frame is not valid UTF-8".to_owned())?;
        Ok((text.split('\0').map(str::to_owned).collect(), received))
    }

    pub fn interrupt(&mut self, desk: u32) -> Result<(), String> {
        self.ask(&["interrupt", &desk.to_string()]).map(|_| ())
    }

    pub fn close_desk(&mut self, desk: u32) -> Result<(), String> {
        self.ask(&["close-desk", &desk.to_string()]).map(|_| ())
    }
}
