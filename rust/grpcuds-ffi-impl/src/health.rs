// SPDX-License-Identifier: MIT OR Apache-2.0
//! The standard gRPC health checking service (`grpc.health.v1.Health`) for the
//! C ABI. Part of the server surface, but a server that never calls
//! `grpcuds_health_register` pays nothing — the linker's `--gc-sections` drops
//! the unreferenced code.
//!
//! Lets stock tooling (`grpc_health_probe`, `grpcurl`, tonic-health) ask "is
//! this daemon serving?" over the normal socket, from plain C:
//!
//! ```c
//! grpcuds_health_register(server);                 // "" starts SERVING
//! grpcuds_health_set_status("ble.BleScanner", GRPCUDS_HEALTH_SERVING);
//! // ... later, e.g. when the radio dies:
//! grpcuds_health_set_status("ble.BleScanner", GRPCUDS_HEALTH_NOT_SERVING);
//! ```
//!
//! `Check` is unary (unknown services fail `NOT_FOUND`, per the protocol);
//! `Watch` is server-streaming (immediate status — `SERVICE_UNKNOWN` for
//! unregistered names — then every change). `set_status` is thread-safe; the
//! watcher writes ride the C ABI's outbound mailbox.
//!
//! The two messages are one string field and one varint enum, so they are
//! encoded/decoded here — no nanopb, no generated code. The bytes match the
//! C++ twin (`grpcudspp/health.h`) and the prost twin (`grpcuds::health`),
//! pinned in the unit tests below; `example/c`'s client probes Check + Watch
//! end-to-end.
//!
//! Process-global registry (one Server per process — the reference topology),
//! mirroring the outbound mailbox.

use core::cell::UnsafeCell;
use core::ffi::{c_char, c_int, c_void, CStr};

use alloc::boxed::Box;
use alloc::vec::Vec;

use grpcuds_core::{Conn, GrpcStatus};

// `grpc.health.v1.HealthCheckResponse.ServingStatus` wire values.
const UNKNOWN: i32 = 0;
const SERVING: i32 = 1;
const SERVICE_UNKNOWN: i32 = 3;

// ---- wire helpers (proto3, field 1 only) ------------------------------------
// HealthCheckRequest  = { string service = 1; }       -> 0x0A <varint len> bytes
// HealthCheckResponse = { ServingStatus status = 1; }  -> 0x08 <varint>
// proto3 default (empty string / UNKNOWN) encodes as zero bytes.

/// Encode a `HealthCheckResponse{status}`. `UNKNOWN` (0) is the proto3 default
/// and encodes empty. Status values are 0..=3, so the varint is one byte.
/// Returns empty on allocation failure (best effort; OOM is already fatal).
fn encode_response(status: i32) -> Vec<u8> {
    let mut out = Vec::new();
    if status == UNKNOWN {
        return out;
    }
    if out.try_reserve(2).is_err() {
        return Vec::new();
    }
    out.push(0x08);
    out.push(status as u8);
    out
}

/// Decode a `HealthCheckRequest`, returning the `service` field bytes (empty
/// when absent — the overall server). `None` on a malformed message.
fn decode_check_request(d: &[u8]) -> Option<&[u8]> {
    let mut i = 0usize;
    let mut service: &[u8] = &[];
    while i < d.len() {
        if d[i] != 0x0A {
            return None; // only field 1 (the service string) exists
        }
        i += 1;
        // varint length
        let mut len: u64 = 0;
        let mut shift = 0u32;
        loop {
            if i >= d.len() || shift >= 64 {
                return None;
            }
            let b = d[i];
            i += 1;
            len |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                break;
            }
            shift += 7;
        }
        // Bound-check as u64 *before* narrowing: on a 32-bit target (armv7) a
        // length above usize::MAX must be rejected, not truncated.
        if len > (d.len() - i) as u64 {
            return None;
        }
        let len = len as usize;
        service = &d[i..i + len];
        i += len;
    }
    Some(service)
}

// ---- the process-global registry --------------------------------------------

/// One `Watch` subscription. `live` flips off (under the lock) when the client
/// cancels; the box stays owned by the registry — a stable address for the
/// cancel hook — and is reaped lazily on the next `Watch`.
struct Watcher {
    call: *mut c_void,
    call_id: i32,
    service: Vec<u8>,
    live: bool,
}

struct HealthState {
    lock: UnsafeCell<libc::pthread_mutex_t>,
    statuses: UnsafeCell<Vec<(Vec<u8>, i32)>>,
    // Boxed on purpose (not `Vec<Watcher>`): each watcher's address is handed to
    // the cancel hook as user_data and must stay stable across Vec growth.
    #[allow(clippy::vec_box)]
    watchers: UnsafeCell<Vec<Box<Watcher>>>,
    seeded: UnsafeCell<bool>, // "" -> SERVING installed on first register
}

