<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
# grpcuds-build

`build.rs` codegen for [grpcuds](https://crates.io/crates/grpcuds) servers
and clients — `tonic-build`'s shape. prost generates the messages; this crate
adds a service generator that emits, per `service`:

- a **server trait** (unary: `fn m(&self, req) -> Result<Resp, Status>`;
  server-streaming: `fn m(&self, req, writer: MessageWriter<Resp>) -> Status`)
  plus an `add_*_service` registration function;
- a typed **`*Client` stub** wrapping `grpcuds::Client`, one method per RPC
  with the gRPC paths baked in.

Either half can be switched off: `configure().build_client(false)` /
`.build_server(false)`.

```rust,ignore
// build.rs
grpcuds_build::compile_protos("proto/ble.proto")?;

// src/lib.rs
pub mod ble { grpcuds::include_proto!("ble"); }

// server: implement the trait, register it
impl ble::BleService for MySim { /* one fn per rpc */ }
let b = ble::add_ble_service(Server::builder().bind(sock), Arc::new(MySim));
let running = b.build()?.run()?;

// client: the generated stub
let mut c = ble::BleServiceClient::connect(sock)?;
let mut stream = c.scan(ble::ScanRequest::default())?;   // server-streaming
while let Some(dev) = stream.message()? { /* ... */ }
```

Requirements: the consumer enables `grpcuds`'s `prost` feature (plus `server`
for the trait half / `client` for the stub half), **depends on `prost`
directly** (the generated messages derive `::prost::Message`, exactly as
with tonic), and has `protoc` on `PATH` (or `PROTOC`) — prost-build invokes
it. Client-streaming and bidirectional RPCs are rejected at build time (the
runtime is unary + server-streaming).
A complete worked example lives in
[`tests/rust/domains/ble-domain`](https://github.com/sy39ju/grpc-uds/tree/main/tests/rust/domains/ble-domain).

License: MIT OR Apache-2.0.
