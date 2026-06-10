// SPDX-License-Identifier: MIT OR Apache-2.0
//! Proof that the safe Rust server integrates with a tokio application.
//!
//! Two facts make it work, both exercised here:
//!
//!   1. `Server` is `Send`, and `serve` is a blocking `poll(2)` loop, so it runs
//!      on a dedicated blocking thread via `tokio::task::spawn_blocking` (NOT on
//!      an async worker, which it would otherwise monopolize).
//!   2. `ServerWriter` is `Send + Sync + Clone` and its `write`/`finish` are
//!      non-blocking (enqueue + eventfd poke), so an ordinary async `tokio::spawn`
//!      task can hold a clone and stream messages — here after `tokio::time::sleep`
//!      awaits, i.e. genuinely from the async runtime, not the I/O thread.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use grpcuds::{Server, ServerWriter, Status};
use grpcuds_core::{decode_header, FRAME_HEADER_LEN};

mod common;
use common::{call, unique_path};

// Compile-time proof of the bounds the integration relies on.
fn _assert_bounds() {
    fn is_send<T: Send>() {}
    fn is_send_sync<T: Send + Sync>() {}
    is_send::<Server>();
    is_send_sync::<ServerWriter>();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tokio_app_streams_from_async_task() {
    let sock = unique_path();

    // Build the server. The streaming handler returns immediately; an async
    // tokio task drives the stream, proving the writer crosses from the I/O
    // thread into the tokio runtime and back.
    let server = Server::builder()
        .bind(&sock)
        .add_server_streaming("/echo.Echo/Async", |_req: &[u8], w: &ServerWriter| {
            let w = w.clone();
            tokio::spawn(async move {
                for i in 0u8..4 {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    if w.write(&[b'a', i]).is_err() {
                        return;
                    }
                }
                let _ = w.finish(Status::ok());
            });
            Status::ok()
        })
        .build()
        .expect("build");

    // Run the blocking poll loop on a dedicated blocking thread.
    let shutdown = Arc::new(AtomicBool::new(false));
    let sd = shutdown.clone();
    let serve = tokio::task::spawn_blocking(move || {
        server.serve(&sd).expect("serve");
    });

    // Drive a real gRPC request. `call` is synchronous/blocking, so run it on a
    // blocking thread too rather than stalling an async worker.
    let st = tokio::task::spawn_blocking(move || call(&sock, b"/echo.Echo/Async", b"go"))
        .await
        .expect("client join");

    assert_eq!(st.status.as_deref(), Some(&b"200"[..]));
    assert_eq!(st.grpc_status.as_deref(), Some(&b"0"[..]));
    assert!(st.end_stream_seen);

    // Four framed 2-byte messages produced from the async task.
    let mut off = 0;
    let mut count = 0u8;
    while off < st.data.len() {
        let h = decode_header(&st.data[off..], 4 * 1024 * 1024)
            .ok()
            .expect("decode_header");
        let pl = h.payload_len as usize;
        assert_eq!(
            &st.data[off + FRAME_HEADER_LEN..off + FRAME_HEADER_LEN + pl],
            &[b'a', count][..]
        );
        off += FRAME_HEADER_LEN + pl;
        count += 1;
    }
    assert_eq!(count, 4, "expected 4 messages produced from the tokio task");

    shutdown.store(true, Ordering::Relaxed);
    serve.await.expect("serve join");
}
