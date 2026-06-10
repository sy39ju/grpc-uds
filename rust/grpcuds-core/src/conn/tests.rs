// SPDX-License-Identifier: MIT OR Apache-2.0
//! Unit + wire-level tests for the conn module.
//!
//! ## Test taxonomy (read this before adding a test)
//!
//! The tests in this file fall into two distinct categories, and it is
//! easy to mistake one for the other:
//!
//! 1. **Dispatch-logic unit tests** — `dispatch_invokes_handler_with_payload`,
//!    `dispatch_propagates_handler_status`,
//!    `dispatch_marks_truncated_frame_internal_error`,
//!    `dispatch_is_idempotent`,
//!    `dispatch_unimplemented_when_path_unregistered`,
//!    and the ConnError-routing tests (`write_call_with_unknown_id_…`, etc.).
//!
//!    These drive `Conn::dispatch` against synthetically injected streams
//!    via `inject_completed`. They validate the **dispatch logic only**:
//!    handler resolution, state transitions, status propagation, header
//!    decoding, error routing. They DO NOT validate that nghttp2 actually
//!    puts the response on the wire — `submit_response_for` is invoked
//!    but the synthetic stream was never opened by a real client, so
//!    nghttp2 silently no-ops the submission and the test's `let _ = …`
//!    swallows the error. **Adding wire-level assertions to these tests
//!    is a category error.** The wire semantics are the responsibility
//!    of the two end-to-end tests below.
//!
//! 2. **End-to-end wire tests** — `unary_echo_round_trip` and
//!    `streaming_deferred_resume_cycle`.
//!
//!    These spin up a real nghttp2 client, submit a request through the
//!    full client → server pump, and inspect the bytes the *client* saw.
//!    They cover the full callback chain plus the data_provider deferred /
//!    resume path. Together with the integration tests in
//!    `tests/rust/cross/tests/*.rs` they are the contract that says
//!    "the wire works".
//!
//! When a future test needs to assert "the server actually wrote X to the
//! wire", model it on the category-2 tests, not the category-1 ones.

use super::callbacks::{on_stream_close, FLAG_END_STREAM};
use super::out_queue::OutQueue;
use super::state::{CancelHook, Conn, ConnError, ConnState, StreamCtx, StreamState};
use super::{Backpressure, OverflowPolicy};
use crate::framing::{decode_header, encode_header, DEFAULT_MAX_MESSAGE_LEN, FRAME_HEADER_LEN};
use crate::headers::GrpcStatus;
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::ffi::c_void;
use core::num::NonZeroUsize;
use core::ptr;

use grpcuds_sys::{
    nghttp2_data_provider, nghttp2_data_source, nghttp2_frame, nghttp2_nv, nghttp2_session,
    nghttp2_session_callbacks, nghttp2_session_callbacks_del, nghttp2_session_callbacks_new,
    nghttp2_session_client_new, nghttp2_session_del, nghttp2_session_mem_recv,
    nghttp2_session_mem_send, nghttp2_submit_request, nghttp2_submit_settings,
};

fn nv(name: &'static [u8], value: &'static [u8]) -> nghttp2_nv {
    nghttp2_nv {
        name: name.as_ptr() as *mut u8,
        value: value.as_ptr() as *mut u8,
        namelen: name.len(),
        valuelen: value.len(),
        flags: 0,
    }
}

/// Drive client ↔ server with no server-side dispatch. Used by tests
/// that want to observe the request-side callbacks in isolation.
unsafe fn pump_no_dispatch(client: *mut nghttp2_session, server: &mut Conn) {
    for _ in 0..32 {
        let mut did_work = false;
        loop {
            let mut p: *const u8 = ptr::null();
            let n = nghttp2_session_mem_send(client, &mut p);
            assert!(n >= 0, "client mem_send failed: {n}");
            if n == 0 || p.is_null() {
                break;
            }
            let slice = core::slice::from_raw_parts(p, n as usize);
            let consumed = server.recv(slice).unwrap();
            assert_eq!(consumed, slice.len());
            did_work = true;
        }
        loop {
            let owned: Vec<u8> = {
                let bytes = server.pull_send().unwrap();
                if bytes.is_empty() {
                    break;
                }
                bytes.to_vec()
            };
            let n = nghttp2_session_mem_recv(client, owned.as_ptr(), owned.len());
            assert!(n >= 0, "client mem_recv failed: {n}");
            did_work = true;
        }
        if !did_work {
            break;
        }
    }
}

