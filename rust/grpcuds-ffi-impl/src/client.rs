// SPDX-License-Identifier: MIT OR Apache-2.0
//! The client C ABI (`grpcuds_client_*` / `grpcuds_response_*` /
//! `grpcuds_stream_*`). Opaque handles + accessors, matching the server side.
#![allow(clippy::missing_safety_doc)]

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::ffi::{c_char, c_int, CStr};
use core::ptr;

use grpcuds_core::client::{ClientConn, ClientError};

/// Opaque client connection.
pub struct grpcuds_client {
    conn: ClientConn,
}

/// Opaque unary response: body bytes + grpc status/message.
pub struct grpcuds_response {
    body: Vec<u8>,
    status: i32,
    message: Option<alloc::string::String>,
}

/// Opaque server-streaming reader. Holds the connection back-pointer so it
/// can pull more messages; the connection must outlive the stream.
pub struct grpcuds_stream {
    client: *mut grpcuds_client,
    // Current message buffer kept alive while the caller reads it.
    cur: Vec<u8>,
    done: bool,
    status: i32,
    message: Option<alloc::string::String>,
}

fn err_to_int(e: ClientError) -> c_int {
    match e {
        ClientError::Connect => -1,
        ClientError::Session => -2,
        ClientError::Io => -3,
        ClientError::Protocol => -4,
    }
}

/// Connect to a gRPC server on the UDS `path` (NUL-terminated). Returns NULL
/// on failure.
#[no_mangle]
pub unsafe extern "C" fn grpcuds_client_connect(path: *const c_char) -> *mut grpcuds_client {
    if path.is_null() {
        return ptr::null_mut();
    }
    let bytes = CStr::from_ptr(path).to_bytes();
    match ClientConn::connect(bytes) {
        Ok(conn) => Box::into_raw(Box::new(grpcuds_client { conn })),
        Err(_) => ptr::null_mut(),
    }
}

/// Like `grpcuds_client_connect`, but retries with exponential backoff
/// (50 ms × 1.6 to a 1 s cap, ±20% jitter; each attempt bounded at 250 ms)
/// until `timeout_ms` elapses — covers the daemon-startup race (socket
/// file absent, or present but pre-listen). `timeout_ms == 0` makes
/// exactly one attempt. NULL on failure.
#[no_mangle]
pub unsafe extern "C" fn grpcuds_client_connect_wait(
    path: *const c_char,
    timeout_ms: u32,
) -> *mut grpcuds_client {
    if path.is_null() {
        return ptr::null_mut();
    }
    let bytes = CStr::from_ptr(path).to_bytes();
    match ClientConn::connect_wait(bytes, timeout_ms) {
        Ok(conn) => Box::into_raw(Box::new(grpcuds_client { conn })),
        Err(_) => ptr::null_mut(),
    }
}

/// Per-call timeout in ms (0 clears). See grpcuds.h for the contract.
#[no_mangle]
pub unsafe extern "C" fn grpcuds_client_set_timeout_ms(
    client: *mut grpcuds_client,
    timeout_ms: u32,
) -> c_int {
    if client.is_null() {
        return -22; // -EINVAL, matching the server-side ERR_INVAL contract
    }
    (*client).conn.set_timeout_ms(timeout_ms);
    0
}

/// Free a client connection.
#[no_mangle]
pub unsafe extern "C" fn grpcuds_client_free(client: *mut grpcuds_client) {
    if !client.is_null() {
        drop(Box::from_raw(client));
    }
}

/// Perform a unary call. `path` is NUL-terminated (`"/pkg.Svc/Method"`).
/// On transport success returns a `grpcuds_response*` (inspect its status and
/// body); on transport failure returns NULL.
#[no_mangle]
pub unsafe extern "C" fn grpcuds_client_unary(
    client: *mut grpcuds_client,
    path: *const c_char,
    req: *const u8,
    req_len: usize,
) -> *mut grpcuds_response {
    if client.is_null() || path.is_null() {
        return ptr::null_mut();
    }
    let c = &mut *client;
    let path_b = CStr::from_ptr(path).to_bytes();
    let req_b = if req_len == 0 {
        &[][..]
    } else {
        core::slice::from_raw_parts(req, req_len)
    };
    let mut call = match c.conn.unary(path_b, req_b) {
        Ok(call) => call,
        Err(_) => return ptr::null_mut(),
    };
    let status = call.status_code().unwrap_or(2);
    let message = call.message().map(alloc::string::String::from);
    let body = match call.recv() {
        Ok(Some(m)) => m,
        _ => Vec::new(),
    };
    Box::into_raw(Box::new(grpcuds_response {
        body,
        status,
        message,
    }))
}

/// The numeric grpc-status of a response (0 = OK).
#[no_mangle]
pub unsafe extern "C" fn grpcuds_response_status(resp: *const grpcuds_response) -> c_int {
    if resp.is_null() {
        return 2; // Unknown
    }
    (*resp).status as c_int
}

/// The `grpc-message` bytes of a response (not NUL-terminated). Writes the
/// length through `len` and returns the pointer (NULL if no message).
#[no_mangle]
pub unsafe extern "C" fn grpcuds_response_message_bytes(
    resp: *const grpcuds_response,
    len: *mut usize,
) -> *const u8 {
    if resp.is_null() {
        return ptr::null();
    }
    match &(*resp).message {
        Some(m) => {
            if !len.is_null() {
                *len = m.len();
            }
            m.as_ptr()
        }
        None => {
            if !len.is_null() {
                *len = 0;
            }
            ptr::null()
        }
    }
}

