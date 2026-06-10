// SPDX-License-Identifier: MIT OR Apache-2.0
//! End-to-end exercise of the C ABI from Rust. We call our own
//! `extern "C"` symbols just like a C consumer would, then drive a real
//! nghttp2 client over a real UDS socket.
//!
//! This is an *integration* test (lives under `tests/`) so it links the
//! crate as an rlib only — bypasses the panic-mode conflict that
//! `crate-type = ["staticlib", "cdylib", "rlib"]` produces during unit
//! test builds.

use std::ffi::CString;
use std::io::{ErrorKind, Read, Write};
use std::os::raw::{c_int, c_void};
use std::os::unix::net::UnixStream;
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use grpcuds_core::framing::{
    decode_header, encode_header, DEFAULT_MAX_MESSAGE_LEN, FRAME_HEADER_LEN,
};
use grpcuds_ffi_impl::{
    grpcuds_call_finish, grpcuds_call_write, grpcuds_conn_fd, grpcuds_conn_free, grpcuds_conn_tick,
    grpcuds_handler_fn, grpcuds_server_accept, grpcuds_server_bind_uds, grpcuds_server_free,
    grpcuds_server_listener_fd, grpcuds_server_new, grpcuds_server_register_method,
};
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

fn unique_path() -> CString {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    CString::new(format!("/tmp/grpcuds-ffi-test-{pid}-{n}.sock")).unwrap()
}

// --- Client-side nghttp2 callbacks -----------------------------------------