#[test]
fn captures_path_and_completes_on_end_stream() {
    let mut server = Conn::new_server().unwrap();

    unsafe {
        let mut cbs: *mut nghttp2_session_callbacks = ptr::null_mut();
        assert_eq!(nghttp2_session_callbacks_new(&mut cbs), 0);
        let mut client: *mut nghttp2_session = ptr::null_mut();
        assert_eq!(
            nghttp2_session_client_new(&mut client, cbs, ptr::null_mut()),
            0
        );
        nghttp2_session_callbacks_del(cbs);
        assert_eq!(nghttp2_submit_settings(client, 0, ptr::null(), 0), 0);

        let nva = [
            nv(b":method", b"POST"),
            nv(b":scheme", b"http"),
            nv(b":path", b"/svc/Method"),
            nv(b":authority", b"localhost"),
            nv(b"te", b"trailers"),
            nv(b"content-type", b"application/grpc"),
        ];
        // data_prd = NULL → HEADERS frame carries END_STREAM.
        let sid = nghttp2_submit_request(
            client,
            ptr::null(),
            nva.as_ptr(),
            nva.len(),
            ptr::null(),
            ptr::null_mut(),
        );
        assert!(sid > 0, "submit_request rc={sid}");

        pump_no_dispatch(client, &mut server);

        nghttp2_session_del(client);
    }

    let streams = server.streams();
    assert_eq!(streams.len(), 1, "exactly one stream tracked");
    assert_eq!(&streams[0].path[..], b"/svc/Method");
    assert_eq!(streams[0].state, StreamState::Complete);
    assert!(streams[0].request.is_empty(), "no body expected");
}

#[test]
fn server_emits_initial_settings_immediately() {
    // Right after creation, server has SETTINGS waiting in its outbound
    // buffer. pull_send should return non-empty without any peer input.
    let mut server = Conn::new_server().unwrap();
    let bytes = server.pull_send().unwrap();
    assert!(!bytes.is_empty(), "expected SETTINGS waiting to be sent");
    assert!(server.wants_read());
}

// ---- dispatch tests ----------------------------------------------

/// Push a synthetic stream in `Complete` state with a single gRPC-framed
/// request whose payload is `body`. Used by dispatch tests so we don't
/// have to drive a full HTTP/2 client through the callbacks.
fn inject_completed(conn: &mut Conn, id: i32, path: &[u8], body: &[u8]) {
    let mut request = Vec::with_capacity(FRAME_HEADER_LEN + body.len());
    let mut hdr = [0u8; FRAME_HEADER_LEN];
    encode_header(false, body.len() as u32, &mut hdr);
    request.extend_from_slice(&hdr);
    request.extend_from_slice(body);
    conn.state.streams.push(Box::new(StreamCtx {
        id,
        state: StreamState::Complete,
        request,
        path: path.to_vec(),
        out: OutQueue::new(),
        status: GrpcStatus::Ok,
        cancel_hook: None,
        deadline_ms: None,
    }));
}

unsafe extern "C" fn echo_handler(
    conn: *mut Conn,
    call_id: i32,
    req: *const u8,
    req_len: usize,
    _ud: *mut c_void,
) -> i32 {
    let c = &mut *conn;
    let req_slice = core::slice::from_raw_parts(req, req_len);
    let _ = c.write_call(call_id, req_slice);
    let _ = c.finish_call(call_id, GrpcStatus::Ok);
    0
}

unsafe extern "C" fn fail_handler(
    _conn: *mut Conn,
    _call_id: i32,
    _req: *const u8,
    _req_len: usize,
    _ud: *mut c_void,
) -> i32 {
    // Return non-OK without finishing — dispatch should auto-finish.
    GrpcStatus::InvalidArgument as i32
}

fn first_queued_payload(s: &StreamCtx) -> Option<&[u8]> {
    s.out.messages.front().map(|m| m.body())
}

#[test]
fn dispatch_invokes_handler_with_payload() {
    let mut server = Conn::new_server().unwrap();
    server
        .register_method(b"/svc/Echo", echo_handler, ptr::null_mut())
        .unwrap();

    inject_completed(&mut server, 1, b"/svc/Echo", b"hello");
    assert_eq!(server.dispatch(), 1);

    let s = &server.streams()[0];
    assert_eq!(s.state, StreamState::Dispatched);
    assert!(s.out.finished);
    assert_eq!(s.out.final_status, GrpcStatus::Ok);
    // Handler wrote one framed message; payload (after 5B prefix) == request.
    assert_eq!(first_queued_payload(s), Some(&b"hello"[..]));
}

#[test]
fn dispatch_unimplemented_when_path_unregistered() {
    let mut server = Conn::new_server().unwrap();
    inject_completed(&mut server, 1, b"/svc/Missing", b"");
    assert_eq!(server.dispatch(), 1);

    let s = &server.streams()[0];
    assert_eq!(s.state, StreamState::Dispatched);
    assert!(s.out.finished);
    assert_eq!(s.out.final_status, GrpcStatus::Unimplemented);
    assert!(s.out.messages.is_empty());
}

