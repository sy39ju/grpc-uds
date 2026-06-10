// SPDX-License-Identifier: MIT OR Apache-2.0
//! `Server::serve_async` (the `tokio` feature): graceful shutdown via a
//! future, and the full wire path while running on the blocking pool.

use std::time::Duration;

use grpcuds::{Server, ServerWriter, Status};

mod common;
use common::{call, unique_path};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serve_async_serves_and_shuts_down_on_future() {
    let sock = unique_path();
    let server = Server::builder()
        .bind(&sock)
        .add_unary("/echo.Echo/Unary", |req: &[u8]| Ok(req.to_vec()))
        .add_server_streaming("/echo.Echo/Async", |_req: &[u8], w: &ServerWriter| {
            let w = w.clone();
            tokio::spawn(async move {
                for i in 0u8..3 {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    if w.write(&[b'x', i]).is_err() {
                        return;
                    }
                }
                let _ = w.finish(Status::ok());
            });
            Status::ok()
        })
        .build()
        .expect("build");

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let serving = tokio::spawn(server.serve_async(async {
        let _ = stop_rx.await;
    }));

    // Drive real calls (blocking client → blocking pool).
    let s2 = sock.clone();
    let st = tokio::task::spawn_blocking(move || call(&s2, b"/echo.Echo/Unary", b"hello"))
        .await
        .expect("client join");
    assert_eq!(st.grpc_status.as_deref(), Some(&b"0"[..]));

    let st = tokio::task::spawn_blocking(move || call(&sock, b"/echo.Echo/Async", b"go"))
        .await
        .expect("client join");
    assert_eq!(st.grpc_status.as_deref(), Some(&b"0"[..]));
    assert!(st.end_stream_seen);

    // Graceful shutdown: resolve the future, serve_async returns Ok.
    stop_tx.send(()).expect("server still running");
    let res = tokio::time::timeout(Duration::from_secs(5), serving)
        .await
        .expect("serve_async did not stop after the shutdown future resolved")
        .expect("task join");
    res.expect("serve_async returned an error");
}