/// The response body bytes. Writes the length through `len`; valid until the
/// response is freed.
#[no_mangle]
pub unsafe extern "C" fn grpcuds_response_body(
    resp: *const grpcuds_response,
    len: *mut usize,
) -> *const u8 {
    if resp.is_null() {
        if !len.is_null() {
            *len = 0;
        }
        return ptr::null();
    }
    if !len.is_null() {
        *len = (*resp).body.len();
    }
    (*resp).body.as_ptr()
}

/// Free a unary response.
#[no_mangle]
pub unsafe extern "C" fn grpcuds_response_free(resp: *mut grpcuds_response) {
    if !resp.is_null() {
        drop(Box::from_raw(resp));
    }
}

/// Start a server-streaming call. Returns a `grpcuds_stream*` (read it with
/// `grpcuds_stream_next`) on transport success, NULL on failure. The
/// `client` must outlive the returned stream.
#[no_mangle]
pub unsafe extern "C" fn grpcuds_client_server_streaming(
    client: *mut grpcuds_client,
    path: *const c_char,
    req: *const u8,
    req_len: usize,
) -> *mut grpcuds_stream {
    if client.is_null() || path.is_null() {
        return ptr::null_mut();
    }
    let c = &mut *client;
    let path_b = CStr::from_ptr(path).to_bytes();
    let req_b = if req_len == 0 {
        &[][..]
    } else {
        core::slice::from_raw_parts(req, req_len)
    };
    // The call borrows the connection; we cannot store the borrow across the
    // C ABI, so we re-issue per-`next` via the connection back-pointer. To do
    // that, submit here and drive through the connection directly.
    match c.conn.server_streaming(path_b, req_b) {
        Ok(_call) => {
            // `_call` borrows `c.conn` and is dropped here; the stream's state
            // lives on the connection, so subsequent `grpcuds_stream_next`
            // drives it via a fresh borrow.
            Box::into_raw(Box::new(grpcuds_stream {
                client,
                cur: Vec::new(),
                done: false,
                status: 0,
                message: None,
            }))
        }
        Err(_) => ptr::null_mut(),
    }
}

/// Read the next message of a stream. Writes its length through `len` and
/// returns the bytes (valid until the next `_next` or `_free`); returns NULL
/// at end of stream (then check `grpcuds_stream_status`).
#[no_mangle]
pub unsafe extern "C" fn grpcuds_stream_next(
    stream: *mut grpcuds_stream,
    len: *mut usize,
) -> *const u8 {
    if stream.is_null() {
        return ptr::null();
    }
    let s = &mut *stream;
    if s.done {
        if !len.is_null() {
            *len = 0;
        }
        return ptr::null();
    }
    let c = &mut *s.client;
    // Re-borrow the in-flight call from the connection and pull one message.
    match c.conn.recv_current() {
        Ok(Some(msg)) => {
            s.cur = msg;
            if !len.is_null() {
                *len = s.cur.len();
            }
            s.cur.as_ptr()
        }
        Ok(None) => {
            s.done = true;
            s.status = c.conn.last_status().unwrap_or(0);
            s.message = c.conn.last_message();
            if !len.is_null() {
                *len = 0;
            }
            ptr::null()
        }
        Err(_) => {
            s.done = true;
            s.status = 14; // Unavailable
            if !len.is_null() {
                *len = 0;
            }
            ptr::null()
        }
    }
}

/// The final grpc-status of a finished stream (0 = OK).
#[no_mangle]
pub unsafe extern "C" fn grpcuds_stream_status(stream: *const grpcuds_stream) -> c_int {
    if stream.is_null() {
        return 2;
    }
    (*stream).status as c_int
}

/// Free a stream reader.
#[no_mangle]
pub unsafe extern "C" fn grpcuds_stream_free(stream: *mut grpcuds_stream) {
    if !stream.is_null() {
        drop(Box::from_raw(stream));
    }
}

// Keep the error mapper referenced (used by future error-returning entry
// points; today the unary/streaming paths fold transport errors into the
// NULL return + status).
const _: fn(ClientError) -> c_int = err_to_int;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn err_to_int_is_the_documented_table() {
        assert_eq!(err_to_int(ClientError::Connect), -1);
        assert_eq!(err_to_int(ClientError::Session), -2);
        assert_eq!(err_to_int(ClientError::Io), -3);
        assert_eq!(err_to_int(ClientError::Protocol), -4);
    }

    /// Null in, documented value out — no socket needed.
    #[test]
    fn null_guards_return_the_documented_values() {
        unsafe {
            assert!(grpcuds_client_connect(core::ptr::null()).is_null());
            assert!(grpcuds_client_connect_wait(core::ptr::null(), 100).is_null());
            assert_eq!(
                grpcuds_client_set_timeout_ms(core::ptr::null_mut(), 100),
                -22
            );

            // 2 == GRPCUDS_UNKNOWN, the header's "no status available" value.
            assert_eq!(grpcuds_response_status(core::ptr::null()), 2);
            assert_eq!(grpcuds_stream_status(core::ptr::null()), 2);

            let mut len: usize = 7;
            assert!(grpcuds_response_body(core::ptr::null(), &mut len).is_null());
            assert_eq!(len, 0, "len is zeroed on the null path");
            assert!(grpcuds_response_message_bytes(core::ptr::null(), &mut len).is_null());
            assert!(grpcuds_stream_next(core::ptr::null_mut(), &mut len).is_null());

            // Frees are null-safe no-ops.
            grpcuds_client_free(core::ptr::null_mut());
            grpcuds_response_free(core::ptr::null_mut());
            grpcuds_stream_free(core::ptr::null_mut());
        }
    }
}