#[test]
fn dispatch_propagates_handler_status() {
    let mut server = Conn::new_server().unwrap();
    server
        .register_method(b"/svc/Fail", fail_handler, ptr::null_mut())
        .unwrap();
    inject_completed(&mut server, 1, b"/svc/Fail", b"x");
    assert_eq!(server.dispatch(), 1);

    let s = &server.streams()[0];
    assert_eq!(s.state, StreamState::Dispatched);
    assert!(s.out.finished);
    assert_eq!(s.out.final_status, GrpcStatus::InvalidArgument);
}

#[test]
fn dispatch_marks_truncated_frame_internal_error() {
    let mut server = Conn::new_server().unwrap();
    server
        .register_method(b"/svc/Echo", echo_handler, ptr::null_mut())
        .unwrap();

    // Header claims 10 bytes but only 3 follow.
    let mut request = Vec::new();
    let mut hdr = [0u8; FRAME_HEADER_LEN];
    encode_header(false, 10, &mut hdr);
    request.extend_from_slice(&hdr);
    request.extend_from_slice(b"abc");
    server.state.streams.push(Box::new(StreamCtx {
        id: 1,
        state: StreamState::Complete,
        request,
        path: b"/svc/Echo".to_vec(),
        out: OutQueue::new(),
        status: GrpcStatus::Ok,
        cancel_hook: None,
        deadline_ms: None,
    }));

    assert_eq!(server.dispatch(), 1);
    let s = &server.streams()[0];
    assert_eq!(s.state, StreamState::Dispatched);
    assert_eq!(s.status, GrpcStatus::Internal);
    assert!(s.out.messages.is_empty(), "echo handler must not have run");
}

#[test]
fn dispatch_is_idempotent() {
    // Re-running dispatch over already-Dispatched streams should be a no-op.
    let mut server = Conn::new_server().unwrap();
    server
        .register_method(b"/svc/Echo", echo_handler, ptr::null_mut())
        .unwrap();
    inject_completed(&mut server, 1, b"/svc/Echo", b"once");
    assert_eq!(server.dispatch(), 1);
    assert_eq!(server.dispatch(), 0);
    // Queue still has the one message the handler enqueued.
    assert_eq!(
        first_queued_payload(&server.streams()[0]),
        Some(&b"once"[..])
    );
}

// ---- OutQueue / data_provider unit tests --------------------------------

#[test]
fn out_queue_drains_partial_chunks() {
    let mut q = OutQueue::new();
    q.enqueue_framed(b"ABCDE").unwrap();
    // 5B header + 5B payload = 10 bytes total.
    let mut chunk = [0u8; 4];
    assert_eq!(q.drain_into(&mut chunk), 4);
    assert_eq!(q.drain_into(&mut chunk), 4);
    let mut tail = [0u8; 8];
    assert_eq!(q.drain_into(&mut tail), 2);
    assert!(q.messages.is_empty(), "queue drained");
}

#[test]
fn out_queue_drains_multiple_messages() {
    let mut q = OutQueue::new();
    q.enqueue_framed(b"hi").unwrap();
    q.enqueue_framed(b"there").unwrap();
    let mut big = [0u8; 64];
    let n = q.drain_into(&mut big);
    // 5+2 + 5+5 = 17 bytes
    assert_eq!(n, 17);
    // Spot check: second message's payload starts at index 7+5=12.
    assert_eq!(&big[12..17], b"there");
}

// ---- Overflow-policy unit tests ---------------------------------------

/// Extract the single-byte payload of every queued message (assumes 1-byte
/// payloads, which is what the burst tests below use).
fn payload_bytes(q: &OutQueue) -> Vec<u8> {
    q.messages.iter().map(|m| m.body()[0]).collect()
}

fn cap3(policy: OverflowPolicy) -> Backpressure {
    Backpressure::Bounded {
        capacity: NonZeroUsize::new(3).unwrap(),
        policy,
    }
}

#[test]
fn out_queue_drop_oldest_keeps_latest_n() {
    let mut q = OutQueue::new();
    q.backpressure = cap3(OverflowPolicy::DropOldest);
    for seq in 0u8..10 {
        // All writes succeed under DropOldest.
        q.enqueue_framed(&[seq]).unwrap();
    }
    assert_eq!(payload_bytes(&q), vec![7, 8, 9], "should keep latest 3");
}

