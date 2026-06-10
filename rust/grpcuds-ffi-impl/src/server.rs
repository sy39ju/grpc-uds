// SPDX-License-Identifier: MIT OR Apache-2.0
//! The server C ABI (`grpcuds_server_*` / `grpcuds_conn_*` / `grpcuds_call_*`).
#![allow(clippy::missing_safety_doc)]

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::ffi::{c_char, c_int, c_void, CStr};
use core::ptr;

use core::num::NonZeroUsize;

use grpcuds_core::{
    Backpressure, Conn, Connection, GrpcStatus, IoError, Listener, OverflowPolicy, TickStatus,
};

use crate::mailbox::MAILBOX;

// ---- Opaque handle types --------------------------------------------------

/// Server handle: owns the Listener and the list of registered methods.
pub struct grpcuds_server {
    listener: Option<Listener>,
    // Boxed: each entry's address is registered with the core as user_data.
    #[allow(clippy::vec_box)]
    methods: Vec<Box<CMethodEntry>>,
}

/// Connection handle: owns one accepted [`Connection`].
pub struct grpcuds_conn {
    inner: Connection,
}

/// User's C handler. `call` is opaque — internally it's `*mut Conn` for the
/// active gRPC call; the user passes it back unchanged to
/// `grpcuds_call_write` / `grpcuds_call_finish`.
pub type grpcuds_handler_fn = unsafe extern "C" fn(
    call: *mut c_void,
    call_id: i32,
    req: *const u8,
    req_len: usize,
    user_data: *mut c_void,
) -> c_int;

/// Per-method registration record. Boxed so the address is stable across
/// `methods` Vec growth — the address is passed to nghttp2's trampoline as
/// `user_data` so it must outlive every accepted connection.
struct CMethodEntry {
    path: Vec<u8>,
    user_handler: grpcuds_handler_fn,
    user_data: *mut c_void,
}

// ---- Handler trampoline ---------------------------------------------------
//
// Core's `Conn::dispatch` invokes a `HandlerFn(*mut Conn, ...)`. The user's
// C handler takes `*mut c_void`. This shim does the cast (no-op at the bit
// level — `*mut Conn` and `*mut c_void` have the same layout) and forwards.
unsafe extern "C" fn c_handler_trampoline(
    conn_ptr: *mut Conn,
    call_id: i32,
    req: *const u8,
    req_len: usize,
    user_data: *mut c_void,
) -> c_int {
    let entry = &*(user_data as *const CMethodEntry);
    (entry.user_handler)(
        conn_ptr as *mut c_void,
        call_id,
        req,
        req_len,
        entry.user_data,
    )
}

// ---- Helpers --------------------------------------------------------------

// POSIX errno values (Linux). Returned as negative numbers so the C caller
// can `if (rc < 0) errno = -rc;` if it wants to use `strerror`.
const ERR_NOENT: c_int = -2; //  ENOENT — no such call_id / stream
const ERR_AGAIN: c_int = -11; // EAGAIN — queue full, try again later
const ERR_NOMEM: c_int = -12; // ENOMEM — try_reserve / Box::new failed
const ERR_INVAL: c_int = -22; // EINVAL — null pointer / bad argument
const ERR_PIPE: c_int = -32; //  EPIPE  — stream already finished

fn io_err_to_c(e: IoError) -> c_int {
    match e {
        IoError::Errno(n) => {
            if n > 0 {
                -n
            } else {
                -1
            }
        }
        IoError::InvalidPath => ERR_INVAL,
        IoError::Conn(ce) => conn_err_to_c(ce),
    }
}

fn conn_err_to_c(e: grpcuds_core::ConnError) -> c_int {
    use grpcuds_core::ConnError as E;
    match e {
        E::OutOfMemory => ERR_NOMEM,
        E::StreamNotFound => ERR_NOENT,
        E::StreamFinished => ERR_PIPE,
        E::QueueFull => ERR_AGAIN,
        // Nghttp2 codes are already negative; pass them through unchanged.
        E::Nghttp2(rc) => rc as c_int,
        // Session-init failures don't have a clean POSIX peer; treat as -1.
        E::Session(_) => -1,
    }
}

fn status_from_c(status: c_int) -> GrpcStatus {
    if (0..=16).contains(&status) {
        // SAFETY: GrpcStatus is #[repr(u8)] with discriminants 0..=16.
        unsafe { core::mem::transmute::<u8, GrpcStatus>(status as u8) }
    } else {
        GrpcStatus::Unknown
    }
}

