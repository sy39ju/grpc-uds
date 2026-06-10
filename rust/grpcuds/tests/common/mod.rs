// SPDX-License-Identifier: MIT OR Apache-2.0
//! Shared test helpers: a hand-rolled nghttp2 + gRPC client so the wire-level
//! tests depend on nothing but the transport. Used by both `echo.rs` (server on
//! a `std::thread`) and `tokio.rs` (server on a tokio blocking thread).
//!
//! Each integration-test binary compiles this module and uses a different
//! subset of it, so suppress dead-code warnings for the unused remainder.
#![allow(dead_code)]

use std::ffi::c_void;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use grpcuds_core::{encode_header, FRAME_HEADER_LEN};

use grpcuds_sys::{
    nghttp2_data_flag_NGHTTP2_DATA_FLAG_EOF as DATA_EOF, nghttp2_data_provider,
    nghttp2_data_source, nghttp2_frame, nghttp2_nv, nghttp2_session, nghttp2_session_callbacks,
    nghttp2_session_callbacks_del, nghttp2_session_callbacks_new,
    nghttp2_session_callbacks_set_on_data_chunk_recv_callback as set_on_data,
    nghttp2_session_callbacks_set_on_frame_recv_callback as set_on_frame,
    nghttp2_session_callbacks_set_on_header_callback as set_on_header, nghttp2_session_client_new,
    nghttp2_session_del, nghttp2_session_mem_recv, nghttp2_session_mem_send,
    nghttp2_submit_request, nghttp2_submit_settings,
};

static COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn unique_path() -> String {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    format!(
        "/tmp/grpcuds-wrapper-test-{}-{}.sock",
        std::process::id(),
        n
    )
}

// ---- Minimal nghttp2 client ------------------------------------------------

pub struct ClientState {
    pub status: Option<Vec<u8>>,
    pub content_type: Option<Vec<u8>>,
    pub grpc_status: Option<Vec<u8>>,
    pub grpc_message: Option<Vec<u8>>,
    pub data: Vec<u8>,
    pub end_stream_seen: bool,
}

unsafe extern "C" fn cli_on_header(
    _s: *mut nghttp2_session,
    _f: *const nghttp2_frame,
    name: *const u8,
    namelen: usize,
    value: *const u8,
    valuelen: usize,
    _flags: u8,
    ud: *mut c_void,
) -> i32 {
    let st = &mut *(ud as *mut ClientState);
    let n = std::slice::from_raw_parts(name, namelen);
    let v = std::slice::from_raw_parts(value, valuelen).to_vec();
    match n {
        b":status" => st.status = Some(v),
        b"content-type" => st.content_type = Some(v),
        b"grpc-status" => st.grpc_status = Some(v),
        b"grpc-message" => st.grpc_message = Some(v),
        _ => {}
    }
    0
}

unsafe extern "C" fn cli_on_data(
    _s: *mut nghttp2_session,
    _flags: u8,
    _sid: i32,
    data: *const u8,
    len: usize,
    ud: *mut c_void,
) -> i32 {
    let st = &mut *(ud as *mut ClientState);
    st.data
        .extend_from_slice(std::slice::from_raw_parts(data, len));
    0
}

unsafe extern "C" fn cli_on_frame(
    _s: *mut nghttp2_session,
    f: *const nghttp2_frame,
    ud: *mut c_void,
) -> i32 {
    let st = &mut *(ud as *mut ClientState);
    // END_STREAM (0x1) is only meaningful on DATA (type 0) and HEADERS
    // (type 1) frames; on a SETTINGS frame (type 4) bit 0x1 is the ACK flag,
    // so checking it blindly would falsely report stream end.
    let ty = (*f).hd.type_;
    if (ty == 0 || ty == 1) && (*f).hd.flags & 0x1 != 0 {
        st.end_stream_seen = true;
    }
    0
}

struct ClientReq {
    bytes: Vec<u8>,
    offset: usize,
}

unsafe extern "C" fn cli_data_read(
    _s: *mut nghttp2_session,
    _sid: i32,
    buf: *mut u8,
    length: usize,
    data_flags: *mut u32,
    source: *mut nghttp2_data_source,
    _ud: *mut c_void,
) -> isize {
    let src = &mut *((*source).ptr as *mut ClientReq);
    let remaining = src.bytes.len() - src.offset;
    let n = remaining.min(length);
    if n > 0 {
        ptr::copy_nonoverlapping(src.bytes.as_ptr().add(src.offset), buf, n);
        src.offset += n;
    }
    if src.offset == src.bytes.len() {
        *data_flags |= DATA_EOF;
    }
    n as isize
}