#[test]
fn out_queue_reject_returns_error_when_full() {
    let mut q = OutQueue::new();
    q.backpressure = cap3(OverflowPolicy::Reject);
    let mut rejected = 0;
    for seq in 0u8..10 {
        match q.enqueue_framed(&[seq]) {
            Ok(()) => {}
            Err(ConnError::QueueFull) => rejected += 1,
            Err(e) => panic!("unexpected error: {e:?}"),
        }
    }
    assert_eq!(rejected, 7, "first 3 accepted, the rest rejected");
    assert_eq!(payload_bytes(&q), vec![0, 1, 2], "first 3 kept");
}

#[test]
fn out_queue_drop_oldest_preserves_inflight_head() {
    let mut q = OutQueue::new();
    q.backpressure = cap3(OverflowPolicy::DropOldest);
    // Fill the queue and partially drain the head so front_offset > 0.
    for seq in 0u8..3 {
        q.enqueue_framed(&[seq]).unwrap();
    }
    let mut small = [0u8; 3]; // half of message 0 (5B header + 1B payload = 6)
    let n = q.drain_into(&mut small);
    assert_eq!(n, 3);
    assert!(q.front_offset > 0 && q.front_offset < 6);
    // Now push more — DropOldest must NOT touch the head (m0). The
    // capacity counts UNSTARTED messages, so [m0(in-flight), …] can
    // legitimately end up with 4 entries: head + 3 unstarted.
    for seq in 3u8..6 {
        q.enqueue_framed(&[seq]).unwrap();
    }
    // Walkthrough: start = [m0*, m1, m2] (unstarted=2)
    //   push m3 → [m0*, m1, m2, m3] (unstarted=3, at cap, no eviction this push)
    //   push m4 → drop m1 → [m0*, m2, m3, m4]
    //   push m5 → drop m2 → [m0*, m3, m4, m5]
    assert_eq!(payload_bytes(&q), vec![0, 3, 4, 5]);
}

#[test]
fn out_queue_drop_oldest_preserves_promised_head() {
    // A NO_COPY promise pins the head even at front_offset == 0: between the
    // read callback's promise and the send_data callback's consume (e.g. a
    // WOULDBLOCK window), DropOldest must evict m1, never the promised m0.
    let mut q = OutQueue::new();
    q.backpressure = cap3(OverflowPolicy::DropOldest);
    for seq in 0u8..3 {
        q.enqueue_framed(&[seq]).unwrap();
    }
    let promised = q.promise_front(usize::MAX);
    assert_eq!(promised, 6, "5B header + 1B payload");
    assert!(q.front_committed);

    q.enqueue_framed(&[3]).unwrap(); // [m0†, m1, m2, m3] — at cap, no eviction
    q.enqueue_framed(&[4]).unwrap(); // drop m1 → [m0†, m2, m3, m4]
    assert_eq!(payload_bytes(&q), vec![0, 2, 3, 4]);

    // The promised regions are exactly the head bytes; consuming them pops
    // the head and releases the pin.
    let (hdr, body) = q.front_regions(promised);
    assert_eq!(hdr.len() + body.len(), promised);
    assert_eq!(body, &[0u8][..]);
    q.consume_front(promised);
    assert!(!q.front_committed);
    assert_eq!(payload_bytes(&q), vec![2, 3, 4]);
}

#[test]
fn out_queue_unbounded_by_default() {
    let mut q = OutQueue::new();
    assert_eq!(q.backpressure, Backpressure::Unbounded);
    for seq in 0u8..50 {
        q.enqueue_framed(&[seq]).unwrap();
    }
    assert_eq!(q.messages.len(), 50);
}

// ---- ConnError variant routing (CA-1) ---------------------------------

#[test]
fn write_call_with_unknown_id_returns_stream_not_found() {
    let mut server = Conn::new_server().unwrap();
    // No stream has ever been opened.
    match server.write_call(42, b"hi") {
        Err(ConnError::StreamNotFound) => {}
        other => panic!("expected StreamNotFound, got {other:?}"),
    }
}

#[test]
fn write_call_after_finish_returns_stream_finished() {
    let mut server = Conn::new_server().unwrap();
    inject_completed(&mut server, 1, b"/svc/Echo", b"x");
    // Manually finish the stream without going through dispatch.
    server.state.streams[0].out.finished = true;

    match server.write_call(1, b"too late") {
        Err(ConnError::StreamFinished) => {}
        other => panic!("expected StreamFinished, got {other:?}"),
    }
}

#[test]
fn finish_call_with_unknown_id_returns_stream_not_found() {
    let mut server = Conn::new_server().unwrap();
    match server.finish_call(99, GrpcStatus::Ok) {
        Err(ConnError::StreamNotFound) => {}
        other => panic!("expected StreamNotFound, got {other:?}"),
    }
}