// ---- Server -----------------------------------------------------------------

#[no_mangle]
pub extern "C" fn grpcuds_server_new() -> *mut grpcuds_server {
    let s = Box::new(grpcuds_server {
        listener: None,
        methods: Vec::new(),
    });
    Box::into_raw(s)
}

#[no_mangle]
pub unsafe extern "C" fn grpcuds_server_free(s: *mut grpcuds_server) {
    if !s.is_null() {
        drop(Box::from_raw(s));
    }
}

#[no_mangle]
pub unsafe extern "C" fn grpcuds_server_bind_uds(
    s: *mut grpcuds_server,
    path: *const c_char,
) -> c_int {
    if s.is_null() || path.is_null() {
        return ERR_INVAL;
    }
    let server = &mut *s;
    let bytes = CStr::from_ptr(path).to_bytes();
    match Listener::bind(bytes) {
        Ok(l) => {
            server.listener = Some(l);
            0
        }
        Err(e) => io_err_to_c(e),
    }
}

#[no_mangle]
pub unsafe extern "C" fn grpcuds_server_listener_fd(s: *const grpcuds_server) -> c_int {
    if s.is_null() {
        return ERR_INVAL;
    }
    (*s).listener.as_ref().map(|l| l.fd()).unwrap_or(-1)
}

#[no_mangle]
pub unsafe extern "C" fn grpcuds_server_register_method(
    s: *mut grpcuds_server,
    path: *const c_char,
    handler: Option<grpcuds_handler_fn>,
    user_data: *mut c_void,
) -> c_int {
    if s.is_null() || path.is_null() {
        return ERR_INVAL;
    }
    let Some(handler) = handler else {
        return ERR_INVAL;
    };
    let server = &mut *s;
    let bytes = CStr::from_ptr(path).to_bytes();

    let mut owned = Vec::new();
    if owned.try_reserve(bytes.len()).is_err() {
        return ERR_NOMEM;
    }
    owned.extend_from_slice(bytes);

    let entry = Box::new(CMethodEntry {
        path: owned,
        user_handler: handler,
        user_data,
    });
    if server.methods.try_reserve(1).is_err() {
        return ERR_NOMEM;
    }
    server.methods.push(entry);
    0
}

#[no_mangle]
pub unsafe extern "C" fn grpcuds_server_accept(s: *mut grpcuds_server) -> *mut grpcuds_conn {
    if s.is_null() {
        return ptr::null_mut();
    }
    let server = &mut *s;
    let listener = match &server.listener {
        Some(l) => l,
        None => return ptr::null_mut(),
    };
    match listener.accept() {
        Ok(None) => ptr::null_mut(),
        Ok(Some(mut connection)) => {
            // Push every server-registered method onto the new connection.
            // entry.as_ref() yields a stable pointer because each entry is
            // heap-allocated and the Vec only grows.
            for entry in &server.methods {
                let entry_ptr = entry.as_ref() as *const CMethodEntry as *mut c_void;
                if connection
                    .conn()
                    .register_method(&entry.path, c_handler_trampoline, entry_ptr)
                    .is_err()
                {
                    return ptr::null_mut();
                }
            }
            let raw = Box::into_raw(Box::new(grpcuds_conn { inner: connection }));
            // Clear any tombstone a prior connection at this reused address may
            // have left, so the mailbox does not drop this connection's writes.
            MAILBOX.register_call((*raw).inner.conn() as *mut Conn as *mut c_void);
            raw
        }
        Err(_) => ptr::null_mut(),
    }
}

// ---- Connection ---------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn grpcuds_conn_free(c: *mut grpcuds_conn) {
    if !c.is_null() {
        // Tombstone + scrub any queued mailbox writes for this connection so a
        // producer that raced the free can never replay against freed memory.
        // Runs on the I/O thread (the drain thread), per the THREADING contract.
        MAILBOX.unregister_call((*c).inner.conn() as *mut Conn as *mut c_void);
        drop(Box::from_raw(c));
    }
}

/// The opaque `call` handle this connection's handlers receive — the key
/// hosts use to (un)register the connection with the C++ outbound mailbox
/// so queued writes can never touch a freed connection. Null on null.
#[no_mangle]
pub unsafe extern "C" fn grpcuds_conn_call_handle(c: *mut grpcuds_conn) -> *mut c_void {
    if c.is_null() {
        return core::ptr::null_mut();
    }
    (*c).inner.conn() as *mut Conn as *mut c_void
}

