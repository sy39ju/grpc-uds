# Changelog

All notable changes to the grpcuds crates are documented here. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions
follow [SemVer](https://semver.org). The workspace crates
(`grpcuds-sys`, `grpcuds-core`, `grpcuds`, `grpcuds-build`,
`protoc-gen-grpcudspp`) version together. MSRV: 1.85.

## [Unreleased]

- **Plain C is an official consumer surface.** The C ABI was always the
  primary boundary; now it has the full kit: `example/c/` (a complete
  server + client in two C11 files — caller-owned poll loop, nanopb glue,
  log sink; ships as `example-c/` in the SDK bundle and runs in ctest),
  `docs/C_API_GUIDE.md`, and **optional C service codegen** — the same
  protoc plugin with `--grpcudspp_opt=c` emits `<base>.grpcuds.{h,c}`: a
  handler table + `<Svc>_register()` with generated nanopb trampolines
  (deferred unary via `GRPCUDS_HANDLER_DEFERRED` + `_respond()`,
  streaming `_send`/`_finish`) and typed client wrappers. Client-side
  code is static inline in the header, so half-builds never see undefined
  symbols. New `grpcuds.h` support types: `grpcuds_call_ref`,
  `GRPCUDS_HANDLER_DEFERRED`, `GRPCUDS_ERR_TRANSPORT/CODEC`. Proven by a
  self-checking C cell (`ble-cgen-c`) covering the deferred path, sync
  OK/error, streaming, and NULL-entry → UNIMPLEMENTED.

- **Log facility — severity log with a host-owned sink.** The gpr_log
  shape, sized for the no_std core: the library NEVER formats — every
  event is a static message plus one numeric argument (errno, call id,
  queue capacity) — and is fully silent until a sink is registered.
  `grpcuds_set_log_callback` (C ABI, always present),
  `grpcuds::SetLogCallback` / `EnableStderrLogging` (C++, header-only
  stderr sink included), `grpcuds::set_logger` (Rust closure). ~20 call
  sites cover listener/accept/IO failures, nghttp2 errors, unimplemented
  methods, deadline expiry on both sides, peer cancellation,
  backpressure drops, and connect/reconnect lifecycle. Cost when unused:
  a null-check per site, ~1 KB of static strings/branches per half
  (server contribution 19,186 → 20,307 B; client 14,419 → 15,305 B —
  README sizes re-measured).

- **Dev-only Wireshark wire logging (`wirelog` feature).** Off by default
  with zero footprint; `./build.sh --wirelog` (or the cargo feature)
  compiles it in, and `GRPCUDS_WIRELOG=<path>.pcap` activates it at
  runtime. Every byte crossing a grpcuds socket — server side and client
  side — is appended to a Wireshark-readable pcap: UDS traffic carries no
  TCP/IP framing, so chunks are wrapped in synthetic IPv4+TCP (fabricated
  handshake, consistent seq/ack, one fake stream per connection, server
  port 80 so Wireshark's default HTTP dissector auto-detects the HTTP/2
  preface and dissects HTTP/2 → gRPC with no Decode-As step). Files rotate at 1 MiB into `.1`/`.2` — at most 3 files / 3 MiB
  on disk including the live one; both knobs are environment-tunable
  (`GRPCUDS_WIRELOG_FILE_KB`, `GRPCUDS_WIRELOG_FILES`). CI proves byte-exactness by reassembling
  the capture and walking it as HTTP/2 frames.

- **Connection lifecycle: connect-with-retry + lazy reconnect.** The
  channel machinery of stock gRPC mostly dissolves on a single UDS path,
  but its useful slice is now built in. `connect_wait` (Rust core + safe
  API, `grpcuds_client_connect_wait` C ABI, `grpcuds::Client(path,
  timeout_ms)` C++) retries connect with the stock-gRPC backoff shape
  rescaled for local IPC — 50 ms × 1.6 up to a 1 s cap, ±20% jitter, each
  attempt itself bounded at 250 ms (a blocking UDS connect can otherwise
  wait indefinitely on a full listener backlog) — until the deadline,
  covering the daemon-startup race; 0 means exactly one attempt. And every client now
  lazily reconnects: after a server restart the call that hit the dead
  connection fails, the next call makes one fresh connect to the same
  path — stock-gRPC IDLE-channel style, same client object throughout.

- **SIGPIPE can no longer kill C host processes.** All three socket-write
  sites (client `write_all`, server `write_some`, the NO_COPY `writev`
  fast path) now use `send`/`sendmsg` with `MSG_NOSIGNAL` instead of raw
  `write`/`writev`. Writing to a peer that died raises SIGPIPE under the
  old calls — fatal by default in C/C++ hosts, which have no Rust runtime
  ignoring it; the error now surfaces as `EPIPE` and feeds the reconnect
  path instead.

- **Binary hardening by default.** Every C++ project in the repo (the
  example, the wrapper tests, the interop matrix) now builds with PIE,
  `-fstack-protector-strong` (when available), full RELRO and
  `-z noexecstack` (`GRPCUDS_HARDENING=OFF` opts out) — explicit because
  cross toolchains often do not default what distro compilers do. The
  example also defaults to `MinSizeRel` when no build type is chosen, and
  the C++ health service swapped `std::map` machinery for flat vectors —
  together taking the BLE example server from an unoptimized 198 KB to
  ~86 KB stripped, smaller than before health existed.

- **Standard health checking (`health` feature).** `grpcuds::health` ships
  the `grpc.health.v1` service: `add_health_service(builder, &reporter)`
  registers `Check` (unary; unknown services fail `NOT_FOUND` per the
  protocol) and `Watch` (server-streaming; immediate status, then every
  change), published through a thread-safe `HealthReporter`. The message
  types are prost-derived in-crate — no protoc, no build script. Stock
  conformance is CI-tested with a tonic-health client over UDS. The C++
  twin is header-only `<grpcudspp/health.h>` (the two protocol messages are
  hand-coded — one string field, one varint — so no nanopb/codegen run);
  the BLE example registers it and its client health-checks before driving
  the service. Hardening that surfaced: the C++ outbound mailbox now keeps
  a tombstone registry — hosts unregister a connection's call handle
  (`grpcuds_conn_call_handle`) before freeing it, so producer writes queued
  across that moment are dropped instead of dereferencing a freed
  connection (the bundled loops do this automatically; custom loops keep
  the historical contract unless they opt in).

- **Deadlines, both sides.** Clients gain a per-call timeout knob —
  `grpcuds_client_set_timeout_ms` (C ABI), `Client::SetTimeout` (C++),
  `Client::set_timeout` (Rust, also on the generated stubs) — covering the
  whole call (gRPC deadline semantics): on expiry the call fails locally
  with `DEADLINE_EXCEEDED` and the stream is cancelled. An armed timeout is
  also SENT as `grpc-timeout`, and the server now honors that header (from
  any conforming client, tonic/grpc++ included): a dispatched call whose
  deadline passes is finished with `DEADLINE_EXCEEDED` and its cancel hook
  fires, so deferred work stops. Expiry is checked on every connection
  tick; hosts with their own poll loop bound the timeout with
  `grpcuds_conn_next_deadline_ms` so idle connections expire too (the
  bundled loops — safe Rust server, `ServerThread`, the test poll loop —
  already do). Handlers can read the remaining budget — stock gRPC's
  "context deadline" — via `grpcuds_call_time_remaining_ms` (C ABI),
  `ServerContext::TimeRemainingMs` (C++), or
  `ServerWriter::time_remaining` / `MessageWriter::time_remaining` (Rust).
  Defaults unchanged: no timeout set, no deadline.

- **Deferred unary completion (C++).** Every generated unary method gains a
  second virtual overload taking `grpcuds::UnaryResponder<Resp>` — the
  grpc++ callback-API `ServerUnaryReactor` shape. Override it to return from
  the handler immediately and complete the call later from any thread
  (`Respond(reply)` / `Fail(status)`, thread-safe, single-use). The default
  implementation delegates to the synchronous handler, so existing services
  compile and behave unchanged. The unary trampoline now routes through the
  responder; interop is covered in CI against grpcuds, tonic, and stock
  grpc++ clients.

- **Examples vs tests, separated.** The interop matrix is test
  infrastructure and now lives under `tests/` — `tests/rust/` (a separate
  workspace: 3 shared domain libs + 9 cells + a cross-language test crate)
  and `tests/cpp/` (9 cells), covering each domain (BLE, AI agent, X.509)
  across grpcuds⇄grpcuds, grpcuds-server+tonic-client, and
  tonic-server+grpcuds-client. The consumer-facing showcase is new:
  `example/ble/` — a complete grpcuds⇄grpcuds BLE service in two C++ files
  (simulated radio, streaming producer thread, GATT read, error path). It doubles as the
  SDK-bundle example (dual-layout CMake), replacing the echo starter. The
  `nanopb` submodule lives at `example/nanopb`. No library/API changes.

## [0.1.0]

Initial release.

- **`grpcuds-core`** — `no_std` gRPC-over-HTTP/2 server transport for UNIX
  domain sockets, wire-compatible with stock gRPC clients. Single-threaded,
  caller-driven event loop; unary + server-streaming; `grpc-status` /
  `grpc-message` trailers; per-stream backpressure (`Reject` /
  `DropOldest`); cancel hooks on peer RST_STREAM; flat memory on long-lived
  connections (per-call state dropped at stream close, nghttp2
  closed-stream retention disabled); zero-copy owned write path with
  `NO_COPY` direct send for DATA frames ≥ 4 KB. HTTP/2 framing is the
  system `libnghttp2` (dynamic by default, `bundled` feature for a static
  build from the pinned submodule).
- **`grpcuds-sys`** — bindgen FFI to `libnghttp2` with vendored headers for
  self-contained host builds.
- **`protoc-gen-grpcudspp`** — protoc plugin emitting gRPC-C++-shaped C++
  service stubs over nanopb messages.
- **`grpcuds-build`** — `build.rs` service codegen (tonic-build's shape):
  prost messages plus a generated server trait + `add_*_service`
  registration glue and a typed `*Client` stub (`build_server` /
  `build_client` toggles); client/bidi streaming rejected at build time.
  Worked example: `tests/rust/domains/ble-domain`.
- **`grpcuds`** — safe Rust server API on the same core. Feature-gated into
  `server` (default) and `client`: a default build is server-only and
  unchanged. The new `client` feature adds a blocking `Client`
  (`connect` -> `unary` / `server_streaming`, plus `unary_msg` /
  `server_streaming_msg` under `prost`) speaking the same wire to any stock
  gRPC server over UDS. `Status` / `GrpcStatus` now derive `Debug`/`Eq`.
  Server API as before:
  `serve_async(shutdown_future)` under the `tokio` feature (I/O loop on the
  blocking pool, graceful-shutdown future);
  `Server::builder().bind(..).add_unary(..).build()?.run()?` with a RAII
  `Running` handle; `Send + Sync + Clone` `ServerWriter` whose
  `write`/`finish` return `Err(Closed)` once the client is gone; per-call
  backpressure; canonical `Status` constructors; optional typed prost
  handlers (`prost` feature) with `MessageWriter<T>`. `grpcuds-core` is an
  internal substrate (`#[doc(hidden)]`); the stable vocabulary types are
  re-exported here.
- **C ABI + C++ client.** `grpcuds-core`, `grpcuds-ffi-impl`, and
  `grpcuds-ffi` split into `server` (default) and `client` Cargo features:
  building server-only is unchanged, and a client-only build drops every
  server symbol (and vice versa) so a C embedder links just the side it
  uses. The `client` feature adds a no_std blocking client (`ClientConn`),
  the `grpcuds_client_*` / `grpcuds_response_*` / `grpcuds_stream_*` C ABI,
  and a header-only `grpcudspp::Client` (byte-level + optional nanopb-typed).
- Not published to crates.io (ship as compiled artifacts / source):
  `grpcuds-ffi` (C ABI staticlib/cdylib + `grpcuds.h`) and the header-only
  C++ wrapper (`cpp/include/grpcudspp`).

[Unreleased]: https://github.com/sy39ju/grpc-uds/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/sy39ju/grpc-uds/releases/tag/v0.1.0