#[test]
fn set_stream_policy_with_unknown_id_returns_stream_not_found() {
    let mut server = Conn::new_server().unwrap();
    match server.set_stream_policy(7, cap3(OverflowPolicy::DropOldest)) {
        Err(ConnError::StreamNotFound) => {}
        other => panic!("expected StreamNotFound, got {other:?}"),
    }
}

#[test]
fn set_cancel_hook_with_unknown_id_returns_stream_not_found() {
    let mut server = Conn::new_server().unwrap();
    unsafe extern "C" fn noop(_: *mut c_void) {}
    match server.set_cancel_hook(13, noop, ptr::null_mut()) {
        Err(ConnError::StreamNotFound) => {}
        other => panic!("expected StreamNotFound, got {other:?}"),
    }
}

#[test]
fn set_cancel_hook_replaces_previous() {
    let mut server = Conn::new_server().unwrap();
    inject_completed(&mut server, 1, b"/svc/Stream", b"");

    let a = Box::new(AtomicBool::new(false));
    let b = Box::new(AtomicBool::new(false));
    let a_ptr = &*a as *const AtomicBool as *mut c_void;
    let b_ptr = &*b as *const AtomicBool as *mut c_void;
    unsafe extern "C" fn flip(ud: *mut c_void) {
        (*(ud as *const AtomicBool)).store(true, Ordering::SeqCst);
    }

    server.set_cancel_hook(1, flip, a_ptr).unwrap();
    server.set_cancel_hook(1, flip, b_ptr).unwrap();

    // Simulate a cancel close — only the most-recently-installed hook
    // should fire (a should stay false, b should flip).
    let ud = &mut *server.state as *mut ConnState as *mut c_void;
    unsafe {
        on_stream_close(ptr::null_mut(), 1, /*INTERNAL_ERROR*/ 2, ud);
    }
    assert!(!a.load(Ordering::SeqCst), "previous hook must not fire");
    assert!(b.load(Ordering::SeqCst), "replacement hook must fire");
}

// ---- cancel hook --------------------------------------------------------

use core::sync::atomic::{AtomicBool, Ordering};

/// Cleanup function that flips the AtomicBool whose address came in
/// via user_data. Each test passes its own atomic so cargo's parallel
/// runner can't cross-contaminate them.
unsafe extern "C" fn flip_flag(ud: *mut c_void) {
    let flag = &*(ud as *const AtomicBool);
    flag.store(true, Ordering::SeqCst);
}

#[test]
fn cancel_hook_fires_on_nonzero_close() {
    let cancelled = Box::new(AtomicBool::new(false));
    let flag_ptr = &*cancelled as *const AtomicBool as *mut c_void;

    let mut server = Conn::new_server().unwrap();
    server.state.streams.push(Box::new(StreamCtx {
        id: 1,
        state: StreamState::Dispatched,
        request: Vec::new(),
        path: b"/svc/Stream".to_vec(),
        out: OutQueue::new(),
        status: GrpcStatus::Ok,
        cancel_hook: Some(CancelHook {
            callback: flip_flag,
            user_data: flag_ptr,
        }),
        deadline_ms: None,
    }));

    let ud = &mut *server.state as *mut ConnState as *mut c_void;
    unsafe {
        on_stream_close(ptr::null_mut(), 1, /*INTERNAL_ERROR*/ 2, ud);
    }

    assert!(cancelled.load(Ordering::SeqCst), "hook must fire on cancel");
    assert!(
        server.streams().is_empty(),
        "closed stream context must be dropped (per-call leak otherwise)"
    );
}

#[test]
fn cancel_hook_does_not_fire_on_clean_close() {
    let cancelled = Box::new(AtomicBool::new(false));
    let flag_ptr = &*cancelled as *const AtomicBool as *mut c_void;

    let mut server = Conn::new_server().unwrap();
    server.state.streams.push(Box::new(StreamCtx {
        id: 1,
        state: StreamState::Dispatched,
        request: Vec::new(),
        path: b"/svc/Stream".to_vec(),
        out: OutQueue::new(),
        status: GrpcStatus::Ok,
        cancel_hook: Some(CancelHook {
            callback: flip_flag,
            user_data: flag_ptr,
        }),
        deadline_ms: None,
    }));

    let ud = &mut *server.state as *mut ConnState as *mut c_void;
    unsafe {
        on_stream_close(ptr::null_mut(), 1, /*NO_ERROR*/ 0, ud);
    }

    assert!(
        !cancelled.load(Ordering::SeqCst),
        "hook must not fire on clean close"
    );
    // The context (hook included) is dropped with the stream — nothing
    // accumulates on the connection.
    assert!(server.streams().is_empty());
}

