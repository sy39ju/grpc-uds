// SPDX-License-Identifier: MIT OR Apache-2.0
//! Echo server inside a tokio application (`--features tokio`):
//!
//!   cargo run --example echo_tokio --features tokio -- /tmp/grpcuds-echo.sock
//!
//! The I/O loop runs on the blocking pool via `serve_async`; the streaming
//! producer is an ordinary tokio task holding a cloned `ServerWriter`.

use grpcuds::{Server, Status};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), grpcuds::Error> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/grpcuds-echo.sock".to_string());

    let server = Server::builder()
        .bind(&path)
        .add_unary("/echo.Echo/Unary", |req: &[u8]| Ok(req.to_vec()))
        .add_server_streaming("/echo.Echo/Ticks", |_req, w| {
            let w = w.clone();
            tokio::spawn(async move {
                for i in 0u32.. {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    if w.write_owned(format!("tick {i}").into_bytes()).is_err() {
                        return; // client gone
                    }
                }
            });
            Status::ok()
        })
        .build()?;

    println!("echo server on unix:{path} — Ctrl-C to stop");
    server
        .serve_async(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
}