// SAFETY: every access to the interior is serialized by the pthread_mutex.
unsafe impl Sync for HealthState {}

static HEALTH: HealthState = HealthState {
    lock: UnsafeCell::new(libc::PTHREAD_MUTEX_INITIALIZER),
    statuses: UnsafeCell::new(Vec::new()),
    watchers: UnsafeCell::new(Vec::new()),
    seeded: UnsafeCell::new(false),
};

struct Guard<'a>(&'a HealthState);
impl Drop for Guard<'_> {
    fn drop(&mut self) {
        unsafe { libc::pthread_mutex_unlock(self.0.lock.get()) };
    }
}

impl HealthState {
    fn lock(&self) -> Guard<'_> {
        unsafe { libc::pthread_mutex_lock(self.lock.get()) };
        Guard(self)
    }

    // Caller holds the lock. Returns the current status, or None if unregistered.
    unsafe fn find(&self, service: &[u8]) -> Option<i32> {
        (*self.statuses.get())
            .iter()
            .find(|(name, _)| name.as_slice() == service)
            .map(|(_, s)| *s)
    }

    // Caller holds the lock. Insert or update `service`'s status.
    unsafe fn set(&self, service: &[u8], status: i32) {
        let v = &mut *self.statuses.get();
        if let Some(e) = v.iter_mut().find(|(name, _)| name.as_slice() == service) {
            e.1 = status;
            return;
        }
        let mut name = Vec::new();
        if name.try_reserve(service.len()).is_err() || v.try_reserve(1).is_err() {
            return;
        }
        name.extend_from_slice(service);
        v.push((name, status));
    }
}

// ---- handler trampolines (run on the I/O thread) ----------------------------

unsafe extern "C" fn check_tr(
    call: *mut c_void,
    call_id: i32,
    req: *const u8,
    req_len: usize,
    _ud: *mut c_void,
) -> c_int {
    let bytes = if req.is_null() || req_len == 0 {
        &[][..]
    } else {
        core::slice::from_raw_parts(req, req_len)
    };
    let Some(service) = decode_check_request(bytes) else {
        finish(
            call,
            call_id,
            GrpcStatus::InvalidArgument,
            b"malformed HealthCheckRequest",
        );
        return 0;
    };
    let status = {
        let _g = HEALTH.lock();
        HEALTH.find(service)
    };
    match status {
        // The protocol: an unknown service name fails with NOT_FOUND.
        None => finish(call, call_id, GrpcStatus::NotFound, b"unknown service"),
        Some(s) => {
            let msg = encode_response(s);
            write(call, call_id, &msg);
            finish(call, call_id, GrpcStatus::Ok, b"");
        }
    }
    0
}

unsafe extern "C" fn watch_tr(
    call: *mut c_void,
    call_id: i32,
    req: *const u8,
    req_len: usize,
    _ud: *mut c_void,
) -> c_int {
    let bytes = if req.is_null() || req_len == 0 {
        &[][..]
    } else {
        core::slice::from_raw_parts(req, req_len)
    };
    let Some(service) = decode_check_request(bytes) else {
        finish(
            call,
            call_id,
            GrpcStatus::InvalidArgument,
            b"malformed HealthCheckRequest",
        );
        return 0;
    };

    let mut name = Vec::new();
    if name.try_reserve(service.len()).is_err() {
        finish(call, call_id, GrpcStatus::Internal, b"");
        return 0;
    }
    name.extend_from_slice(service);

    let (current, watcher_ptr) = {
        let _g = HEALTH.lock();
        let current = HEALTH.find(service).unwrap_or(SERVICE_UNKNOWN);
        // Reap watchers cancelled since the last subscription (their cancel
        // hook already fired and won't fire again).
        let ws = &mut *HEALTH.watchers.get();
        ws.retain(|w| w.live);
        if ws.try_reserve(1).is_err() {
            // Can't track this watcher; still answer with the current status.
            (current, core::ptr::null_mut::<Watcher>())
        } else {
            let mut w = Box::new(Watcher {
                call,
                call_id,
                service: name,
                live: true,
            });
            let ptr = w.as_mut() as *mut Watcher;
            ws.push(w);
            (current, ptr)
        }
    };

    if !watcher_ptr.is_null() {
        // Cancel hook fires once when the stream is reset; it marks the watcher
        // dead so set_status stops writing to it (reaped on the next Watch).
        super::grpcuds_call_set_cancel_hook(call, call_id, Some(on_cancel), watcher_ptr.cast());
    }
    // Initial status goes out AFTER releasing the lock: a write can re-enter the
    // core, which may fire on_cancel for another watcher (and on_cancel takes
    // the lock — non-recursive).
    let msg = encode_response(current);
    write(call, call_id, &msg);
    0 // stream stays open; set_status feeds it
}