// ---- end-to-end wire test -------------------------------------------
//
// Spin up a real nghttp2 client, submit a unary echo request with a
// 5B-framed body via a client-side data_provider, drive both sessions
// until quiescence, and inspect what the client received: status 200 +
// application/grpc + framed echo payload + grpc-status: 0.

use grpcuds_sys::{
    nghttp2_data_flag_NGHTTP2_DATA_FLAG_EOF as DATA_EOF,
    nghttp2_session_callbacks_set_on_data_chunk_recv_callback as set_on_data,
    nghttp2_session_callbacks_set_on_frame_recv_callback as set_on_frame,
    nghttp2_session_callbacks_set_on_header_callback as set_on_header,
};

struct ClientState {
    // Headers the server sent (collected across both HEADERS frames).
    status: Option<Vec<u8>>,
    content_type: Option<Vec<u8>>,
    grpc_status: Option<Vec<u8>>,
    // DATA frame payload(s) concatenated.
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
    _session: *mut nghttp2_session,
    _frame: *const nghttp2_frame,
    name: *const u8,
    namelen: usize,
    value: *const u8,
    valuelen: usize,
    _flags: u8,
    user_data: *mut c_void,
) -> i32 {
    let st = &mut *(user_data as *mut ClientState);
    let n = core::slice::from_raw_parts(name, namelen);
    let v = core::slice::from_raw_parts(value, valuelen).to_vec();
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
    _session: *mut nghttp2_session,
    _flags: u8,
    _stream_id: i32,
    data: *const u8,
    len: usize,
    user_data: *mut c_void,
) -> i32 {
    let st = &mut *(user_data as *mut ClientState);
    st.data
        .extend_from_slice(core::slice::from_raw_parts(data, len));
    0
}

unsafe extern "C" fn cli_on_frame(
    _session: *mut nghttp2_session,
    frame: *const nghttp2_frame,
    user_data: *mut c_void,
) -> i32 {
    let st = &mut *(user_data as *mut ClientState);
    if (*frame).hd.flags & FLAG_END_STREAM != 0 {
        st.end_stream_seen = true;
    }
    0
}