fn nv(name: &'static [u8], value: &'static [u8]) -> nghttp2_nv {
    nghttp2_nv {
        name: name.as_ptr() as *mut u8,
        value: value.as_ptr() as *mut u8,
        namelen: name.len(),
        valuelen: value.len(),
        flags: 0,
    }
}

fn frame(msg: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    let mut hdr = [0u8; FRAME_HEADER_LEN];
    encode_header(false, msg.len() as u32, &mut hdr);
    body.extend_from_slice(&hdr);
    body.extend_from_slice(msg);
    body
}

/// Start a gRPC call to `path`, pump until the first response DATA bytes
/// arrive, then drop the connection on the floor (no GOAWAY, no stream close)
/// — i.e. behave like a client that died mid-stream. Returns the bytes seen.
pub fn call_then_vanish(sock: &str, path: &'static [u8], req_msg: &[u8]) -> Vec<u8> {
    let st = drive_call(sock, path, req_msg, true);
    st.data
}

/// Make one gRPC call to `path` carrying `req_msg`, return the client state.
pub fn call(sock: &str, path: &'static [u8], req_msg: &[u8]) -> ClientState {
    drive_call(sock, path, req_msg, false)
}

fn drive_call(
    sock: &str,
    path: &'static [u8],
    req_msg: &[u8],
    vanish_on_first_data: bool,
) -> ClientState {
    let mut stream = UnixStream::connect(sock).expect("connect");
    stream.set_nonblocking(true).unwrap();

    let mut state = Box::new(ClientState {
        status: None,
        content_type: None,
        grpc_status: None,
        grpc_message: None,
        data: Vec::new(),
        end_stream_seen: false,
    });
    let mut req = Box::new(ClientReq {
        bytes: frame(req_msg),
        offset: 0,
    });

    unsafe {
        let mut cbs: *mut nghttp2_session_callbacks = ptr::null_mut();
        assert_eq!(nghttp2_session_callbacks_new(&mut cbs), 0);
        set_on_header(cbs, Some(cli_on_header));
        set_on_data(cbs, Some(cli_on_data));
        set_on_frame(cbs, Some(cli_on_frame));
        let mut client: *mut nghttp2_session = ptr::null_mut();
        assert_eq!(
            nghttp2_session_client_new(&mut client, cbs, state.as_mut() as *mut _ as *mut c_void),
            0
        );
        nghttp2_session_callbacks_del(cbs);
        assert_eq!(nghttp2_submit_settings(client, 0, ptr::null(), 0), 0);

        let path_nv = nghttp2_nv {
            name: b":path".as_ptr() as *mut u8,
            value: path.as_ptr() as *mut u8,
            namelen: 5,
            valuelen: path.len(),
            flags: 0,
        };
        let nva = [
            nv(b":method", b"POST"),
            nv(b":scheme", b"http"),
            path_nv,
            nv(b":authority", b"localhost"),
            nv(b"te", b"trailers"),
            nv(b"content-type", b"application/grpc"),
        ];
        let provider = nghttp2_data_provider {
            source: nghttp2_data_source {
                ptr: req.as_mut() as *mut _ as *mut c_void,
            },
            read_callback: Some(cli_data_read),
        };
        let sid = nghttp2_submit_request(
            client,
            ptr::null(),
            nva.as_ptr(),
            nva.len(),
            &provider,
            ptr::null_mut(),
        );
        assert!(sid > 0);

        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            // Client → socket
            loop {
                let mut p: *const u8 = ptr::null();
                let n = nghttp2_session_mem_send(client, &mut p);
                assert!(n >= 0);
                if n == 0 || p.is_null() {
                    break;
                }
                stream
                    .write_all(std::slice::from_raw_parts(p, n as usize))
                    .unwrap();
            }
            // Socket → client
            let mut buf = [0u8; 4096];
            loop {
                match stream.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let rc = nghttp2_session_mem_recv(client, buf.as_ptr(), n);
                        assert!(rc >= 0, "client mem_recv: {rc}");
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(e) => panic!("read err: {e}"),
                }
            }
            if state.end_stream_seen {
                break;
            }
            if vanish_on_first_data && !state.data.is_empty() {
                break; // simulate a client death: just stop talking
            }
            thread::sleep(Duration::from_millis(2));
        }
        nghttp2_session_del(client);
    }
    // `stream` drops here — for the vanish path that closes the socket with
    // the stream still open, which the server must treat as a cancellation.
    *state
}
