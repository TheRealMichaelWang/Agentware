//! Logging for a system that has no log daemon, no terminal, and no shell.
//!
//! Everything goes to `/dev/kmsg`, which puts supervisor messages into the
//! kernel ring buffer alongside kernel messages. They are timestamped, they
//! survive into `dmesg`, and they are automatically mirrored to every console
//! the kernel knows about (both `tty0` and `ttyS0` for us).
//!
//! `/dev/kmsg` only exists once devtmpfs is mounted, so until [`open`] is
//! called we fall back to stderr, which the kernel has already wired to
//! `/dev/console` for PID 1.

use std::fmt;
use std::fs::OpenOptions;
use std::io::Write;
use std::sync::Mutex;

/// Syslog severity levels, as understood by the `<N>` prefix on a `/dev/kmsg`
/// record.
pub const ERR: u8 = 3;
pub const WARN: u8 = 4;
pub const INFO: u8 = 6;

static KMSG: Mutex<Option<std::fs::File>> = Mutex::new(None);

/// Switch logging over to `/dev/kmsg`. Call this immediately after devtmpfs is
/// mounted. Failure is not fatal, we just keep using stderr.
pub fn open() {
    match OpenOptions::new().write(true).open("/dev/kmsg") {
        Ok(file) => {
            if let Ok(mut slot) = KMSG.lock() {
                *slot = Some(file);
            }
        }
        Err(err) => {
            eprintln!("supervisor: could not open /dev/kmsg: {err}");
        }
    }
}

/// Write one log record. Each `write` to `/dev/kmsg` is exactly one record, so
/// the message is formatted into a single buffer before it is handed over.
pub fn emit(level: u8, args: fmt::Arguments<'_>) {
    let mut line = String::with_capacity(96);
    let _ = fmt::write(&mut line, args);

    if let Ok(mut slot) = KMSG.lock() {
        if let Some(file) = slot.as_mut() {
            let record = format!("<{level}>supervisor: {line}\n");
            if file.write_all(record.as_bytes()).is_ok() {
                return;
            }
            // The device went away. Drop it and fall through to stderr so we
            // do not lose every message from here on.
            *slot = None;
        }
    }

    eprintln!("supervisor: {line}");
}

macro_rules! kinfo {
    ($($arg:tt)*) => { $crate::klog::emit($crate::klog::INFO, format_args!($($arg)*)) };
}

macro_rules! kwarn {
    ($($arg:tt)*) => { $crate::klog::emit($crate::klog::WARN, format_args!($($arg)*)) };
}

macro_rules! kerr {
    ($($arg:tt)*) => { $crate::klog::emit($crate::klog::ERR, format_args!($($arg)*)) };
}

pub(crate) use {kerr, kinfo, kwarn};
