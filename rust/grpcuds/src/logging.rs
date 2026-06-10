// SPDX-License-Identifier: MIT OR Apache-2.0
//! Severity logging with a host-owned sink — the safe-Rust face of the
//! library-wide log facility (`grpcuds_set_log_callback` in C,
//! `grpcuds::SetLogCallback` in C++).
//!
//! The library never formats: every event is a static message + one
//! numeric argument (errno / call id / queue capacity). Unregistered, the
//! library is silent.
//!
//! ```no_run
//! use grpcuds::LogLevel;
//! grpcuds::set_logger(LogLevel::Info, |level, msg, arg| {
//!     eprintln!("grpcuds[{level:?}] {msg} (arg={arg})");
//! });
//! ```

use std::ffi::c_void;
use std::sync::OnceLock;

pub use grpcuds_core::logging::LogLevel;

type Sink = Box<dyn Fn(LogLevel, &str, i64) + Send + Sync>;

static SINK: OnceLock<Sink> = OnceLock::new();

unsafe extern "C" fn trampoline(
    level: i32,
    msg: *const std::ffi::c_char,
    arg: i64,
    _user: *mut c_void,
) {
    let Some(sink) = SINK.get() else { return };
    let level = match level {
        0 => LogLevel::Error,
        1 => LogLevel::Info,
        _ => LogLevel::Debug,
    };
    // Messages are static ASCII literals from the core.
    let msg = unsafe { std::ffi::CStr::from_ptr(msg) }
        .to_str()
        .unwrap_or("");
    sink(level, msg, arg);
}

/// Install the process-global log sink. The sink may fire from the server
/// I/O thread and from any thread using a [`Client`](crate::Client)
/// concurrently. One installation per process: returns `false` (and
/// changes nothing) if a sink is already set.
///
/// The sink runs inside library callbacks across an `extern "C"` boundary:
/// it must not panic (a panic aborts the process) and must not call back
/// into grpcuds.
pub fn set_logger(
    max_level: LogLevel,
    sink: impl Fn(LogLevel, &str, i64) + Send + Sync + 'static,
) -> bool {
    if SINK.set(Box::new(sink)).is_err() {
        return false;
    }
    grpcuds_core::logging::set_log_callback(Some(trampoline), max_level, std::ptr::null_mut());
    true
}
