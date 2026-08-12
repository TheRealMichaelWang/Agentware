//! A stand-in compositor, used to prove the descriptor handoff works before the
//! real `ui-manager` exists.
//!
//! It does what `ui-manager` will do at startup: connect to the supervisor's
//! control socket and register as `ui-manager`. Registering is what marks it
//! ready, so anything that depends on the compositor starts only after this.
//!
//! Then it waits for the supervisor to push it descriptors. Each one arrives as
//! ancillary data on a `desk-attached` frame and is already connected to a new
//! agentdesk. Reading a message off one proves the workspace and the compositor
//! are talking over a socket neither of them ever opened by path.
//!
//! Usage: awui [expected_desks]

use std::io::{IoSliceMut, Read, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use rustix::net::{
    RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, recvmsg,
};

const SOCKET: &str = "/run/agentware/sup.sock";

/// A bound on every blocking read, so a broken handoff fails the self-test
/// instead of hanging the machine until someone notices.
const TIMEOUT: Duration = Duration::from_secs(10);

fn main() {
    let expected: usize = std::env::args()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(2);

    let mut stream = match UnixStream::connect(SOCKET) {
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
    eprintln!("awui: registered as ui-manager");

    let mut received = 0;
    let mut failures = 0;

    while received < expected {
        match accept_handoff(&stream) {
            Ok((fields, fd)) => {
                received += 1;
                match read_greeting(fd) {
                    Ok(text) => {
                        eprintln!("awui: PASS {} -> workspace says {text:?}", fields.join(" "));
                    }
                    Err(err) => {
                        eprintln!("awui: FAIL {} -> {err}", fields.join(" "));
                        failures += 1;
                    }
                }
            }
            Err(err) => {
                eprintln!("awui: FAIL waiting for a workspace handoff: {err}");
                failures += 1;
                break;
            }
        }
    }

    if failures == 0 && received == expected {
        eprintln!("awui: all {received} workspace handoff(s) arrived over passed descriptors");
        std::process::exit(0);
    }

    eprintln!("awui: {failures} failure(s), {received}/{expected} handoffs received");
    std::process::exit(1);
}

fn register(stream: &mut UnixStream) -> Result<(), String> {
    let body = "register\0ui-manager";
    let mut frame = (body.len() as u32).to_le_bytes().to_vec();
    frame.extend_from_slice(body.as_bytes());
    stream.write_all(&frame).map_err(|err| format!("register write: {err}"))?;

    let mut header = [0u8; 4];
    stream.read_exact(&mut header).map_err(|err| format!("register reply: {err}"))?;
    let len = u32::from_le_bytes(header) as usize;
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).map_err(|err| format!("register reply body: {err}"))?;

    let text = String::from_utf8_lossy(&body);
    let mut fields = text.split('\0');
    match fields.next() {
        Some("ok") => Ok(()),
        _ => Err(format!("supervisor refused registration: {text}")),
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
