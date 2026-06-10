// SPDX-License-Identifier: MIT OR Apache-2.0
//! The logging C ABI — shared by both halves (always present).
#![allow(clippy::missing_safety_doc)]

use core::ffi::{c_char, c_int, c_void};

use grpcuds_core::logging::{self, LogLevel};

fn level_from_c(level: c_int) -> LogLevel {
    match level {
        0 => LogLevel::Error,
        1 => LogLevel::Info,
        _ => LogLevel::Debug,
    }
}

/// Register (or with NULL, remove) the process-global log sink. See
/// grpcuds.h for the full contract.
#[no_mangle]
pub unsafe extern "C" fn grpcuds_set_log_callback(
    callback: Option<
        unsafe extern "C" fn(level: c_int, msg: *const c_char, arg: i64, user_data: *mut c_void),
    >,
    max_level: c_int,
    user_data: *mut c_void,
) {
    logging::set_log_callback(callback, level_from_c(max_level), user_data);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static HITS: AtomicUsize = AtomicUsize::new(0);
    unsafe extern "C" fn count(_l: c_int, msg: *const c_char, _a: i64, _u: *mut c_void) {
        assert!(!msg.is_null());
        HITS.fetch_add(1, Ordering::Relaxed);
    }

    #[test]
    fn registration_round_trip_and_null_unregister() {
        unsafe {
            grpcuds_set_log_callback(Some(count), 2, core::ptr::null_mut());
            grpcuds_core::logging::error(c"abi test", 1);
            assert_eq!(HITS.load(Ordering::Relaxed), 1);
            grpcuds_set_log_callback(None, 2, core::ptr::null_mut());
            grpcuds_core::logging::error(c"silent", 2);
            assert_eq!(HITS.load(Ordering::Relaxed), 1);
        }
    }
}
