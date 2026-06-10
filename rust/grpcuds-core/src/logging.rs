// SPDX-License-Identifier: MIT OR Apache-2.0
//! Severity logging with a host-owned sink — the gpr_log shape, sized for
//! a `no_std` core.
//!
//! The library NEVER formats (the no-`core::fmt` invariant): every event
//! is a **static NUL-terminated message + one numeric argument** (errno,
//! stream id, status code, …). Formatting/timestamping/routing is the
//! host's business and costs the host binary, not the library.
//!
//! Unregistered (the default) the library is silent and every call site
//! is a single relaxed load + branch. Registration is process-global,
//! C-ABI-shaped, and always compiled — error-path visibility is wanted in
//! production too, unlike the dev-only `wirelog` feature.
//!
//! The callback may fire from the I/O thread and from any thread using a
//! client, concurrently — the host's sink must be thread-safe (stderr
//! `fprintf`, the C++ wrapper's default sink, qualifies).

use core::ffi::{c_char, c_void};
use core::sync::atomic::{AtomicI32, AtomicPtr, AtomicUsize, Ordering};

/// Severities, C-ABI stable. Matches `grpcuds_log_level` in `grpcuds.h`.
// Debug on a fieldless enum is three static strings, linked only when a
// host actually formats the level — it cannot drag fmt into the .a.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd)]
#[repr(i32)]
pub enum LogLevel {
    Error = 0,
    Info = 1,
    Debug = 2,
}

/// The host sink: `level` is a `LogLevel` value, `msg` a static
/// NUL-terminated string owned by the library (valid forever), `arg` the
/// event's numeric context, `user_data` what was registered.
pub type LogFn =
    unsafe extern "C" fn(level: i32, msg: *const c_char, arg: i64, user_data: *mut c_void);

// fn pointers stored as usize (0 = unregistered) — AtomicPtr<fn> is not a
// thing, and Option<LogFn> has no atomic. The triple is not updated
// atomically as a unit; registering once at startup (the documented
// contract) makes that moot.
static LOG_FN: AtomicUsize = AtomicUsize::new(0);
static LOG_USER: AtomicPtr<c_void> = AtomicPtr::new(core::ptr::null_mut());
static LOG_MAX: AtomicI32 = AtomicI32::new(LogLevel::Error as i32);

/// Register (or with `None`, remove) the process-global log sink.
/// `max_level` is the most verbose level delivered. Call once at startup,
/// before serving traffic.
pub fn set_log_callback(f: Option<LogFn>, max_level: LogLevel, user_data: *mut c_void) {
    LOG_MAX.store(max_level as i32, Ordering::Relaxed);
    LOG_USER.store(user_data, Ordering::Relaxed);
    LOG_FN.store(f.map(|f| f as usize).unwrap_or(0), Ordering::Release);
}

/// Emit one event. `msg` must be NUL-terminated (use `c"…"` literals);
/// the `&'static` bound is what lets the sink keep the pointer.
#[inline]
pub fn log(level: LogLevel, msg: &'static core::ffi::CStr, arg: i64) {
    let f = LOG_FN.load(Ordering::Acquire);
    if f == 0 || (level as i32) > LOG_MAX.load(Ordering::Relaxed) {
        return;
    }
    let user = LOG_USER.load(Ordering::Relaxed);
    // SAFETY: non-zero means a valid LogFn was stored by set_log_callback.
    let f: LogFn = unsafe { core::mem::transmute::<usize, LogFn>(f) };
    unsafe { f(level as i32, msg.as_ptr(), arg, user) };
}

#[inline]
pub fn error(msg: &'static core::ffi::CStr, arg: i64) {
    log(LogLevel::Error, msg, arg);
}
#[inline]
pub fn info(msg: &'static core::ffi::CStr, arg: i64) {
    log(LogLevel::Info, msg, arg);
}
#[inline]
pub fn debug(msg: &'static core::ffi::CStr, arg: i64) {
    log(LogLevel::Debug, msg, arg);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static EVENTS: Mutex<Vec<(i32, std::string::String, i64)>> = Mutex::new(Vec::new());

    unsafe extern "C" fn capture(level: i32, msg: *const c_char, arg: i64, user: *mut c_void) {
        assert_eq!(user as usize, 0xC0FFEE);
        let s = core::ffi::CStr::from_ptr(msg)
            .to_string_lossy()
            .into_owned();
        EVENTS.lock().unwrap().push((level, s, arg));
    }

    /// One test for the whole global-state surface (registration latches
    /// per process; parallel #[test]s would race the registry).
    #[test]
    fn sink_registration_gating_and_payloads() {
        // Unregistered: silent, no crash.
        error(c"before registration", 1);

        set_log_callback(Some(capture), LogLevel::Info, 0xC0FFEE as *mut c_void);
        error(c"accept failed", -13);
        info(c"conn open", 7);
        debug(c"filtered out", 9); // above max_level
        {
            let ev = EVENTS.lock().unwrap();
            assert_eq!(ev.len(), 2);
            assert_eq!(ev[0], (0, "accept failed".into(), -13));
            assert_eq!(ev[1], (1, "conn open".into(), 7));
        }

        // Raising verbosity delivers DEBUG too.
        set_log_callback(Some(capture), LogLevel::Debug, 0xC0FFEE as *mut c_void);
        debug(c"now visible", 3);
        assert_eq!(EVENTS.lock().unwrap().len(), 3);

        // Unregistering silences again.
        set_log_callback(None, LogLevel::Debug, core::ptr::null_mut());
        error(c"after unregister", 0);
        assert_eq!(EVENTS.lock().unwrap().len(), 3);
    }
}
