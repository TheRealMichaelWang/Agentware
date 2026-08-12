//! A stand-in compositor, used to prove the descriptor handoff works before the
//! real `haimanager` exists.
//!
//! It does what `haimanager` will do at startup: connect to the supervisor's
//! control socket and register as `haimanager`. Registering is what marks it
//! ready, so anything that depends on the compositor starts only after this.
//!
//! Then it waits for the supervisor to push it descriptors. Each arrives as
//! ancillary data on a frame naming what it is: a `desk-attached`, an
//! `app-attached` or an `agent-attached`. Each descriptor is already connected
//! to the process in question, over a socket neither side ever opened by path.
//!
//! Only workspaces greet, so only those are read from. What matters for the
//! others is that the descriptor arrives at all, tagged with the workspace it
//! belongs to, since that tag is what scopes an agent to its own desk.
//!
//! Usage: awui [expected_attachments]

use std::io::{IoSliceMut, Read, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use awproto::{ROLE_HAIMANAGER, SOCKET_PATH, encode, read_frame};
use rustix::net::{RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, recvmsg};

/// A bound on every blocking read, so a broken handoff fails the self-test
/// instead of hanging the machine until someone notices.
const TIMEOUT: Duration = Duration::from_secs(10);

fn main() {
    let expected: usize = std::env::args()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(2);

    let mut stream = match UnixStream::connect(SOCKET_PATH) {
        Ok(stream) => stream,
        Err(err) => {
            eprintln!("awui: FAIL cannot reach the supervisor: {err}");
            std::process::exit(1);
        }
    };
    let _ = stream.set_read_timeout(Some(TIMEOUT));

    // Registering is the readiness signal. Nothing that needs a compositor
    // starts until this reply comes back.
    if let Err(err) = register(&mut stream) {
        eprintln!("awui: FAIL {err}");
        std::process::exit(1);
    }
    eprintln!("awui: registered as {ROLE_HAIMANAGER}");

    let mut received = 0;
    let mut failures = 0;
    let mut held: Vec<OwnedFd> = Vec::new();

    while received < expected {
        let (fields, fd) = match accept_handoff(&stream) {
            Ok(handoff) => handoff,
            Err(err) => {
                eprintln!("awui: FAIL waiting for a handoff: {err}");
                failures += 1;
                break;
            }
        };
        received += 1;
        let label = fields.join(" ");

        match fields.first().map(String::as_str) {
            // Workspaces greet as soon as they start, so the descriptor can be
            // proven live rather than merely delivered.
            Some("desk-attached") => match read_greeting(fd) {
                Ok(text) => eprintln!("awui: PASS {label} -> workspace says {text:?}"),
                Err(err) => {
                    eprintln!("awui: FAIL {label} -> {err}");
                    failures += 1;
                }
            },

            // Apps and agents are held open. Their arrival, tagged with the
            // workspace, is the whole claim being tested.
            Some("app-attached") | Some("agent-attached") => {
                held.push(fd);
                eprintln!("awui: PASS {label}");
            }

            _ => {
                eprintln!("awui: FAIL unexpected handoff {label}");
                failures += 1;
            }
        }
    }

    if failures == 0 && received == expected {
        eprintln!("awui: all {received} handoff(s) arrived over passed descriptors");
        std::process::exit(0);
    }

    eprintln!("awui: {failures} failure(s), {received}/{expected} handoffs received");
    std::process::exit(1);
}

fn register(stream: &mut UnixStream) -> Result<(), String> {
    stream
        .write_all(&encode(&["register", ROLE_HAIMANAGER]))
        .map_err(|err| format!("register write: {err}"))?;

    let reply = read_frame(stream).map_err(|err| format!("register reply: {err}"))?;
    match reply.first().map(String::as_str) {
        Some("ok") => Ok(()),
        _ => Err(format!("supervisor refused registration: {}", reply.join(" "))),
    }
}

/// Wait for one frame carrying a descriptor.
///
/// The descriptor and the frame explaining it arrive in the same `recvmsg`,
/// which is why the supervisor sends them as one message rather than queueing
/// the bytes and the descriptor separately.
fn accept_handoff(stream: &UnixStream) -> Result<(Vec<String>, OwnedFd), String> {
    let mut buf = [0u8; 1024];
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut control = RecvAncillaryBuffer::new(&mut space);

    let received = recvmsg(
        stream.as_fd(),
        &mut [IoSliceMut::new(&mut buf)],
        &mut control,
        RecvFlags::empty(),
    )
    .map_err(|err| format!("recvmsg: {err}"))?;

    if received.bytes == 0 {
        return Err("the supervisor closed the connection".to_owned());
    }

    let mut handed = None;
    for message in control.drain() {
        if let RecvAncillaryMessage::ScmRights(fds) = message {
            for fd in fds {
                handed = Some(fd);
            }
        }
    }

    let fd = handed.ok_or_else(|| "frame arrived with no descriptor attached".to_owned())?;

    // The 4-byte length header is skipped: what matters here is the fields.
    let body = &buf[4..received.bytes.min(buf.len())];
    let text = String::from_utf8_lossy(body).into_owned();
    Ok((text.split('\0').map(str::to_owned).collect(), fd))
}

fn read_greeting(fd: OwnedFd) -> Result<String, String> {
    let mut stream = UnixStream::from(fd);
    let _ = stream.set_read_timeout(Some(TIMEOUT));

    let mut buf = [0u8; 256];
    let n = stream.read(&mut buf).map_err(|err| format!("read from workspace: {err}"))?;
    if n == 0 {
        return Err("workspace closed the connection without saying anything".to_owned());
    }

    Ok(String::from_utf8_lossy(&buf[..n]).trim().to_owned())
}
