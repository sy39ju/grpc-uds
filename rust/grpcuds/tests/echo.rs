// SPDX-License-Identifier: MIT OR Apache-2.0
//! Wire-level proof: a server built on the safe Rust wrapper serves a stock
//! HTTP/2 + gRPC client (hand-rolled here with nghttp2 so the test depends on
//! nothing but the transport). The server runs on a background thread via
//! `Server::serve`; the client just speaks bytes over the UDS.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use grpcuds::{Server, ServerBuilder, Status, StatusCode};
use grpcuds_core::{decode_header, FRAME_HEADER_LEN};

mod common;
use common::{call, unique_path};

// ---- Server harness --------------------------------------------------------

struct ServerHarness {
    sock: String,
    shutdown: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl ServerHarness {
    fn start(build: impl FnOnce(ServerBuilder) -> ServerBuilder) -> Self {
        let sock = unique_path();
        let server = build(Server::builder().bind(&sock)).build().expect("build");
        let shutdown = Arc::new(AtomicBool::new(false));
        let sd = shutdown.clone();
        let handle = thread::spawn(move || {
            server.serve(&sd).expect("serve");
        });
        ServerHarness {
            sock,
            shutdown,
            handle: Some(handle),
        }
    }
}

impl Drop for ServerHarness {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

// ---- Tests -----------------------------------------------------------------

#[test]
fn unary_echo_round_trip() {
    let srv =
        ServerHarness::start(|b| b.add_unary("/echo.Echo/Unary", |req: &[u8]| Ok(req.to_vec())));

    let st = call(&srv.sock, b"/echo.Echo/Unary", b"hello");
    assert_eq!(st.status.as_deref(), Some(&b"200"[..]));
    assert_eq!(st.content_type.as_deref(), Some(&b"application/grpc"[..]));
    assert_eq!(st.grpc_status.as_deref(), Some(&b"0"[..]));
    assert!(st.end_stream_seen);

    let parsed = decode_header(&st.data, 4 * 1024 * 1024)
        .ok()
        .expect("decode_header");
    let pl = parsed.payload_len as usize;
    assert_eq!(&st.data[FRAME_HEADER_LEN..FRAME_HEADER_LEN + pl], b"hello");
}

#[test]
fn server_streaming_emits_multiple_messages() {
    let srv = ServerHarness::start(|b| {
        b.add_server_streaming("/echo.Echo/Stream", |req: &[u8], w| {
            for _ in 0..3 {
                let _ = w.write(req);
            }
            let _ = w.finish(Status::ok());
            Status::ok()
        })
    });

    let st = call(&srv.sock, b"/echo.Echo/Stream", b"x");
    assert_eq!(st.grpc_status.as_deref(), Some(&b"0"[..]));
    assert!(st.end_stream_seen);

    // Three framed "x" messages: 3 * (5-byte header + 1 payload).
    let mut off = 0;
    let mut count = 0;
    while off < st.data.len() {
        let h = decode_header(&st.data[off..], 4 * 1024 * 1024)
            .ok()
            .expect("decode_header");
        let pl = h.payload_len as usize;
        assert_eq!(
            &st.data[off + FRAME_HEADER_LEN..off + FRAME_HEADER_LEN + pl],
            b"x"
        );
        off += FRAME_HEADER_LEN + pl;
        count += 1;
    }
    assert_eq!(count, 3, "expected 3 streamed messages");
}

#[test]
fn error_status_ships_grpc_message_trailer() {
    let srv = ServerHarness::start(|b| {
        b.add_unary("/echo.Echo/NonEmpty", |req: &[u8]| {
            if req.is_empty() {
                return Err(Status::new(
                    StatusCode::InvalidArgument,
                    "request must not be empty",
                ));
            }
            Ok(req.to_vec())
        })
    });

    let st = call(&srv.sock, b"/echo.Echo/NonEmpty", b"");
    // INVALID_ARGUMENT == 3, with the percent-encoded message trailer.
    assert_eq!(st.grpc_status.as_deref(), Some(&b"3"[..]));
    let msg = st.grpc_message.expect("grpc-message trailer present");
    // gRPC percent-encoding only escapes bytes outside 0x20..=0x7E (and '%'),
    // so ASCII spaces stay literal — the message ships as-is.
    assert_eq!(msg, b"request must not be empty");
    assert!(st.end_stream_seen);
}

#[test]
fn off_thread_producer_streams_then_finishes() {
    // The whole point of the Send + Sync ServerWriter: the handler returns
    // immediately, hands a *clone* of the writer to a separate producer thread,
    // and that thread streams messages (with delays, so they can't all be
    // inline) and finishes the stream from off the I/O thread. The mailbox +
    // wakeup eventfd carry every op back to the single I/O thread.
    let srv = ServerHarness::start(|b| {
        b.add_server_streaming("/echo.Echo/Background", |_req: &[u8], w| {
            let w = w.clone();
            thread::spawn(move || {
                for i in 0u8..5 {
                    thread::sleep(Duration::from_millis(5));
                    if w.write(&[b'm', i]).is_err() {
                        return;
                    }
                }
                let _ = w.finish(Status::ok());
            });
            Status::ok()
        })
    });

    let st = call(&srv.sock, b"/echo.Echo/Background", b"go");
    assert_eq!(st.grpc_status.as_deref(), Some(&b"0"[..]));
    assert!(st.end_stream_seen);

    // Five framed 2-byte messages produced from the other thread.
    let mut off = 0;
    let mut count = 0u8;
    while off < st.data.len() {
        let h = decode_header(&st.data[off..], 4 * 1024 * 1024)
            .ok()
            .expect("decode_header");
        let pl = h.payload_len as usize;
        assert_eq!(
            &st.data[off + FRAME_HEADER_LEN..off + FRAME_HEADER_LEN + pl],
            &[b'm', count][..]
        );
        off += FRAME_HEADER_LEN + pl;
        count += 1;
    }
    assert_eq!(count, 5, "expected 5 messages produced off the I/O thread");
}
