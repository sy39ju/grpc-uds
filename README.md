# grpcuds

[![CI](https://github.com/sy39ju/grpc-uds/actions/workflows/ci.yml/badge.svg)](https://github.com/sy39ju/grpc-uds/actions/workflows/ci.yml)
[![Built with Claude Code](https://img.shields.io/badge/built%20with%20Claude%20Code-Claude%20Fable%205-D97757?logo=anthropic&logoColor=white)](https://claude.com/claude-code)

Wire-compatible gRPC **server and client** over UNIX domain sockets for
embedded Linux. A single-threaded `no_std` Rust core speaks real
gRPC-over-HTTP/2, so **stock gRPC peers interoperate unchanged** — only the
transport is a UDS. Both sides fit an embedded budget — tens of KB each, not
the megabytes a full gRPC stack pulls in — and you link only the side you use
(measured figures: [Footprint & memory](docs/FOOTPRINT.md)).

## Quick start

`./build.sh` is the build driver (`--help` for everything):

```sh
# 1. The artifacts: libgrpcuds_ffi.{a,so} + the protoc plugin + grpcuds.pc
./build.sh                       # --side server|client|both, --target, --bundled

# 2. Build + run everything, natively: the C++ wrapper tests, the BLE
#    showcase (example/ble), the plain-C example (example/c), and the
#    9+9 interop test matrix — one command
git submodule update --init example/nanopb   # once — C++ codegen
sudo apt install python3-protobuf            # nanopb generator dep (PEP-668
                                             # distros block bare pip3; a venv works too)
./build.sh examples

# 3. A distributable tree to drop into a new C/C++ project
./build.sh package           # Rust consumers use crates.io instead
```

| Per-piece command | Builds |
| --- | --- |
| `./build.sh lib` | the C ABI runtime (`--bundled` = static nghttp2 for bare sysroots) |
| `./build.sh plugin` | `protoc-gen-grpcudspp`, the host codegen tool |
| `./build.sh rlib` | the safe Rust API crate |
| `cmake -S . -B build` | just the C++ side |

C++ messages over 1 KB need a bigger trampoline scratch buffer —
`-DGRPCUDSPP_MAX_MESSAGE_SIZE=4096`
([migration guide](docs/MIGRATING_FROM_GRPC_CPP.md), "Variable-size fields").

## Dependencies

It stays small by delegating instead of bundling:

- HTTP/2 → the **system `libnghttp2`**, dynamically linked (opt-in static
  bundle via `--features bundled`).
- Messages → **nanopb** flat C structs, not protobuf-full.
- The core moves opaque framed bytes; it knows nothing about your messages
  or domain.
- It runs on *your* event loop (or an opt-in background thread); it owns no
  threads by default.

| Dependency | Role | Stage |
| ---------- | ---- | ----- |
| `libnghttp2` | HTTP/2 framing | runtime, dynamic (or `--features bundled`) |
| nanopb (pinned submodule) | message codegen + runtime source | C/C++ consumer builds only — the library itself links no nanopb |
| `protoc` + `libclang` | stub codegen + bindgen | build-time |
| `libc` crate | allocator + UDS syscalls | core |

## Features

**vs production gRPC** — what a stock-gRPC developer gives up:

| Capability | production gRPC | grpcuds |
| ---------- | --------------- | ------- |
| Wire compat (HTTP/2 + gRPC framing + trailers) | ✓ | ✓ — stock clients unmodified |
| Unary / server-streaming RPCs | ✓ | ✓ |
| `grpc-status` + `grpc-message` trailers | ✓ | ✓ |
| Client cancellation (RST_STREAM) | ✓ | ✓ — cancel hooks / writer feedback |
| Per-stream outbound backpressure | ✓ | ✓ — `DropOldest` / `Reject` |
| Client API | ✓ | ✓ — blocking `Client` + generated typed stubs (`NewStub` / `*Client`), behind the `client` feature (server-only by default) |
| Concurrent connections | ✓ | ✓ — multiplexed on the single I/O thread, ~15–18 KB heap each |
| Client / bidirectional streaming | ✓ | ✗ — rejected at codegen |
| TCP / multi-transport | ✓ | ✗ — UDS only |
| TLS / call credentials | ✓ | ✗ — filesystem permissions are the boundary |
| Deadlines (`grpc-timeout`) | ✓ | ✓ — clients arm per-call timeouts (`SetTimeout` / `set_timeout`) and send `grpc-timeout`; the server honors the header (expires the call with `DEADLINE_EXCEEDED`, fires the cancel hook) |
| Custom metadata | ✓ | ✗ — fixed header set |
| Compression | ✓ | ✗ — `identity` only |
| Health checking (`grpc.health.v1`) | ✓ | ✓ — opt-in: Rust `health` feature / C++ `<grpcudspp/health.h>`; `Check` + `Watch`, stock probers work |
| Reflection | ✓ | ✗ — its RPC is a bidirectional stream (out of scope) |
| Interceptors / middleware | ✓ | ✗ |
| Load balancing / channels | ✓ | ✗ — no channels/LB (one socket path); the useful slice survives: connect-with-retry (`connect_wait`) + lazy reconnect after a server restart |
| Proto coverage | full | C++: proto3 subset via nanopb (no Any / well-known types); Rust: prost |
| Threading | thread pools | single I/O thread + mailbox producers |

If any ✗ row is load-bearing for you, use production gRPC. grpcuds exists
for the case where none of them are — an embedded device exposing a local
service to a handful of same-device stock-gRPC peers. The API is `grpcpp/`-shaped
(`ServerBuilder` / `Service` / `ServerWriter<T>` / `Status`) with codegen for
both C++ (`protoc-gen-grpcudspp` over nanopb) and Rust (`grpcuds-build` over
prost), behind a stable C ABI (`grpcuds.h`) + a header-only C++ wrapper; it
runs on *your* event loop, or an opt-in `ServerThread`. **Plain C is fully
supported** — the C ABI is the primary boundary and nanopb messages are
pure C; see `example/c/` and [docs/C_API_GUIDE.md](docs/C_API_GUIDE.md)
(only the generated service classes are C++-specific).

## Caveats

Honest consequences of the design choices, beyond the feature table:

- **Not an async-native stack.** The core is a single-threaded poll loop.
  Tokio integration is first-class but *coexistence*, not fusion:
  `serve_async` (the `tokio` feature) parks the I/O loop on one
  blocking-pool thread with future-driven shutdown, and the blocking
  `Client` belongs inside `spawn_blocking`. There are no `async fn`
  handlers and the UDS fd never registers with tokio's reactor. If you
  want a zero-extra-threads async server, that's tonic's job.
- **Handlers must not block.** They run on the I/O thread and return
  immediately; a blocking handler stalls *every* connection. Streaming is
  producer-push (`writer.Write(...)` / `Finish(status)` from another
  context) and long-running unary work defers the same way
  (`UnaryResponder`). Porting grpc++ code whose handlers loop-and-block
  requires restructuring ([migration guide](docs/MIGRATING_FROM_GRPC_CPP.md)).
- **nanopb means fixed capacities (C++).** Message fields carry
  compile-time caps from `.options`; oversized strings/arrays are not
  dynamically grown. Choosing the caps is part of designing your `.proto`.
- **The C ABI header is hand-maintained.** `grpcuds.h` is the contract and
  `grpcuds-ffi` implements it by discipline — agreement is enforced by
  link-time symbol resolution and the CI integration matrix, not by a
  generator (no cbindgen; the header doubles as the curated C API doc).
- **Sized for local IPC.** One thread serializes all connection I/O —
  designed for a handful of same-device peers, not for fan-out or
  throughput competition with a threaded stack. The bench tables are the
  envelope; outside it, use production gRPC.

## Security

This transport provides **no built-in payload security — by design** (the
Features table's ✗ rows): no TLS, no call credentials, no transport-level
peer identity, and no message authentication. It is intended for local IPC
between cooperating processes over Unix domain sockets.

Access control is delegated to the operating system. Use UDS socket
permissions, parent-directory permissions, ownership and groups, and
platform MAC policies (SELinux, AppArmor, Smack) where needed. grpcuds
binds pathname sockets only — abstract-namespace sockets, which bypass
filesystem permissions entirely, are not supported. The server creates
the socket file at bind time (replacing any existing file at that path)
honoring the process umask, and does not chmod it: set a restrictive
umask before binding (race-free — the file is born restricted), or place
the socket in a permission-locked directory, and use a trusted path such
as `/run/<svc>/`.

OS-level controls have limits: they do not distinguish mutually untrusted
processes running under the same UID, and adding application-level
encryption does not authorize peers either. If stronger isolation or
confidentiality is required, enforce it at the platform level or inside
the application payload.

**Resource limits are the caller's responsibility.** The transport bounds
per-message size (16 MiB, a session-fatal cap) but applies *no* limit on
the number of concurrent connections, and the per-stream outbound queue is
**unbounded by default**. A hostile — or buggy — local peer can therefore
exhaust memory three ways: opening connections without end, calling a
server-streaming method and never reading while the producer writes, or
opening a request stream that never completes and carries no `grpc-timeout`
(such a stream is not reaped server-side). Mitigate in your integration:
cap accepted connections in your event loop, set
`Backpressure::Bounded` / `SetBackpressure` on streaming methods, and have
clients arm deadlines. This is the trade for a single-threaded core sized
for cooperating local peers, not adversarial load.

The bundled CMake projects (C and C++) enable binary hardening by default
(`GRPCUDS_HARDENING=OFF` to opt out): PIE, stack protector where
available, full RELRO/BIND_NOW, and a non-executable stack. See
`SECURITY.md` for vulnerability reporting.

## Size & memory

`panic="abort"` + `no_std`, after link + strip, `libnghttp2` linked
dynamically: the server adds tens of KB to a host C/C++ app, the client a
little less, and the C++ wrapper is header-only (0 bytes). Standalone the C++
example binaries are well under 100 KB; the Rust ones carry the static `std`
floor on top of the same small transport.

All exact figures — C-embed contributions, standalone binaries, heap/RSS/PSS,
and the apple-to-apple tables vs grpc++ and tonic — live in one place and are
kept per version: **[Footprint & memory](docs/FOOTPRINT.md)**. Reproduce with
`tests/bench/measure_footprint.sh` + `tests/bench/measure_tables.sh`.

## Authorship

This library was written with [Claude Code](https://claude.com/claude-code),
powered by Anthropic's **Claude Fable 5** (`claude-fable-5`) — design,
implementation, tests, benchmarks, and these docs grew out of a long
human–AI pair-programming session. A human reviewed and owns every line.

## License

Dual-licensed under **MIT OR Apache-2.0**, at your option — see
[`LICENSE-MIT`](LICENSE-MIT) / [`LICENSE-APACHE`](LICENSE-APACHE). Unless
you state otherwise, any contribution you submit is dual-licensed the same
way. The nghttp2 source pinned for the opt-in `bundled` feature is MIT
(its `COPYING` ships alongside); by default `libnghttp2` is dynamically
linked, not bundled. Attributions:
[`THIRD-PARTY-NOTICES`](THIRD-PARTY-NOTICES).