#[no_mangle]
pub unsafe extern "C" fn grpcuds_conn_fd(c: *const grpcuds_conn) -> c_int {
    if c.is_null() {
        return ERR_INVAL;
    }
    (*c).inner.fd()
}

/// Drive one I/O cycle (read + dispatch + opportunistic write). Returns 0
/// if the connection is still alive, 1 when it has closed (caller must
/// `grpcuds_conn_free`), or a negative value on error.
///
/// Equivalent to `grpcuds_conn_tick_read`. Prefer the revents-aware pair
/// (`_tick_read` / `_tick_write`) when the caller has a `poll`-style event
/// loop — skipping the read syscall on POLLOUT-only iterations is the
/// whole reason the split exists.
#[no_mangle]
pub unsafe extern "C" fn grpcuds_conn_tick(c: *mut grpcuds_conn) -> c_int {
    grpcuds_conn_tick_read(c)
}

/// Read-phase tick. Call when `revents` indicates the socket is readable
/// (POLLIN), peer-closed (POLLHUP), or in error (POLLERR). Does:
///   1. Drain readable bytes into nghttp2.
///   2. Run any newly-Complete handler dispatches.
///   3. Opportunistically flush nghttp2's send queue back to the socket.
///
/// Step 3 means a single `_tick_read` per poll iteration is enough when
/// both POLLIN and POLLOUT fired — no need to also call `_tick_write`.
///
/// Returns 0 (alive), 1 (closed), or a negative errno on error.
#[no_mangle]
pub unsafe extern "C" fn grpcuds_conn_tick_read(c: *mut grpcuds_conn) -> c_int {
    if c.is_null() {
        return ERR_INVAL;
    }
    match (*c).inner.tick_read() {
        Ok(TickStatus::Live) => 0,
        Ok(TickStatus::Closed) => 1,
        Err(e) => io_err_to_c(e),
    }
}

/// Write-phase tick. Call when `revents` is POLLOUT only (the previous
/// write hit EAGAIN and the event loop re-armed write interest). Skips the
/// read syscall and the dispatch pass; just drains buffered output.
///
/// Returns 0 (alive), 1 (closed), or a negative errno on error.
#[no_mangle]
pub unsafe extern "C" fn grpcuds_conn_tick_write(c: *mut grpcuds_conn) -> c_int {
    if c.is_null() {
        return ERR_INVAL;
    }
    match (*c).inner.tick_write() {
        Ok(TickStatus::Live) => 0,
        Ok(TickStatus::Closed) => 1,
        Err(e) => io_err_to_c(e),
    }
}

/// True (1) iff the connection currently has outbound work — either
/// nghttp2 has frames queued or a previous write left bytes buffered.
/// Use this to decide whether to keep POLLOUT armed.
///
/// Returns 0 if false, 1 if true, or a negative errno on null/invalid.
/// Remaining ms until this connection's earliest `grpc-timeout` deadline:
/// non-negative when one is armed (0 = due now — tick the connection), -1
/// when no in-flight call carries a deadline, -EINVAL on null. See grpcuds.h.
#[no_mangle]
pub unsafe extern "C" fn grpcuds_conn_next_deadline_ms(c: *const grpcuds_conn) -> i64 {
    if c.is_null() {
        return ERR_INVAL as i64;
    }
    match (*c).inner.next_deadline_ms() {
        Some(ms) => ms.min(i64::MAX as u64) as i64,
        None => -1,
    }
}

#[no_mangle]
pub unsafe extern "C" fn grpcuds_conn_wants_write(c: *const grpcuds_conn) -> c_int {
    if c.is_null() {
        return ERR_INVAL;
    }
    if (*c).inner.wants_write() {
        1
    } else {
        0
    }
}

