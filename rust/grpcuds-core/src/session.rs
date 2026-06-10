// SPDX-License-Identifier: MIT OR Apache-2.0
//! Safe RAII wrappers over nghttp2 session + callback handles.
//!
//! Both types own a heap-allocated nghttp2 struct and free it on Drop.
//! Constructors return `Result` — panic-free per CLAUDE.md.

use core::ffi::c_void;
use core::ptr;

use grpcuds_sys::{
    nghttp2_option, nghttp2_option_del, nghttp2_option_new, nghttp2_option_set_no_closed_streams,
    nghttp2_session, nghttp2_session_callbacks, nghttp2_session_callbacks_del,
    nghttp2_session_callbacks_new, nghttp2_session_del, nghttp2_session_server_new2,
};

#[cfg_attr(test, derive(Debug))]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SessionError {
    /// nghttp2 reported allocation or invalid-argument failure.
    Alloc,
}

/// Owned `nghttp2_session_callbacks*`. Drop frees it.
pub struct Callbacks {
    ptr: *mut nghttp2_session_callbacks,
}

impl Callbacks {
    pub fn new() -> Result<Self, SessionError> {
        let mut ptr: *mut nghttp2_session_callbacks = ptr::null_mut();
        let rc = unsafe { nghttp2_session_callbacks_new(&mut ptr) };
        if rc != 0 || ptr.is_null() {
            return Err(SessionError::Alloc);
        }
        Ok(Self { ptr })
    }

    #[inline]
    pub fn as_ptr(&self) -> *mut nghttp2_session_callbacks {
        self.ptr
    }
}

impl Drop for Callbacks {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe { nghttp2_session_callbacks_del(self.ptr) };
        }
    }
}

// `Callbacks` is just a pointer to an opaque struct that nghttp2 itself
// is responsible for synchronizing. We never share it across threads in
// practice (single I/O thread; see CLAUDE.md), so neither Send nor Sync.

/// Owned server-side `nghttp2_session*`. Drop frees it.
pub struct Session {
    ptr: *mut nghttp2_session,
}

impl Session {
    /// Create a server session. `user_data` is handed back to every callback.
    ///
    /// # Safety
    ///
    /// `user_data` is dereferenced by the registered nghttp2 callbacks; the
    /// caller must keep the pointee alive (and exclusively reachable through
    /// the callbacks) for the whole session lifetime.
    pub unsafe fn new_server(
        callbacks: &Callbacks,
        user_data: *mut c_void,
    ) -> Result<Self, SessionError> {
        // By default nghttp2 retains every *closed* stream object for the
        // priority dependency tree, pruned against the advertised
        // SETTINGS_MAX_CONCURRENT_STREAMS — which we never advertise, so the
        // retention is effectively unbounded: ~0.25 KB leaked per finished
        // call on a long-lived connection. We don't use stream priorities,
        // so tell nghttp2 to drop closed streams outright.
        let mut opt: *mut nghttp2_option = ptr::null_mut();
        let rc = unsafe { nghttp2_option_new(&mut opt) };
        if rc != 0 || opt.is_null() {
            return Err(SessionError::Alloc);
        }
        let mut ptr: *mut nghttp2_session = ptr::null_mut();
        let rc = unsafe {
            nghttp2_option_set_no_closed_streams(opt, 1);
            let rc = nghttp2_session_server_new2(&mut ptr, callbacks.as_ptr(), user_data, opt);
            nghttp2_option_del(opt);
            rc
        };
        if rc != 0 || ptr.is_null() {
            return Err(SessionError::Alloc);
        }
        Ok(Self { ptr })
    }

    /// Create a client session. `user_data` is handed back to every callback.
    ///
    /// # Safety
    ///
    /// `user_data` is dereferenced by the registered nghttp2 callbacks; the
    /// caller must keep the pointee alive for the whole session lifetime.
    #[cfg(feature = "client")]
    pub unsafe fn new_client(
        callbacks: &Callbacks,
        user_data: *mut c_void,
    ) -> Result<Self, SessionError> {
        let mut ptr: *mut nghttp2_session = ptr::null_mut();
        let rc = unsafe {
            grpcuds_sys::nghttp2_session_client_new(&mut ptr, callbacks.as_ptr(), user_data)
        };
        if rc != 0 || ptr.is_null() {
            return Err(SessionError::Alloc);
        }
        Ok(Self { ptr })
    }

    #[inline]
    pub fn as_ptr(&self) -> *mut nghttp2_session {
        self.ptr
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe { nghttp2_session_del(self.ptr) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callbacks_alloc_and_drop() {
        let cbs = Callbacks::new().expect("alloc callbacks");
        assert!(!cbs.as_ptr().is_null());
    }

    #[test]
    fn server_session_alloc_and_drop() {
        let cbs = Callbacks::new().expect("alloc callbacks");
        // SAFETY: null user_data; no callback dereferences it in this test.
        let sess =
            unsafe { Session::new_server(&cbs, core::ptr::null_mut()) }.expect("alloc session");
        assert!(!sess.as_ptr().is_null());
    }

    #[test]
    fn nghttp2_version_string_reachable() {
        // Forces actual dynamic-link to libnghttp2 at test-run time.
        let v = unsafe { grpcuds_sys::nghttp2_version(0) };
        assert!(!v.is_null(), "libnghttp2 not linked");
    }
}
