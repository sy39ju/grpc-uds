# grpcuds

A lightweight server transport library that is **wire-compatible with stock gRPC
clients** over UDS. An embedded server exposes domain operations (canonical
example: BLE scan/GATT) to gRPC clients via server-streaming. The client is
stock gRPC (cannot be changed) → the server must speak HTTP/2 + the gRPC wire.

> See **DESIGN.md** for the detailed design and decision rationale,
> **docs/BUILDING.md** for native/cross builds, and **tests/bench/** for the
> measured tonic comparison + memcheck harness.

## Invariants (do not change)
- **No own HTTP/2 implementation** → **dynamically link** the system
  `libnghttp2` (confirmed present on armv7).
- **Messages via nanopb**. protobuf-full is forbidden (size). The core does not
  know about message serialization — it only handles framed bytes + path +
  streams. nanopb encode/decode lives in the C stub layer.
- **Domain logic (BLE etc.) is C (host Native API)**. The core does not know the
  domain (generic transport).
- The main loop is a **single-threaded event loop**. The UDS fd is watched by the
  host event loop (epoll/libevent).
- The transport has **no security** (local IPC, same device).

## Size budget
- Current app 100KB → after adding this feature, **within 200KB** (core addition
  ≤100KB, target ~40~60KB).
- Dynamic linking is assumed. Do not go for a static bundle (+100~180KB).

## Coding rules
- `#![no_std]` + `extern crate alloc`, system malloc as the global allocator.
- `panic = "abort"`. **Therefore `catch_unwind` is unavailable → the core must be
  panic-free**: route every error path through `Result`. No
  `unwrap`/`expect`/indexing panics.
- **Do not use `core::fmt`** (a size killer). Delegate logging to a C callback.
- `unsafe` is confined to `grpcuds-sys` (the FFI boundary). `grpcuds-core` is
  safe-first.
- Avoid nightly features (build-std etc.) — the goal is to hit the budget with
  stable no_std.

## Crate structure
- `grpcuds-sys`      : raw nghttp2 FFI generated directly with bindgen (the
  existing `-sys` crates are abandoned, do not use them).
- `grpcuds-core`     : gRPC framing + UDS + stream state machine. no_std.
- `grpcuds-ffi-impl` : C ABI symbols as a testable rlib.
- `grpcuds-ffi`      : staticlib/cdylib shell + `grpcuds.h`. Symbol prefix
  `grpcuds_`. `publish = false` (ships as compiled artifacts).
- `grpcuds`          : safe Rust server API over the core (`prost` typed
  handlers, `tokio` serve_async).
- `protoc-gen-grpcudspp` : protoc plugin emitting the C++ service stubs
  (default) or plain-C stubs (`--grpcudspp_opt=c`).
- `grpcuds-build`    : build.rs service codegen for Rust (prost) — server
  trait + typed `*Client` stub (`build_server`/`build_client` toggles).

Consumer-facing example code and test infrastructure are SEPARATE trees:
- `example/ble/`    : THE example — a COMPLETE grpcuds⇄grpcuds BLE service
  in two C++ files (simulated radio; streaming producer thread). No adapter
  patterns, no shared libs — readable top to bottom. Dual-layout CMake: it
  also ships verbatim at the SDK bundle root.
- `example/c/`      : the plain-C example — echo server+client straight on
  the C ABI (grpcuds.h + nanopb only, no C++, no service codegen); doubles
  as `example-c/` in the SDK bundle. docs/C_API_GUIDE.md is its guide.
- `example/nanopb`  : the nanopb runtime+generator submodule (C/C++ codegen
  only — the Rust side uses prost). Product input: SDK packaging and every
  C++ codegen step point here.
- `tests/`          : the 3×3 interop matrix — 3 domains (BLE, AI agent,
  X.509) × 3 transport combos (gg grpcuds⇄grpcuds, gt
  grpcuds-server+tonic-client, tg tonic-server+grpcuds-client). Test
  infrastructure, not consumer material.
  - `tests/rust/` : its OWN cargo workspace (relaxed profiles).
    `domains/*-domain` hold shared grpcuds+tonic impls; `cells/*` are the 9
    thin cells; `cross/` drives the C++ binaries; `sizebench/` measures
    footprint. Test protos: `tests/rust/proto/` (`.proto` only).
  - `tests/cpp/`  : the 9 C++ cells + optional stock-grpc++ peer cells
    (gated on `find_package(gRPC)`). `tests/cpp/proto/` adds the nanopb
    `.options` + the C++-only `agent_cpp.proto`; a `cross` test guards the
    two proto copies against drift.

## Build / measure
```bash
./build.sh                       # native: runtime lib + host plugin + .pc
./build.sh examples              # one-shot: lib+plugin, root cmake+ctest, rust, cross
./build.sh package           # dist/grpcuds-<ver>-cpp/{example,example-c,target,host,sdk}
                                 # + .tar.gz (Rust ships via crates.io only)
SYSROOT=<cross-sysroot> ./build.sh --target armv7-unknown-linux-gnueabihf
cd tests/bench && cargo build --release && ./target/release/runner   # perf/size table
cd tests/bench && ./memcheck.sh              # valgrind sweep (also in CI, main only)
cd tests/bench && ./helgrind.sh              # mailbox race detector (CI, main only)
cd tests/rust && cargo test --workspace                     # 9 Rust cells
git submodule update --init example/nanopb                # once (C++ codegen)
cmake -S . -B build && cmake --build build && ctest --test-dir build
  # root = cpp/ wrapper tests + tests/cpp matrix + example/ble + example/c
tests/bench/measure_tables.sh            # grpcuds vs grpc++/tonic footprint
tests/bench/measure_footprint.sh          # probes/std-floor/heap (README sizes)
```
**All measured size/memory numbers live in ONE place — `docs/FOOTPRINT.md`,
recorded per version (newest first). Every other doc links there instead of
quoting figures. After a re-measurement, prepend a new `## <ver> — <date>`
section; do not scatter numbers back into README/DESIGN/etc.**

## Current stage
Implemented end to end (core, C ABI, C++ wrapper, plugin, safe Rust API, the
3×3 interop matrix in Rust + C++ incl. stock-grpc++ interop, bench/memcheck, CI
gates incl. fmt/clippy/MSRV). Remaining before publish (user-owned): armv7 build
+ size measurement on a real sysroot, public GitHub repo + crates.io publish
(`scripts/release.sh`).
