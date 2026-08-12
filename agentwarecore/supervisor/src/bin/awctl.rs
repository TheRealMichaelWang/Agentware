//! Client for the supervisor control socket.
//!
//! Three uses. `awctl <verb> [args...]` issues a single request and prints the
//! reply, which is the only way to poke at the broker on a machine with no
//! shell. `awctl selftest` runs a scripted sequence and exits non-zero if any
//! step misbehaves, which is what the boot-time self-test runs. `awctl demo`
//! opens one workspace with one application in it, which is what puts something
//! on screen while `desktop-main` does not exist.
//!
//! It speaks the protocol over a real socket rather than calling into the
//! supervisor, so the framing, the partial-read handling and the dispatch all
//! get exercised for real.

use std::io::Write;
use std::os::unix::net::UnixStream;

use awproto::{SOCKET_PATH, encode, read_frame};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let code = match args.first().map(String::as_str) {
        None => {
            eprintln!("awctl: usage: awctl <verb> [args...] | awctl selftest");
            2
        }
        Some("selftest") => selftest(),
        Some("demo") => demo(),
        Some(_) => single(&args),
    };

    std::process::exit(code);
}

fn single(args: &[String]) -> i32 {
    let mut conn = match Conn::open() {
        Ok(conn) => conn,
        Err(err) => {
            eprintln!("awctl: {err}");
            return 1;
        }
    };

    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    match conn.request(&borrowed) {
        Ok(reply) => {
            println!("{}", reply.join(" "));
            i32::from(reply.first().map(String::as_str) != Some("ok"))
        }
        Err(err) => {
            eprintln!("awctl: {err}");
            1
        }
    }
}

/// Drive the broker through a full workspace lifecycle.
///
/// The sequence is ordered so each step depends on the last having actually
/// worked, rather than checking a pile of independent calls.
fn selftest() -> i32 {
    let mut conn = match Conn::open() {
        Ok(conn) => conn,
        Err(err) => {
            eprintln!("selftest: cannot reach the supervisor: {err}");
            return 1;
        }
    };

    let mut failures = 0;
    let mut check = |name: &str, outcome: Result<Vec<String>, String>, want_ok: bool| -> Vec<String> {
        match outcome {
            Ok(reply) => {
                let succeeded = reply.first().map(String::as_str) == Some("ok");
                if succeeded == want_ok {
                    println!("selftest: PASS {name} -> {}", reply.join(" "));
                } else {
                    println!("selftest: FAIL {name} -> {}", reply.join(" "));
                    failures += 1;
                }
                reply
            }
            Err(err) => {
                println!("selftest: FAIL {name} -> transport error: {err}");
                failures += 1;
                Vec::new()
            }
        }
    };

    // An empty workspace: no prompt, so no agent process should exist.
    let reply = check("create empty desk", conn.request(&["create-desk"]), true);
    let empty = reply.get(1).cloned().unwrap_or_default();

    // A workspace created from the start menu with an opening prompt. Creating
    // it does not start a turn: the agentdesk asks for an agent itself, which is
    // what the explicit start-agent below stands in for.
    let reply = check("create desk with prompt", conn.request(&["create-desk", "do a thing"]), true);
    let busy = reply.get(1).cloned().unwrap_or_default();

    check("start agent", conn.request(&["start-agent", &busy]), true);

    // Apps are per workspace, so the same app in two desks is two processes.
    check("open app in first desk", conn.request(&["open-app", &empty, "awtest"]), true);
    check("open app in second desk", conn.request(&["open-app", &busy, "awtest"]), true);

    // An app that ignores SIGTERM, to force the cgroup.kill path during close.
    check("open stubborn app", conn.request(&["open-app", &empty, "awstubborn"]), true);

    check("list desks", conn.request(&["list-desks"]), true);

    // One agent *process* per workspace at a time. This is not about refusing
    // the human's messages: what happens to a message that arrives mid-turn is
    // the agentdesk's decision and never reaches the supervisor.
    check("reject second agent process", conn.request(&["start-agent", &busy]), false);

    // Failures that must be rejected rather than crash the supervisor.
    check("reject unknown desk", conn.request(&["close-desk", "9999"]), false);
    check("reject bad app name", conn.request(&["open-app", &empty, "../etc/passwd"]), false);
    check("reject unknown verb", conn.request(&["nonsense"]), false);
    check("reject idle interrupt", conn.request(&["interrupt", &empty]), false);

    check("interrupt running agent", conn.request(&["interrupt", &busy]), true);

    check("close first desk", conn.request(&["close-desk", &empty]), true);
    check("close second desk", conn.request(&["close-desk", &busy]), true);

    if failures == 0 {
        println!("selftest: all control socket checks passed");
        0
    } else {
        println!("selftest: {failures} check(s) failed");
        1
    }
}

/// Put a workspace and an application on screen.
///
/// `desktop-main` is what will do this, and it does not exist. Until it does,
/// the compositor has nothing to render and the display half of the system
/// cannot be looked at. This is the smallest thing that fills that gap, and it
/// goes through the real broker: the workspace and the app are forked by PID 1,
/// their descriptors are pushed to the haimanager by the supervisor, and nothing
/// here is aware of any of it.
fn demo() -> i32 {
    let mut conn = match Conn::open() {
        Ok(conn) => conn,
        Err(err) => {
            eprintln!("demo: cannot reach the supervisor: {err}");
            return 1;
        }
    };

    let reply = match conn.request(&["create-desk"]) {
        Ok(reply) => reply,
        Err(err) => {
            eprintln!("demo: create-desk: {err}");
            return 1;
        }
    };
    if reply.first().map(String::as_str) != Some("ok") {
        eprintln!("demo: create-desk refused: {}", reply.join(" "));
        return 1;
    }

    let desk = reply.get(1).cloned().unwrap_or_default();
    println!("demo: workspace {desk} created");

    match conn.request(&["open-app", &desk, "awapp"]) {
        Ok(reply) if reply.first().map(String::as_str) == Some("ok") => {
            println!("demo: awapp opened in workspace {desk}");
            0
        }
        Ok(reply) => {
            eprintln!("demo: open-app refused: {}", reply.join(" "));
            1
        }
        Err(err) => {
            eprintln!("demo: open-app: {err}");
            1
        }
    }
}

struct Conn {
    stream: UnixStream,
}

impl Conn {
    fn open() -> Result<Self, String> {
        UnixStream::connect(SOCKET_PATH)
            .map(|stream| Self { stream })
            .map_err(|err| format!("connect to {SOCKET_PATH}: {err}"))
    }

    fn request(&mut self, fields: &[&str]) -> Result<Vec<String>, String> {
        self.stream.write_all(&encode(fields)).map_err(|err| format!("write: {err}"))?;
        read_frame(&mut self.stream).map_err(|err| format!("read reply: {err}"))
    }
}