/// Client-side data provider: streams a fixed byte sequence and signals
/// EOF when exhausted.
struct ClientReq {
    bytes: Vec<u8>,
    offset: usize,
}
unsafe extern "C" fn cli_data_read(
    _session: *mut nghttp2_session,
    _stream_id: i32,
    buf: *mut u8,
    length: usize,
    data_flags: *mut u32,
    source: *mut nghttp2_data_source,
    _user_data: *mut c_void,
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

/// Pump client ↔ server bytes until both quiesce or 64 cycles elapse.
/// In between cycles we run `server.dispatch()` so that newly Complete
/// streams get their handlers invoked and the response gets queued.
unsafe fn pump(client: *mut nghttp2_session, server: &mut Conn) {
    for _ in 0..64 {
        let mut did_work = false;
        loop {
            let mut p: *const u8 = ptr::null();
            let n = nghttp2_session_mem_send(client, &mut p);
            assert!(n >= 0, "client mem_send: {n}");
            if n == 0 || p.is_null() {
                break;
            }
            let slice = core::slice::from_raw_parts(p, n as usize);
            let used = server.recv(slice).unwrap();
            assert_eq!(used, slice.len());
            did_work = true;
        }
        if server.dispatch() > 0 {
            did_work = true;
        }
        loop {
            let owned: Vec<u8> = {
                let bytes = server.pull_send().unwrap();
                if bytes.is_empty() {
                    break;
                }
                bytes.to_vec()
            };
            let n = nghttp2_session_mem_recv(client, owned.as_ptr(), owned.len());
            assert!(n >= 0, "client mem_recv: {n}");
            did_work = true;
        }
        if !did_work {
            break;
        }
    }
}

#[test]
fn unary_echo_round_trip() {
    let mut server = Conn::new_server().unwrap();
    server
        .register_method(b"/svc/Echo", echo_handler, ptr::null_mut())
        .unwrap();

    // Build request body: 5B prefix + "hello".
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

    unsafe {
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

        pump(client, &mut server);

        nghttp2_session_del(client);
    }

    // Server side: nghttp2 closed the stream after shipping the full
    // response (HEADERS + DATA + trailing HEADERS) and the per-call
    // context was dropped with it.
    assert!(server.streams().is_empty());

    // Client side: should have received "200" + application/grpc +
    // framed payload (5B prefix + "hello") + grpc-status: 0 + end-of-stream.
    assert_eq!(cli_state.status.as_deref(), Some(&b"200"[..]));
    assert_eq!(
        cli_state.content_type.as_deref(),
        Some(&b"application/grpc"[..])
    );
    assert_eq!(cli_state.grpc_status.as_deref(), Some(&b"0"[..]));
    assert!(
        cli_state.end_stream_seen,
        "trailing HEADERS must close stream"
    );
    assert!(
        cli_state.data.len() >= FRAME_HEADER_LEN,
        "DATA frame missing"
    );
    // Decode the 5B-prefixed payload in the DATA bytes.
    let parsed = decode_header(&cli_state.data, DEFAULT_MAX_MESSAGE_LEN).unwrap();
    let pl = parsed.payload_len as usize;
    assert_eq!(
        &cli_state.data[FRAME_HEADER_LEN..FRAME_HEADER_LEN + pl],
        b"hello"
    );
}

/// Streaming handler that records its call_id and returns without
/// writing anything. The test then drives the call from outside.
unsafe extern "C" fn streaming_handler(
    _conn: *mut Conn,
    call_id: i32,
    _req: *const u8,
    _req_len: usize,
    ud: *mut c_void,
) -> i32 {
    let slot = &mut *(ud as *mut i32);
    *slot = call_id;
    0
}

#[test]
fn streaming_deferred_resume_cycle() {
    let mut server = Conn::new_server().unwrap();
    // The handler will write its call_id into `captured_id` so we can
    // address subsequent write_call / finish_call from the test body.
    let mut captured_id: Box<i32> = Box::new(0);
    let ud_ptr = captured_id.as_mut() as *mut i32 as *mut c_void;
    server
        .register_method(b"/svc/Stream", streaming_handler, ud_ptr)
        .unwrap();

    let mut cli_state = Box::new(ClientState::new());

    unsafe {
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

        // gRPC server-streaming: client still sends one (possibly empty)
        // request message. Ship a 5B prefix with payload_len=0 so the
        // dispatcher sees a valid frame.
        let body = alloc::vec![0u8; FRAME_HEADER_LEN];
        let mut req_src = Box::new(ClientReq {
            bytes: body,
            offset: 0,
        });
        let nva = [
            nv(b":method", b"POST"),
            nv(b":scheme", b"http"),
            nv(b":path", b"/svc/Stream"),
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

        // Phase 1: handler runs and returns without writing. Server has
        // submitted response HEADERS; data_provider returns DEFERRED so
        // no DATA frame is on the wire yet.
        pump(client, &mut server);
        assert_eq!(*captured_id, sid, "handler should have observed call_id");
        assert_eq!(cli_state.status.as_deref(), Some(&b"200"[..]));
        assert!(cli_state.data.is_empty(), "no DATA until first write");
        assert!(cli_state.grpc_status.is_none(), "no trailer yet");
        assert_eq!(server.streams()[0].state, StreamState::Dispatched);
        assert!(!server.streams()[0].out.finished);

        // Phase 2: app pushes a message → nghttp2 resumes DATA → client
        // observes the framed payload.
        server.write_call(*captured_id, b"msg1").unwrap();
        pump(client, &mut server);
        assert!(cli_state.data.len() >= FRAME_HEADER_LEN + 4);
        let h = decode_header(&cli_state.data, DEFAULT_MAX_MESSAGE_LEN).unwrap();
        assert_eq!(h.payload_len, 4);
        assert_eq!(
            &cli_state.data[FRAME_HEADER_LEN..FRAME_HEADER_LEN + 4],
            b"msg1"
        );
        assert!(cli_state.grpc_status.is_none(), "still no trailer");

        // Phase 3: second message — verifies resume-after-drain works.
        let baseline_len = cli_state.data.len();
        server.write_call(*captured_id, b"msg2").unwrap();
        pump(client, &mut server);
        assert!(cli_state.data.len() > baseline_len);
        assert!(
            cli_state.data.windows(4).any(|w| w == b"msg2"),
            "second payload should appear in client DATA stream"
        );

        // Phase 4: finish → trailer with grpc-status:0 + END_STREAM.
        server.finish_call(*captured_id, GrpcStatus::Ok).unwrap();
        pump(client, &mut server);
        assert_eq!(cli_state.grpc_status.as_deref(), Some(&b"0"[..]));
        assert!(cli_state.end_stream_seen);
        assert!(server.streams().is_empty(), "closed stream context dropped");

        nghttp2_session_del(client);
    }
}

// ---- server-side grpc-timeout deadlines --------------------------------------

/// A dispatched-but-deferred call whose client sent `grpc-timeout` is
/// finished with DEADLINE_EXCEEDED (and its cancel hook fired) once the
/// deadline passes — the stock-gRPC server behavior.
#[test]
fn grpc_timeout_expires_deferred_call_with_deadline_exceeded() {
    let mut server = Conn::new_server().unwrap();
    let mut captured_id: Box<i32> = Box::new(0);
    let ud_ptr = captured_id.as_mut() as *mut i32 as *mut c_void;
    server
        .register_method(b"/svc/Stream", streaming_handler, ud_ptr)
        .unwrap();

    let mut cli_state = Box::new(ClientState::new());
    let mut body = Vec::new();
    let mut hdr = [0u8; FRAME_HEADER_LEN];
    encode_header(false, 2, &mut hdr);
    body.extend_from_slice(&hdr);
    body.extend_from_slice(b"go");
    let mut req_src = Box::new(ClientReq {
        bytes: body,
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
            nv(b":path", b"/svc/Stream"),
            nv(b":authority", b"localhost"),
            nv(b"te", b"trailers"),
            nv(b"content-type", b"application/grpc"),
            nv(b"grpc-timeout", b"60m"),
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

        pump(client, &mut server);
        assert!(*captured_id > 0, "handler must have run (deferred)");
        assert!(
            cli_state.grpc_status.is_none(),
            "call still open before the deadline"
        );

        // Arm a cancel hook (the deferred producer's cleanup) and let the
        // deadline lapse.
        let cancelled = Box::new(AtomicBool::new(false));
        let flag_ptr = &*cancelled as *const AtomicBool as *mut c_void;
        server
            .set_cancel_hook(*captured_id, flip_flag, flag_ptr)
            .unwrap();
        assert_eq!(server.next_deadline_ms().map(|r| r <= 60), Some(true));

        std::thread::sleep(std::time::Duration::from_millis(80));
        server.expire_deadlines();
        pump(client, &mut server);

        assert!(
            cancelled.load(Ordering::SeqCst),
            "expiry must fire the cancel hook"
        );
        assert_eq!(
            cli_state.grpc_status.as_deref(),
            Some(&b"4"[..]),
            "client must see DEADLINE_EXCEEDED trailers"
        );
        assert!(cli_state.end_stream_seen);
        assert!(server.next_deadline_ms().is_none(), "deadline disarmed");

        nghttp2_session_del(client);
    }
}

/// A call WITHOUT grpc-timeout never expires.
#[test]
fn no_grpc_timeout_means_no_deadline() {
    let mut server = Conn::new_server().unwrap();
    let mut captured_id: Box<i32> = Box::new(0);
    let ud_ptr = captured_id.as_mut() as *mut i32 as *mut c_void;
    server
        .register_method(b"/svc/Stream", streaming_handler, ud_ptr)
        .unwrap();
    assert!(server.next_deadline_ms().is_none());
    server.expire_deadlines(); // no-op, must not panic
}

// ---- per-call time-remaining query --------------------------------------------

#[test]
fn call_time_remaining_tracks_the_grpc_timeout_budget() {
    let mut server = Conn::new_server().unwrap();
    server.state.streams.push(Box::new(StreamCtx {
        id: 1,
        state: StreamState::Dispatched,
        request: Vec::new(),
        path: b"/svc/M".to_vec(),
        out: OutQueue::new(),
        status: GrpcStatus::Ok,
        cancel_hook: None,
        deadline_ms: Some(crate::monotonic_ms() + 5_000),
    }));

    let left = server.call_time_remaining_ms(1).unwrap().unwrap();
    assert!(
        left > 4_000 && left <= 5_000,
        "5s budget reads back as ~5s, got {left}ms"
    );

    // A lapsed deadline reads 0 (due), never underflows.
    server.state.streams[0].deadline_ms = Some(crate::monotonic_ms().saturating_sub(1_000));
    assert_eq!(server.call_time_remaining_ms(1).unwrap(), Some(0));
}

#[test]
fn call_time_remaining_without_deadline_is_none() {
    let mut server = Conn::new_server().unwrap();
    server.state.streams.push(Box::new(StreamCtx {
        id: 7,
        state: StreamState::Dispatched,
        request: Vec::new(),
        path: b"/svc/M".to_vec(),
        out: OutQueue::new(),
        status: GrpcStatus::Ok,
        cancel_hook: None,
        deadline_ms: None,
    }));
    assert_eq!(server.call_time_remaining_ms(7).unwrap(), None);
}

#[test]
fn call_time_remaining_unknown_id_is_stream_not_found() {
    let server = Conn::new_server().unwrap();
    assert!(matches!(
        server.call_time_remaining_ms(99),
        Err(ConnError::StreamNotFound)
    ));
}
