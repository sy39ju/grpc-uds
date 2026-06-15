<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
# grpcuds

A tiny, wire-compatible **gRPC server over UNIX domain sockets**. Stock gRPC
clients (tonic, grpc-cpp, grpc-java, …) connect unchanged; the server side is
a `no_std`-core stack over the system `libnghttp2` that fits embedded size
budgets — measured against tonic over the same socket: **~3× smaller stripped
binary, a 4-crate dependency closure, ~2× less resident memory**, unary
latency parity (numbers and harness in the
[repository README](https://github.com/sy39ju/grpc-uds)).

```rust,no_run
use grpcuds::{Server, Status};

fn main() -> Result<(), grpcuds::Error> {
    let running = Server::builder()
        .bind("/run/echo.sock")
        .add_unary("/echo.Echo/Unary", |req: &[u8]| Ok(req.to_vec()))
        .add_server_streaming("/echo.Echo/Watch", |_req, w| {
            let w = w.clone();                       // Send + Sync + Clone
            std::thread::spawn(move || {
                while let Ok(()) = w.write(b"tick") { /* produce… */ }
            });
            Status::ok()                              // handler returns at once
        })
        .build()?
        .run()?;                                      // background I/O thread
    running.join()                                    // drop also stops it
}
```

- **Unary** handlers are `&[u8] -> Result<Vec<u8>, Status>` — exactly one
  response on `Ok`, none on `Err`; the unary wire contract is enforced by the
  signature.
- **Streaming** handlers return immediately; any thread holding a cloned
  `ServerWriter` produces via the internal mailbox (the single I/O thread is
  the only place the nghttp2 session is touched).
- **Cancellation feedback**: once the client is gone, `write`/`finish` return
  `Err(Closed)` — producer loops stop within one wasted message.
- **Backpressure**: `set_backpressure(Bounded { capacity, DropOldest|Reject })`
  bounds a stream against a slow client.
- **Typed handlers** (`prost` feature): `add_unary_msg` /
  `add_server_streaming_msg` with prost structs and a `MessageWriter<T>`;
  undecodable requests answer `INTERNAL` without invoking the handler.
- **client** (`client` feature): a blocking [`Client`] that dials a grpcuds
  (or any stock gRPC) server over UDS — `Client::connect`, then `unary` /
  `server_streaming` (and `unary_msg` / `server_streaming_msg` with `prost`).
  Off by default, so a default build is server-only and unchanged.
- **health** (`health` feature): the standard `grpc.health.v1` service —
  `health::add_health_service(builder, &reporter)` gives stock probers
  (`grpc_health_probe`, tonic-health, grpcurl) `Check` + `Watch` for free.
- **tokio** (`tokio` feature): `server.serve_async(shutdown_future).await`
  runs the loop on the blocking pool until the future resolves —
  `tonic::serve_with_shutdown`'s shape. Writers work from async tasks as-is;
  without the feature the same pattern is a one-liner over `spawn_blocking`
  (see `tests/tokio.rs`).

## Scope

Server-only (the peer is a stock gRPC client), UDS-only, unary +
server-streaming, Linux. No TLS (filesystem permissions are the boundary),
no deadlines/metadata/compression — the full capability table lives in the
repository README. If those rows matter to you, use
[tonic](https://crates.io/crates/tonic); this crate exists for the places
tonic doesn't fit.

## MSRV

Rust 1.85, all features (CI-checked).

## License

MIT OR Apache-2.0, at your option.