// ---- Call -------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn grpcuds_call_write(
    call: *mut c_void,
    call_id: i32,
    data: *const u8,
    len: usize,
) -> c_int {
    if call.is_null() || (data.is_null() && len > 0) {
        return ERR_INVAL;
    }
    let slice = if len == 0 {
        &[]
    } else {
        core::slice::from_raw_parts(data, len)
    };
    // Always thread-safe (option X): on the registered I/O thread, touch the
    // core directly (honoring backpressure); off it, hand off to the mailbox,
    // drained later on the I/O thread. See docs/THREADING.md.
    if MAILBOX.on_io_thread() {
        let conn = &mut *(call as *mut Conn);
        match conn.write_call(call_id, slice) {
            Ok(()) => 0,
            Err(e) => conn_err_to_c(e),
        }
    } else {
        match MAILBOX.enqueue_write(call, call_id, slice) {
            Ok(()) => 0,
            Err(()) => ERR_NOMEM,
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn grpcuds_call_finish(
    call: *mut c_void,
    call_id: i32,
    status: c_int,
) -> c_int {
    if call.is_null() {
        return ERR_INVAL;
    }
    let st = status_from_c(status);
    // Always thread-safe (option X) — see grpcuds_call_write.
    if MAILBOX.on_io_thread() {
        let conn = &mut *(call as *mut Conn);
        match conn.finish_call(call_id, st) {
            Ok(()) => 0,
            Err(e) => conn_err_to_c(e),
        }
    } else {
        match MAILBOX.enqueue_finish(call, call_id, st, &[]) {
            Ok(()) => 0,
            Err(()) => ERR_NOMEM,
        }
    }
}

/// Like `grpcuds_call_finish`, but also ships a `grpc-message` trailer.
/// `msg`/`msg_len` are the raw (un-encoded) message bytes; the runtime
/// percent-encodes them per the gRPC wire spec. A null `msg` or `msg_len == 0`
/// is equivalent to `grpcuds_call_finish` (status-only trailer).
#[no_mangle]
pub unsafe extern "C" fn grpcuds_call_finish_msg(
    call: *mut c_void,
    call_id: i32,
    status: c_int,
    msg: *const u8,
    msg_len: usize,
) -> c_int {
    if call.is_null() {
        return ERR_INVAL;
    }
    let bytes: &[u8] = if msg.is_null() || msg_len == 0 {
        &[]
    } else {
        core::slice::from_raw_parts(msg, msg_len)
    };
    let st = status_from_c(status);
    // Always thread-safe (option X) — see grpcuds_call_write.
    if MAILBOX.on_io_thread() {
        let conn = &mut *(call as *mut Conn);
        match conn.finish_call_msg(call_id, st, bytes) {
            Ok(()) => 0,
            Err(e) => conn_err_to_c(e),
        }
    } else {
        match MAILBOX.enqueue_finish(call, call_id, st, bytes) {
            Ok(()) => 0,
            Err(()) => ERR_NOMEM,
        }
    }
}

// ---- Backpressure ----------------------------------------------------------
//
// The C ABI splits the bounded / unbounded cases into two separate
// entry points so the misuse-prone `capacity=0 with a policy` combo is
// not representable on the wire. `policy_kind` (used only by the
// bounded variant) MUST be one of the GRPCUDS_BACKPRESSURE_* constants
// in grpcuds.h. The integers are translated explicitly here so the
// Rust enum can evolve without breaking the C ABI.

/// Install a cancel-cleanup callback for an active call. The runtime fires
/// the callback at most once when the stream is closed with a non-zero
/// error code (peer RST_STREAM, session shutdown, ...). It does NOT fire
/// on graceful `grpc-status:0` close.
///
/// The `user_data` pointer must outlive either (a) the firing of the
/// callback or (b) the connection itself if the call closes gracefully.
/// The safe pattern is heap-allocation + `free` from inside the
/// callback. See the header for an example.
///
/// Returns 0 on success, `-ENOENT` if `call_id` has no active stream,
/// `-EINVAL` for a null call or null callback.
/// Remaining ms of this call's grpc-timeout budget: non-negative when the
/// client sent a deadline (0 = due), -1 when it sent none, -ENOENT for an
/// unknown call_id, -EINVAL on null. See grpcuds.h.
#[no_mangle]
pub unsafe extern "C" fn grpcuds_call_time_remaining_ms(call: *mut c_void, call_id: i32) -> i64 {
    if call.is_null() {
        return ERR_INVAL as i64;
    }
    let conn = &mut *(call as *mut Conn);
    match conn.call_time_remaining_ms(call_id) {
        Ok(Some(ms)) => ms.min(i64::MAX as u64) as i64,
        Ok(None) => -1,
        Err(_) => ERR_NOENT as i64,
    }
}

#[no_mangle]
pub unsafe extern "C" fn grpcuds_call_set_cancel_hook(
    call: *mut c_void,
    call_id: i32,
    callback: Option<unsafe extern "C" fn(*mut c_void)>,
    user_data: *mut c_void,
) -> c_int {
    if call.is_null() {
        return ERR_INVAL;
    }
    let Some(callback) = callback else {
        return ERR_INVAL;
    };
    let conn = &mut *(call as *mut Conn);
    match conn.set_cancel_hook(call_id, callback, user_data) {
        Ok(()) => 0,
        Err(e) => conn_err_to_c(e),
    }
}

/// Restore an unbounded outbound queue for an active call. Writes will
/// never be refused for backpressure reasons.
///
/// Returns 0 on success, `-ENOENT` if `call_id` has no active stream,
/// `-EINVAL` for a null call.
#[no_mangle]
pub unsafe extern "C" fn grpcuds_call_set_backpressure_unbounded(
    call: *mut c_void,
    call_id: i32,
) -> c_int {
    if call.is_null() {
        return ERR_INVAL;
    }
    let conn = &mut *(call as *mut Conn);
    match conn.set_stream_policy(call_id, Backpressure::Unbounded) {
        Ok(()) => 0,
        Err(e) => conn_err_to_c(e),
    }
}

/// Cap an active call's outbound queue at `capacity` unstarted messages
/// and pick an overflow policy:
///
/// - `policy_kind` = 0 (Reject)     — overflowing writes return `-EAGAIN`.
/// - `policy_kind` = 1 (DropOldest) — evict the oldest unstarted message.
///
/// `capacity` MUST be > 0 (use the *_unbounded variant for unbounded).
///
/// Returns 0 on success, `-ENOENT` if `call_id` has no active stream,
/// `-EINVAL` for a null call, `capacity == 0`, or an unknown `policy_kind`.
#[no_mangle]
pub unsafe extern "C" fn grpcuds_call_set_backpressure_bounded(
    call: *mut c_void,
    call_id: i32,
    capacity: usize,
    policy_kind: c_int,
) -> c_int {
    if call.is_null() {
        return ERR_INVAL;
    }
    let Some(capacity) = NonZeroUsize::new(capacity) else {
        return ERR_INVAL;
    };
    let policy = match policy_kind {
        0 => OverflowPolicy::Reject,
        1 => OverflowPolicy::DropOldest,
        _ => return ERR_INVAL,
    };
    let conn = &mut *(call as *mut Conn);
    match conn.set_stream_policy(call_id, Backpressure::Bounded { capacity, policy }) {
        Ok(()) => 0,
        Err(e) => conn_err_to_c(e),
    }
}

// ---- Outbound mailbox (thread-safe off-I/O-thread writes) ------------------
//
// grpcuds_call_write / _finish[_msg] above are always thread-safe: off the
// registered I/O thread they enqueue into the process-global mailbox instead of
// touching the core. These three wire that mailbox into the caller's poll loop.
// See docs/THREADING.md.

/// Wakeup eventfd (created on first use). Add it to your poll set; when
/// readable, call `grpcuds_mailbox_drain` on the I/O thread.
#[no_mangle]
pub extern "C" fn grpcuds_mailbox_wakeup_fd() -> c_int {
    MAILBOX.wakeup_fd()
}

/// Mark the calling thread as the I/O thread (runs the poll loop / drains the
/// mailbox). Until called, every thread is treated as the I/O thread, so a
/// single-threaded server needs no setup.
#[no_mangle]
pub extern "C" fn grpcuds_mailbox_register_io_thread() {
    MAILBOX.register_io_thread();
}

/// Drain queued off-thread writes into the core. Call on the I/O thread when
/// the wakeup fd is readable (or once per poll iteration).
#[no_mangle]
pub extern "C" fn grpcuds_mailbox_drain() {
    MAILBOX.drain();
}

// ---- Tests --------------------------------------------------------------------
//
// Unit tests can't live here because `crate-type = ["staticlib", "cdylib",
// "rlib"]` combined with `panic = "abort"` makes cargo test's panic-mode
// requirements unsatisfiable. End-to-end coverage lives in tests/echo.rs
// (integration test — links the rlib only).

#[cfg(test)]
mod tests {
    use super::*;
    use grpcuds_core::{ConnError, SessionError};

    /// The ERR_* constants ARE the grpcuds.h contract ("negated POSIX
    /// errno") — pin them to the platform's real errno values.
    #[test]
    fn error_constants_match_negated_errno() {
        assert_eq!(ERR_NOENT, -libc::ENOENT);
        assert_eq!(ERR_AGAIN, -libc::EAGAIN);
        assert_eq!(ERR_NOMEM, -libc::ENOMEM);
        assert_eq!(ERR_INVAL, -libc::EINVAL);
        assert_eq!(ERR_PIPE, -libc::EPIPE);
    }

    #[test]
    fn io_err_to_c_negates_errnos_and_maps_variants() {
        assert_eq!(io_err_to_c(IoError::Errno(13)), -13);
        assert_eq!(
            io_err_to_c(IoError::Errno(0)),
            -1,
            "non-positive errno collapses"
        );
        assert_eq!(io_err_to_c(IoError::InvalidPath), ERR_INVAL);
        assert_eq!(
            io_err_to_c(IoError::Conn(ConnError::StreamNotFound)),
            ERR_NOENT
        );
    }

    #[test]
    fn conn_err_to_c_covers_the_header_taxonomy() {
        assert_eq!(conn_err_to_c(ConnError::OutOfMemory), ERR_NOMEM);
        assert_eq!(conn_err_to_c(ConnError::StreamNotFound), ERR_NOENT);
        assert_eq!(conn_err_to_c(ConnError::StreamFinished), ERR_PIPE);
        assert_eq!(conn_err_to_c(ConnError::QueueFull), ERR_AGAIN);
        // nghttp2 codes are already negative and pass through unchanged.
        assert_eq!(conn_err_to_c(ConnError::Nghttp2(-505)), -505);
        assert_eq!(conn_err_to_c(ConnError::Session(SessionError::Alloc)), -1);
    }

    /// status_from_c transmutes — these bounds ARE the safety argument.
    #[test]
    fn status_from_c_bounds_the_transmute() {
        assert_eq!(status_from_c(0), GrpcStatus::Ok);
        assert_eq!(status_from_c(5), GrpcStatus::NotFound);
        assert_eq!(status_from_c(16), GrpcStatus::Unauthenticated);
        assert_eq!(status_from_c(17), GrpcStatus::Unknown);
        assert_eq!(status_from_c(-1), GrpcStatus::Unknown);
        assert_eq!(status_from_c(i32::MAX), GrpcStatus::Unknown);
    }

    /// Every entry point's documented null behavior, without a socket.
    #[test]
    fn null_guards_return_the_documented_values() {
        unsafe {
            let null_srv: *mut grpcuds_server = core::ptr::null_mut();
            assert_eq!(
                grpcuds_server_bind_uds(null_srv, core::ptr::null()),
                ERR_INVAL
            );
            assert_eq!(grpcuds_server_listener_fd(core::ptr::null()), ERR_INVAL);
            assert!(grpcuds_server_accept(null_srv).is_null());

            let null_conn: *mut grpcuds_conn = core::ptr::null_mut();
            assert_eq!(grpcuds_conn_tick(null_conn), ERR_INVAL);
            assert_eq!(grpcuds_conn_tick_read(null_conn), ERR_INVAL);
            assert_eq!(grpcuds_conn_tick_write(null_conn), ERR_INVAL);

            assert_eq!(
                grpcuds_call_write(core::ptr::null_mut(), 1, core::ptr::null(), 0),
                ERR_INVAL
            );
            // data == null with len > 0 is invalid even with a non-null call
            // (the null call check fires first here; the combined guard is
            // exercised end-to-end by the echo tests).
            assert_eq!(grpcuds_call_finish(core::ptr::null_mut(), 1, 0), ERR_INVAL);

            // Frees are null-safe no-ops.
            grpcuds_server_free(core::ptr::null_mut());
            grpcuds_conn_free(core::ptr::null_mut());
        }
    }

    #[test]
    fn call_time_remaining_contract_values() {
        unsafe {
            // Null call -> -EINVAL (as i64).
            assert_eq!(
                grpcuds_call_time_remaining_ms(core::ptr::null_mut(), 1),
                ERR_INVAL as i64
            );
            // Live conn, unknown call id -> -ENOENT.
            let Ok(mut conn) = Conn::new_server() else {
                panic!("new_server failed");
            };
            let call = &mut conn as *mut Conn as *mut c_void;
            assert_eq!(grpcuds_call_time_remaining_ms(call, 42), ERR_NOENT as i64);
        }
    }

    #[test]
    fn bind_uds_rejects_an_unbindable_path_without_touching_state() {
        unsafe {
            let s = grpcuds_server_new();
            // Empty path: invalid before any syscall.
            let empty = [0u8; 1];
            assert_eq!(
                grpcuds_server_bind_uds(s, empty.as_ptr() as *const c_char),
                ERR_INVAL
            );
            assert_eq!(grpcuds_server_listener_fd(s), -1, "still unbound");
            grpcuds_server_free(s);
        }
    }
}
