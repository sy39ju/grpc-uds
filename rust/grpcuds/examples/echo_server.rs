// SPDX-License-Identifier: MIT OR Apache-2.0
//! Minimal echo server built on the safe Rust wrapper.
//!
//!   cargo run --example echo_server -- /tmp/grpcuds-echo.sock
//!
//! Shows how little a Rust server needs: a builder chain, closures, and a
//! `ServerWriter` — no `unsafe`, no raw pointers, no C ABI.

use grpcuds::{Server, Status, StatusCode};

fn main() -> Result<(), grpcuds::Error> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/grpcuds-echo.sock".to_string());

    let running = Server::builder()
        .bind(&path)
        // Unary: echo the request straight back. The framework writes the
        // single Ok(..) response and finishes the call.
        .add_unary("/echo.Echo/Unary", |req: &[u8]| Ok(req.to_vec()))
        // Server streaming: emit the request three times, inline, then close
        // the stream explicitly. A streaming handler owns its own `finish`.
        .add_server_streaming("/echo.Echo/Stream", |req: &[u8], w| {
            for _ in 0..3 {
                if w.write(req).is_err() {
                    return Status::code_only(StatusCode::Aborted);
                }
            }
            let _ = w.finish(Status::ok());
            Status::ok()
        })
        // Demonstrates the grpc-message trailer path: reject empty requests.
        .add_unary("/echo.Echo/NonEmpty", |req: &[u8]| {
            if req.is_empty() {
                return Err(Status::invalid_argument("request must not be empty"));
            }
            Ok(req.to_vec())
        })
        .build()?
        .run()?;

    println!("echo server listening on unix:{path}  (Ctrl-C to stop)");
    running.join() // runs until killed; Drop would also stop + join
}
