<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
# tests/rust — the interop matrix (Rust)

A separate cargo workspace (its own relaxed profiles) holding the **3 domains ×
3 transport combos** matrix. `tonic` plays the stock-gRPC peer.

| domain ↓ \ combo → | `gg` grpcuds⇄grpcuds | `gt` grpcuds srv + tonic cli | `tg` tonic srv + grpcuds cli |
|--------------------|:--------------------:|:---------------------------:|:----------------------------:|
| **BLE**            | `cells/ble-gg`       | `cells/ble-gt`              | `cells/ble-tg`               |
| **AI agent**       | `cells/agent-gg`     | `cells/agent-gt`            | `cells/agent-tg`             |
| **X.509**          | `cells/x509-gg`      | `cells/x509-gt`             | `cells/x509-tg`              |

Each cell is a thin crate: a runnable `src/main.rs` demo + a `tests/e2e.rs`. The
real domain logic lives once in `domains/{ble,agent,x509}-domain` (a grpcuds
server impl + a tonic server impl + fixtures), reused by all three of its cells.

```bash
cargo test --workspace          # all 9 cells + the scripted agent loop
cargo run -p ble-gg             # run a single cell's demo
```

## Footprint: grpcuds vs tonic (Rust)

Same role + service logic — only the transport differs. Only the domains whose
implementations run **identical** logic qualify (BLE and the agent echo runtime);
X.509 is excluded (real cert agent vs mock). Measured by
`../bench/measure_tables.sh` (binaries build in `sizebench/`), `opt-level="z"` +
fat LTO + strip.

grpcuds is **~3× smaller on disk and ~2–2.5× smaller in PSS** than tonic: both
pay the static `std` floor, but grpcuds links system `libnghttp2` dynamically
and adds only its small core, while tonic statically links the
`tonic`/`tokio`/`hyper`/`h2` stack. The **C++** build drops the `std` floor
entirely and lands two orders of magnitude below a stock grpc++ server.

The full per-version size + PSS tables (Rust-vs-tonic and C++-vs-grpc++) live in
one place: **[`docs/FOOTPRINT.md`](../../docs/FOOTPRINT.md)**. Reproduce with
`../bench/measure_tables.sh` (it prints all columns — the C++ / grpc++ ones show
`—` until the `tests/cpp` binaries exist; build them **Release**, since a Debug
build silently inflates the numbers).

## AI agent + local ollama

The agent cells drive a deterministic echo runtime in their tests. To run the
**real** `Assistant` model↔tools loop against a local [ollama](https://ollama.com):

```bash
OLLAMA_HOST=localhost:11434 cargo run -p agent-gt   # grpcuds server + tonic client
OLLAMA_HOST=localhost:11434 cargo run -p agent-tg   # tonic server + grpcuds client
```

Offline (no `OLLAMA_HOST`/`ANTHROPIC_API_KEY`) they fall back to the scripted
backend. The live path is also an `#[ignore]`d test per cell.

## Cross-language (`cross/`)

`cross/` drives the **C++** example binaries (`tests/cpp/`): a Rust tonic peer
in-process either drives a C++ grpcuds server (`gt`) or is driven by a C++
grpcuds client (`tg`). Build the C++ side first, then:

```bash
cargo test -p cross   # skips (not fails) any C++ binary that isn't built
```

Binary locations are taken from `$<DOMAIN>_GT_SERVER_BIN` / `$<DOMAIN>_TG_CLIENT_BIN`
(falling back to `tests/cpp/build/...`).