struct ClientState {
    status: Option<Vec<u8>>,
    content_type: Option<Vec<u8>>,
    grpc_status: Option<Vec<u8>>,
    data: Vec<u8>,
    end_stream_seen: bool,
}
impl ClientState {
    fn new() -> Self {
        Self {
            status: None,
            content_type: None,
            grpc_status: None,
            data: Vec::new(),
            end_stream_seen: false,
        }
    }
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
    if n == b":status" {
        st.status = Some(v);
    } else if n == b"content-type" {
        st.content_type = Some(v);
    } else if n == b"grpc-status" {
        st.grpc_status = Some(v);
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
    if (*f).hd.flags & 0x1 != 0 {
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

// --- User C handler --------------------------------------------------------

unsafe extern "C" fn echo_handler_c(
    call: *mut c_void,
    call_id: i32,
    req: *const u8,
    req_len: usize,
    _ud: *mut c_void,
) -> c_int {
    let _w = grpcuds_call_write(call, call_id, req, req_len);
    let _f = grpcuds_call_finish(call, call_id, 0);
    0
}

#[test]
fn lifecycle_smoke() {
    unsafe {
        let s = grpcuds_server_new();
        assert!(!s.is_null());
        // Without bind, listener_fd is -1 and accept returns NULL.
        assert_eq!(grpcuds_server_listener_fd(s), -1);
        assert!(grpcuds_server_accept(s).is_null());
        grpcuds_server_free(s);
    }
}

#[test]
fn echo_round_trip_through_c_abi() {
    let path = unique_path();

    unsafe {
        // 1. Build the server via the C ABI.
        let server = grpcuds_server_new();
        assert!(!server.is_null());
        assert_eq!(grpcuds_server_bind_uds(server, path.as_ptr()), 0);

        let echo: grpcuds_handler_fn = echo_handler_c;
        let svc_path = CString::new("/svc/Echo").unwrap();
        assert_eq!(
            grpcuds_server_register_method(server, svc_path.as_ptr(), Some(echo), ptr::null_mut(),),
            0
        );

        let listener_fd = grpcuds_server_listener_fd(server);
        assert!(listener_fd > 0);

        // 2. Connect a client via std UnixStream + an nghttp2 client session.
        let mut stream =
            UnixStream::connect(std::str::from_utf8(path.as_bytes()).unwrap()).unwrap();
        stream.set_nonblocking(true).unwrap();

        // 3. Accept on the server side.
        let conn = {
            let mut tries = 0;
            loop {
                let c = grpcuds_server_accept(server);
                if !c.is_null() {
                    break c;
                }
                tries += 1;
                assert!(tries < 100, "accept never returned a connection");
                thread::sleep(Duration::from_millis(1));
            }
        };
        assert!(grpcuds_conn_fd(conn) > 0);

        // 4. Build framed request body: 5B header + "hello".
        let mut body = Vec::new();
        let mut hdr = [0u8; FRAME_HEADER_LEN];
        encode_header(false, 5, &mut hdr);
        body.extend_from_slice(&hdr);
        body.extend_from_slice(b"hello");
        let mut req_src = Box::new(ClientReq {
            bytes: body,
            offset: 0,
        });
        let mut cli_state = Box::new(ClientState::new());

        let mut cbs: *mut nghttp2_session_callbacks = ptr::null_mut();
        assert_eq!(nghttp2_session_callbacks_new(&mut cbs), 0);
        set_on_header(cbs, Some(cli_on_header));
        set_on_data(cbs, Some(cli_on_data));
        set_on_frame(cbs, Some(cli_on_frame));
        let mut client: *mut nghttp2_session = ptr::null_mut();
        assert_eq!(
            nghttp2_session_client_new(
                &mut client,
                cbs,
                cli_state.as_mut() as *mut _ as *mut c_void
            ),
            0
        );
        nghttp2_session_callbacks_del(cbs);
        assert_eq!(nghttp2_submit_settings(client, 0, ptr::null(), 0), 0);

        let nva = [
            nv(b":method", b"POST"),
            nv(b":scheme", b"http"),
            nv(b":path", b"/svc/Echo"),
            nv(b":authority", b"localhost"),
            nv(b"te", b"trailers"),
            nv(b"content-type", b"application/grpc"),
        ];
        let provider = nghttp2_data_provider {
            source: nghttp2_data_source {
                ptr: req_src.as_mut() as *mut _ as *mut c_void,
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

        // 5. Drive the round trip.
        for _ in 0..128 {
            let mut did_work = false;

            // Client → socket
            loop {
                let mut p: *const u8 = ptr::null();
                let n = nghttp2_session_mem_send(client, &mut p);
                assert!(n >= 0);
                if n == 0 || p.is_null() {
                    break;
                }
                let slice = std::slice::from_raw_parts(p, n as usize);
                stream.write_all(slice).unwrap();
                did_work = true;
            }

            // Server tick via the C ABI.
            let rc = grpcuds_conn_tick(conn);
            assert!(rc == 0 || rc == 1, "tick error: {rc}");

            // Socket → client mem_recv
            let mut buf = [0u8; 4096];
            loop {
                match stream.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let rc = nghttp2_session_mem_recv(client, buf.as_ptr(), n);
                        assert!(rc >= 0);
                        did_work = true;
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(e) => panic!("read err: {e}"),
                }
            }

            if !did_work && cli_state.end_stream_seen {
                break;
            }
            if !did_work {
                thread::sleep(Duration::from_millis(1));
            }
        }

        nghttp2_session_del(client);

        // 6. Validate from the client side.
        assert_eq!(cli_state.status.as_deref(), Some(&b"200"[..]));
        assert_eq!(
            cli_state.content_type.as_deref(),
            Some(&b"application/grpc"[..])
        );
        assert_eq!(cli_state.grpc_status.as_deref(), Some(&b"0"[..]));
        assert!(cli_state.end_stream_seen);
        assert!(cli_state.data.len() >= FRAME_HEADER_LEN);
        let parsed = match decode_header(&cli_state.data, DEFAULT_MAX_MESSAGE_LEN) {
            Ok(h) => h,
            Err(_) => panic!("decode_header failed"),
        };
        let pl = parsed.payload_len as usize;
        assert_eq!(
            &cli_state.data[FRAME_HEADER_LEN..FRAME_HEADER_LEN + pl],
            b"hello"
        );

        grpcuds_conn_free(conn);
        grpcuds_server_free(server);
    }
}
