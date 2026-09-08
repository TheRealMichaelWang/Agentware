//! Ending a model exchange because the workspace changed.
//!
//! A model exchange is a streamed HTTP response, and while the harness reads
//! it, it reads nothing else. That was a choice rather than a law: closing
//! the socket mid-stream aborts the generation at the server, and the
//! haimanager's notices sit in their own socket waiting to be read. So a
//! [`Watch`] is a descriptor whose readability means "stop reading the model
//! and look here instead", and the HTTP client waits on both at once.
//!
//! What the model was saying is thrown away. It was being said about a
//! workspace that no longer exists, and the harness starts the exchange again
//! with the fresh state attached; with the conversation's prefix cached at
//! the server, what that costs is the tokens generated so far and one more
//! time to first token, not a prefill.

use std::fmt;
use std::io;
use std::os::fd::{BorrowedFd, RawFd};
use std::time::Duration;

use rustix::event::{PollFd, PollFlags};

/// A descriptor whose readability ends the exchange in progress.
#[derive(Clone, Copy, Debug)]
pub struct Watch {
    fd: RawFd,
}

impl Watch {
    pub fn new(fd: RawFd) -> Watch {
        Watch { fd }
    }

    /// Block until `stream` has something to read, or the watch fires.
    ///
    /// `Ok(())` means the stream is readable and the caller's read will not
    /// block. [`interrupted`] means the watch fired first; the caller should
    /// stop and let whoever owns the watched descriptor read it. Neither
    /// within `patience` is a dead connection, reported as such.
    pub fn wait(&self, stream: BorrowedFd<'_>, patience: Duration) -> io::Result<()> {
        // SAFETY: the descriptor was handed to this process at spawn and is
        // owned by the link that gave it out, which outlives every exchange.
        let watched = unsafe { BorrowedFd::borrow_raw(self.fd) };
        let mut fds = [
            PollFd::new(&stream, PollFlags::IN),
            PollFd::new(&watched, PollFlags::IN),
        ];
        let timeout = rustix::event::Timespec {
            tv_sec: patience.as_secs() as _,
            tv_nsec: patience.subsec_nanos() as _,
        };
        loop {
            match rustix::event::poll(&mut fds, Some(&timeout)) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "the model stopped answering",
                    ));
                }
                Ok(_) => break,
                Err(rustix::io::Errno::INTR) => continue,
                Err(err) => return Err(err.into()),
            }
        }
        // The watch wins a tie. A notice that arrived in the same instant as
        // the next token is a notice about what the token was said against.
        // Hangup and error count as firing: the reader of that descriptor is
        // the one to find out what happened to it.
        if !fds[1].revents().is_empty() {
            return Err(interrupted());
        }
        Ok(())
    }
}

/// The error a read returns when the watch fired.
#[derive(Debug)]
pub struct Interrupted;

impl fmt::Display for Interrupted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the workspace changed")
    }
}

impl std::error::Error for Interrupted {}

/// An I/O error carrying [`Interrupted`], so it travels through every
/// `io::Result` between the socket and the harness unchanged.
///
/// Its kind is `Other`, deliberately: `ErrorKind::Interrupted` means EINTR,
/// which every read loop in this crate retries, and this is the one thing a
/// read loop must not retry.
pub fn interrupted() -> io::Error {
    io::Error::other(Interrupted)
}

/// Whether an error is the watch firing, as opposed to anything else that
/// can go wrong with a socket.
pub fn is_interrupted(err: &io::Error) -> bool {
    err.get_ref().is_some_and(|inner| inner.is::<Interrupted>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::fd::{AsFd, AsRawFd};
    use std::os::unix::net::UnixStream;

    #[test]
    fn a_word_on_the_watched_descriptor_ends_the_wait() {
        let (stream, _quiet_peer) = UnixStream::pair().unwrap();
        let (watched, mut speaker) = UnixStream::pair().unwrap();
        let watch = Watch::new(watched.as_raw_fd());
        speaker.write_all(b"changed").unwrap();

        let err = watch.wait(stream.as_fd(), Duration::from_secs(5)).unwrap_err();
        assert!(is_interrupted(&err), "{err}");
        // Not EINTR: nothing may retry this.
        assert_ne!(err.kind(), io::ErrorKind::Interrupted);
    }

    #[test]
    fn the_stream_being_readable_is_not_an_interruption() {
        let (stream, mut model) = UnixStream::pair().unwrap();
        let (watched, _silent) = UnixStream::pair().unwrap();
        let watch = Watch::new(watched.as_raw_fd());
        model.write_all(b"data: token").unwrap();

        watch.wait(stream.as_fd(), Duration::from_secs(5)).unwrap();
    }

    #[test]
    fn silence_on_both_is_a_timeout_not_an_interruption() {
        let (stream, _model) = UnixStream::pair().unwrap();
        let (watched, _silent) = UnixStream::pair().unwrap();
        let watch = Watch::new(watched.as_raw_fd());

        let err = watch.wait(stream.as_fd(), Duration::from_millis(20)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(!is_interrupted(&err));
    }

    #[test]
    fn a_hangup_on_the_watched_descriptor_fires_too() {
        // The reader of that descriptor is the one to find out what happened
        // to it; the exchange just has to stop.
        let (stream, _model) = UnixStream::pair().unwrap();
        let (watched, speaker) = UnixStream::pair().unwrap();
        let watch = Watch::new(watched.as_raw_fd());
        drop(speaker);

        let err = watch.wait(stream.as_fd(), Duration::from_secs(5)).unwrap_err();
        assert!(is_interrupted(&err), "{err}");
    }
}
