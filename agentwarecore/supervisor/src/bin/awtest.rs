//! A stand-in service, used to exercise the supervisor before the real
//! graphical stack exists.
//!
//! It is a separate binary rather than a mode of the supervisor itself, so the
//! spawn path under test is the real one: a distinct executable on disk, found
//! by path, launched with argv, inheriting stdio from PID 1.
//!
//! Usage:
//!   awtest exit <code> [delay_ms]   run for a while, then exit with `code`
//!   awtest abort                    die by SIGABRT, to test signal handling
//!   awtest run                      stay up forever
//!
//! With no arguments it runs forever, so it can stand in for an app launched by
//! name with no argv of its own.

use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("run");

    // Goes to whatever the supervisor handed us, which proves stdio was
    // inherited correctly all the way down to /dev/console.
    eprintln!("awtest: mode {mode}");

    match mode {
        "exit" => {
            let code = parse_arg(&args, 2, 0);
            let delay_ms = parse_arg(&args, 3, 0);
            if delay_ms > 0 {
                std::thread::sleep(Duration::from_millis(delay_ms as u64));
            }
            std::process::exit(code);
        }

        "abort" => std::process::abort(),

        "run" => {
            loop {
                std::thread::sleep(Duration::from_secs(3600));
            }
        }

        _ => {
            eprintln!("awtest: unknown mode {mode:?}");
            std::process::exit(2);
        }
    }
}

fn parse_arg(args: &[String], index: usize, default: i32) -> i32 {
    args.get(index).and_then(|value| value.parse().ok()).unwrap_or(default)
}
