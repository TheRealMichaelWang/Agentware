//! Wire format for the supervisor control socket.
//!
//! Deliberately hand-rolled rather than pulled from a serialization crate. The
//! supervisor is the one process that may not crash, so its parser is worth
//! being able to read in full, and PID 1 gains nothing from a derive macro.
//!
//! A frame is a little-endian `u32` length followed by that many bytes. The
//! payload is UTF-8 fields separated by NUL. NUL cannot appear inside UTF-8
//! text, so no escaping is needed and a prompt may contain newlines, quotes, or
//! anything else a human might type.
//!
//! Requests:
//!   register     <role>
//!   create-desk  [prompt]
//!   open-app     <desk> <app>
//!   start-agent  <desk>
//!   interrupt    <desk>
//!   close-desk   <desk>
//!   list-desks
//!   poweroff
//!   reboot
//!
//! Responses are `ok` followed by zero or more result fields, or `err`
//! followed by a message.
//!
//! This is a library rather than a module of the supervisor so that the
//! stand-in binaries speak the same protocol by construction instead of by
//! copy-paste.
//!
//! The same framing carries the display protocol, in [`display`], the agent's
//! surface in [`agent`], and the agentdesk's private channel to its agent in
//! [`turn`]. None of those touch PID 1, but they live here for the same
//! reason: two processes that must agree on a wire should read it out of one
//! file. [`settings`] is the one contract that is a file rather than a wire.

pub mod agent;
pub mod broker;
pub mod display;
pub mod pace;
pub mod settings;
pub mod theme;
pub mod turn;

use std::fmt;
use std::io::{self, Read};

/// Where the supervisor listens.
pub const SOCKET_PATH: &str = "/run/agentware/sup.sock";

/// Names the descriptor connecting a process to the haimanager, by number.
/// Every agentdesk, app and agent is handed one at spawn.
pub const HAI_FD_ENV: &str = "AGENTWARE_HAI_FD";

/// Names the descriptor connecting an agent to its owning agentdesk. Agents
/// only. Conversation history arrives down it and telemetry goes back up it,
/// which is why neither ever touches PID 1 or the filesystem.
pub const DESK_FD_ENV: &str = "AGENTWARE_DESK_FD";

/// The role the compositor claims. Registering under it is what marks it ready
/// and what makes it eligible to receive workspace descriptors.
pub const ROLE_HAIMANAGER: &str = "haimanager";

/// Longest frame the supervisor will accept.
///
/// Without a ceiling, a buggy or hostile client could make PID 1 allocate until
/// the machine dies. 64 KiB is far more than any command needs.
pub const MAX_FRAME: usize = 64 * 1024;

pub(crate) const HEADER: usize = 4;

#[derive(Debug)]
pub enum ProtoError {
    /// The declared length exceeds `MAX_FRAME`. The connection is unusable
    /// after this, because the stream can no longer be resynchronised.
    TooLarge(usize),
    NotUtf8,
}

impl fmt::Display for ProtoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtoError::TooLarge(n) => write!(f, "frame of {n} bytes exceeds the limit"),
            ProtoError::NotUtf8 => write!(f, "frame is not valid UTF-8"),
        }
    }
}

/// Build a frame from a list of fields.
pub fn encode(fields: &[&str]) -> Vec<u8> {
    let body = fields.join("\0");
    let mut frame = Vec::with_capacity(HEADER + body.len());
    frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
    frame.extend_from_slice(body.as_bytes());
    frame
}

/// Accumulates bytes off a socket and hands back whole frames.
///
/// A stream socket splits and merges writes freely, so a read may contain half
/// a frame, three frames, or two and a half. Reassembly has to live somewhere,
/// and this is it.
pub struct Decoder {
    buf: Vec<u8>,
    /// Longest frame this decoder will accept. The control socket wants a tight
    /// ceiling because PID 1 must not be made to allocate; a display connection
    /// carries whole documents and needs a looser one.
    limit: usize,
}

impl Default for Decoder {
    fn default() -> Self {
        Self { buf: Vec::new(), limit: MAX_FRAME }
    }
}

impl Decoder {
    pub fn with_limit(limit: usize) -> Self {
        Self { buf: Vec::new(), limit }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Pull the next complete frame, if one has arrived.
    pub fn next_frame(&mut self) -> Result<Option<Vec<String>>, ProtoError> {
        if self.buf.len() < HEADER {
            return Ok(None);
        }

        let len = u32::from_le_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]) as usize;
        if len > self.limit {
            return Err(ProtoError::TooLarge(len));
        }
        if self.buf.len() < HEADER + len {
            return Ok(None);
        }

        let body = self.buf[HEADER..HEADER + len].to_vec();
        self.buf.drain(..HEADER + len);

        let text = String::from_utf8(body).map_err(|_| ProtoError::NotUtf8)?;
        Ok(Some(text.split('\0').map(str::to_owned).collect()))
    }
}

/// Read one whole frame from a blocking stream.
///
/// Only for clients. The supervisor never blocks on a read, so it uses
/// [`Decoder`] against non-blocking sockets instead.
pub fn read_frame(source: &mut impl Read) -> io::Result<Vec<String>> {
    let mut header = [0u8; HEADER];
    source.read_exact(&mut header)?;

    let len = u32::from_le_bytes(header) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::other(ProtoError::TooLarge(len).to_string()));
    }

    let mut body = vec![0u8; len];
    source.read_exact(&mut body)?;

    let text = String::from_utf8(body)
        .map_err(|_| io::Error::other(ProtoError::NotUtf8.to_string()))?;
    Ok(text.split('\0').map(str::to_owned).collect())
}