unsafe extern "C" fn on_cancel(user_data: *mut c_void) {
    if user_data.is_null() {
        return;
    }
    let _g = HEALTH.lock();
    let w = &mut *(user_data as *mut Watcher);
    w.live = false;
}

// ---- direct core calls (handlers run on the I/O thread) ---------------------

unsafe fn write(call: *mut c_void, call_id: i32, msg: &[u8]) {
    let conn = &mut *(call as *mut Conn);
    let _ = conn.write_call(call_id, msg);
}
unsafe fn finish(call: *mut c_void, call_id: i32, status: GrpcStatus, msg: &[u8]) {
    let conn = &mut *(call as *mut Conn);
    let _ = conn.finish_call_msg(call_id, status, msg);
}

// ---- C ABI ------------------------------------------------------------------

/// Register `grpc.health.v1.Health/{Check,Watch}` on `server`. The overall
/// service (`""`) starts as SERVING. Returns 0, or a negative errno on a null
/// server / registration failure.
#[no_mangle]
pub unsafe extern "C" fn grpcuds_health_register(server: *mut super::grpcuds_server) -> c_int {
    if server.is_null() {
        return -22; // -EINVAL
    }
    {
        let _g = HEALTH.lock();
        if !*HEALTH.seeded.get() {
            HEALTH.set(b"", SERVING);
            *HEALTH.seeded.get() = true;
        }
    }
    let rc = super::grpcuds_server_register_method(
        server,
        c"/grpc.health.v1.Health/Check".as_ptr(),
        Some(check_tr),
        core::ptr::null_mut(),
    );
    if rc != 0 {
        return rc;
    }
    super::grpcuds_server_register_method(
        server,
        c"/grpc.health.v1.Health/Watch".as_ptr(),
        Some(watch_tr),
        core::ptr::null_mut(),
    )
}

/// Set (or register) `service`'s serving status and notify its `Watch`ers.
/// `service` is a NUL-terminated UTF-8 name; `""` is the server overall.
/// `status` is a `GRPCUDS_HEALTH_*` value. Thread-safe — call it from any
/// thread (off-I/O-thread watcher writes ride the outbound mailbox).
#[no_mangle]
pub unsafe extern "C" fn grpcuds_health_set_status(service: *const c_char, status: c_int) {
    let name: &[u8] = if service.is_null() {
        &[]
    } else {
        CStr::from_ptr(service).to_bytes()
    };
    // Snapshot the watcher targets under the lock; write OUTSIDE it (a write may
    // re-enter the core -> on_cancel -> takes the lock, non-recursive).
    let mut targets: Vec<(*mut c_void, i32)> = Vec::new();
    {
        let _g = HEALTH.lock();
        HEALTH.set(name, status);
        let ws = &*HEALTH.watchers.get();
        for w in ws.iter() {
            if w.live && w.service.as_slice() == name {
                if targets.try_reserve(1).is_err() {
                    break;
                }
                targets.push((w.call, w.call_id));
            }
        }
    }
    let msg = encode_response(status);
    for (call, call_id) in targets {
        // Thread-safe: on the I/O thread direct, else via the mailbox.
        super::grpcuds_call_write(call, call_id, msg.as_ptr(), msg.len());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_encoding_matches_the_protocol() {
        // tag 1 (varint) = 0x08, value = status; UNKNOWN(0) is empty (proto3).
        assert_eq!(encode_response(0), b"");
        assert_eq!(encode_response(1), b"\x08\x01"); // SERVING
        assert_eq!(encode_response(2), b"\x08\x02"); // NOT_SERVING
        assert_eq!(encode_response(3), b"\x08\x03"); // SERVICE_UNKNOWN
    }

    #[test]
    fn request_decoding_reads_field_one() {
        // tag 1 (len-delimited) = 0x0A, len 3, "svc".
        assert_eq!(decode_check_request(b"\x0a\x03svc"), Some(&b"svc"[..]));
        // empty body => overall service "".
        assert_eq!(decode_check_request(b""), Some(&b""[..]));
        // wrong field tag / truncated length => malformed.
        assert_eq!(decode_check_request(b"\x10\x01"), None);
        assert_eq!(decode_check_request(b"\x0a\x05ab"), None);
    }

    #[test]
    fn registry_set_and_find() {
        let st = HealthState {
            lock: UnsafeCell::new(libc::PTHREAD_MUTEX_INITIALIZER),
            statuses: UnsafeCell::new(Vec::new()),
            watchers: UnsafeCell::new(Vec::new()),
            seeded: UnsafeCell::new(false),
        };
        unsafe {
            let _g = st.lock();
            assert_eq!(st.find(b"ghost"), None);
            st.set(b"", SERVING);
            assert_eq!(st.find(b""), Some(SERVING));
            st.set(b"svc", SERVING);
            st.set(b"svc", 2);
            assert_eq!(st.find(b"svc"), Some(2));
        }
    }
}
