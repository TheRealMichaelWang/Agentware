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
//!   create-desk [prompt]
//!   open-app     <desk> <app>
//!   start-agent  <desk>
//!   interrupt    <desk>
//!   close-desk   <desk>
//!   list-desks
//!
//! Responses are `ok` followed by zero or more result fields, or `err`
//! followed by a message.

use std::fmt;

/// Longest frame the supervisor will accept.
///
/// Without a ceiling, a buggy or hostile client could make PID 1 allocate until
/// the machine dies. 64 KiB is far more than any command needs.
pub const MAX_FRAME: usize = 64 * 1024;

const HEADER: usize = 4;

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
            ProtoError::TooLarge(n) => write!(f, "frame of {n} bytes exceeds the {MAX_FRAME} limit"),
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
#[derive(Default)]
pub struct Decoder {
    buf: Vec<u8>,
}

impl Decoder {
    pub fn feed(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Pull the next complete frame, if one has arrived.
    pub fn next_frame(&mut self) -> Result<Option<Vec<String>>, ProtoError> {
        if self.buf.len() < HEADER {
            return Ok(None);
        }

        let len = u32::from_le_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]) as usize;
        if len > MAX_FRAME {
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
