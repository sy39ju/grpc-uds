<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
# tests/cpp — the interop matrix (C++)

The **3 domains × 3 transport combos** matrix in C++. The stock-gRPC peer is
`tonic` (Rust) for the cross-language tests, and — when it's installed —
**grpc++** (C++) for a same-language comparison (see below). The C++ side uses
deterministic mocks (the point is the *wire*, not the domain).

| domain ↓ \ combo → | `gg` grpcuds⇄grpcuds | `gt` grpcuds srv (+ rust tonic cli) | `tg` grpcuds cli (+ rust tonic srv) |
|--------------------|:--------------------:|:-----------------------------------:|:-----------------------------------:|
| **BLE**            | `ble/gg` (ctest)     | `ble/gt` server bin                 | `ble/tg` client bin                 |
| **AI agent**       | `agent/gg` (ctest)   | `agent/gt` server bin               | `agent/tg` client bin               |
| **X.509**          | `x509/gg` (ctest)    | `x509/gt` server bin                | `x509/tg` client bin                |

- **gg** binaries run server + client in one process and self-check (`ctest`).
- **gt** binaries are grpcuds servers; **tg** binaries are self-checking grpcuds
  clients. Both are driven by the Rust `cross` tests (`tests/rust/cross/`),
  which supply the tonic peer.

### Same-language stock peer: grpc++

When **grpc++** is installed (`find_package(gRPC)` succeeds), each domain also
builds a stock **grpc++** server + client (`<domain>/grpcpp/`, protobuf-full)
and two `ctest` interop cells that prove grpcuds is wire-compatible with stock
gRPC C++ — both directions:

- `<domain>-grpcuds-server-grpcpp-client` — grpcuds C++ server ⇄ grpc++ client
- `<domain>-grpcpp-server-grpcuds-client` — grpc++ server ⇄ grpcuds C++ client

(BLE and agent; the agent grpc++ peer uses the full `agent.proto` since
protobuf-full handles the oneof.) Without grpc++ these cells are skipped and the
rest of the matrix builds unchanged.

The C++ agent serves only the `Agent` service (the `Assistant` loop's
`AgentEvent` oneof doesn't render as nanopb plain structs), via the wire-identical
`proto/agent_cpp.proto`.

## Footprint: grpcuds C++ vs stock grpc++

The same-language, apple-to-apple comparison — a grpcuds C++ server/client vs a
stock grpc++ one doing the identical job (BLE and the agent echo runtime; X.509
excluded as real cert agent vs mock) — is measured here by
`tests/bench/measure_tables.sh` (Release + stripped; grpc++ statically linked
for a self-contained comparison). The bottom line: grpc++'s
grpc+protobuf+abseil stack is multi-megabyte while the grpcuds C++ binary doing
the same job is well under 100 KB and idles in hundreds of KB — roughly two
orders of magnitude smaller on disk and ~20× in memory.

The full size + idle-PSS tables (and the Rust grpcuds-vs-tonic comparison) live
in one place, kept per version: **[`docs/FOOTPRINT.md`](../../docs/FOOTPRINT.md)**.
Reproduce with `tests/bench/measure_tables.sh` (build `tests/cpp` Release first;
the grpc++ columns need grpc++ installed).

## Build + run

```bash
# prerequisites (from the repo root)
cd rust && cargo build --release -p grpcuds-ffi --features client -p protoc-gen-grpcudspp
git submodule update --init example/nanopb
cd ..

cmake -S tests/cpp -B tests/cpp/build   # auto-finds example/nanopb
cmake --build tests/cpp/build
ctest --test-dir tests/cpp/build          # the 3 gg cells

# the gt/tg cells (cross-language) are run from the Rust side:
cd tests/rust && cargo test -p cross
```

Codegen (nanopb messages + grpcudspp service stubs) is wired once in
`cmake/grpcuds_codegen.cmake`; the shared server poll loop is
`common/poll_loop.h`.
