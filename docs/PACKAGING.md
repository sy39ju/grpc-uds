<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
# Packaging & release artifacts

This repo produces two very different kinds of output, and the boundary between
them has been fuzzy. This document draws the line:

1. **grpcuds (the transport library)** — what *we* build and ship.
2. **the server implementation** — what a *consumer* builds on top of us.

The rule of thumb: **`libgrpcuds_ffi` moves opaque bytes.** Anything that knows
about your `.proto` (nanopb structs, the service stubs, message codegen) is the
consumer's server build, not ours. See [`README.md`](../README.md) and
[`docs/BUILDING.md`](BUILDING.md) for how each piece is built.

## 0. Artifact inventory

| Artifact | Produced by | Arch | Kind |
| -------- | ----------- | ---- | ---- |
| `libgrpcuds_ffi.a` | `cargo build -p grpcuds-ffi` | **target** | runtime, prebuilt |
| `libgrpcuds_ffi.so` | `cargo build -p grpcuds-ffi` | target | runtime, prebuilt (optional) |
| `grpcuds.h` | source (`rust/grpcuds-ffi/include/`) | any | C ABI header |
| `grpcudspp/*.h` | source (`cpp/include/`) | any | header-only C++ wrapper |
| `grpcuds.pc` | `scripts/gen-pkgconfig.sh` | target | pkg-config |
| `protoc-gen-grpcudspp` | `cargo build -p protoc-gen-grpcudspp` | **host** | codegen plugin |
| `nanopb_generator.py` | `nanopb/generator/` | **host** | codegen tool |
| `pb_*.c` / `pb.h` | `nanopb/` | (compiled on target) | nanopb runtime **source** |
| `proto/*.proto` + `*.options` | source | any | contract example |
| `docs/`, `tests/rust/`, `tests/cpp/` | source | any | guides / reference servers |
| crates `grpcuds-sys`, `grpcuds-core`, `grpcuds`, `grpcuds-build`, `protoc-gen-grpcudspp` | `cargo publish` | host | crates.io channel |

`grpcuds-ffi` / `grpcuds-ffi-impl` are `publish = false` (a staticlib/cdylib is
not consumable via cargo); their compiled `.a`/`.so` ship through the bundle
below, not crates.io. **crates.io is the only Rust channel** — there is
deliberately no Rust source bundle, so the crates have a single source of
truth.

## 1. Release artifact buckets

Three buckets, by *where they run*:

### a. Target-install (deploy/link on the device, e.g. armv7)
The only prebuilt binary that lands on the target, plus what's needed to link it.
- `lib/libgrpcuds_ffi.a` (target arch) — and optionally `.so`
- `lib/pkgconfig/grpcuds.pc` (target paths)
- system `libnghttp2.so` — **already on the device**, dynamically linked, not shipped

### b. Server-integration (arch-independent — what a developer builds against)
Only what a consumer develops against — nothing repo-internal:
- `include/grpcuds.h` + `include/grpcudspp/*.h`
- `proto/` — the contract (`ble.proto` + nanopb `ble.options`); add your own
  `.proto` here and run the same codegen on it
- `nanopb/` — generator + runtime source (codegen + `pb_*.c` compiled into the server)
- `docs/` — the developer guides (C_API_GUIDE, CPP_API_GUIDE, MIGRATING_FROM_GRPC_CPP,
  THREADING) **plus `docs/api/`** — the Doxygen symbol reference, regenerated at
  package time (skipped with a warning if `doxygen` isn't installed)

Two **examples live at the bundle root** (next to `target/`, `host/`,
`sdk/`): `example/` — the complete C++ BLE scanner service + typed-stub
client (builds from `sdk/proto/ble.proto`) — and `example-c/` — the same
library from plain C (echo server + client, self-contained proto,
nanopb-only codegen). Both CMakeLists auto-detect the bundle layout —
`cmake -S . -B build`, build, `./run_demo.sh build`.

The repo's example matrix (`tests/cpp`, `tests/rust`) deliberately does
**not** ship: it builds against repo-relative paths and is test/benchmark
infrastructure, not consumer material.

### c. Host-only (run on the build machine, never shipped to target)
Cross-compilation splits the toolchain from the runtime. These run on the host:
- `bin/protoc-gen-grpcudspp` — the protoc plugin (host arch)
- `nanopb_generator.py` — python codegen (host, in bucket b's `nanopb/`)
- `protoc` — taken from the host `PATH`

> Why (c) is split out: in a cross build the plugin and generator run on x86-64
> to *emit C/C++ source*, which the target toolchain then compiles. Shipping the
> host plugin to an armv7 device would be useless — it never runs there.

## 2. Build scripts

`./build.sh` builds the runtime + host tools, native or cross:

```sh
# Native host (x86-64): runtime lib + host plugin + pkg-config
./build.sh

# Cross (armv7): runtime lib only — host tools stay native
export SYSROOT=/abs/path/to/armv7-sysroot       # dynamic nghttp2 link
./build.sh --target armv7-unknown-linux-gnueabihf

# Static nghttp2 (self-contained .a, no libnghttp2.so dep on target)
./build.sh --target armv7-unknown-linux-gnueabihf --bundled
```

It only ever runs `cargo` + `gen-pkgconfig.sh`; the cross toolchain/sysroot are
**user-owned** (see `rust/.cargo/config.toml`). See [`docs/BUILDING.md`](BUILDING.md)
for the underlying cargo invocations and link-mode details.

## 3. Release / packaging script

`scripts/package.sh` stages the three buckets into the C/C++ distributable
tree from already-built artifacts (run `build.sh` first):

```sh
./build.sh --target armv7-unknown-linux-gnueabihf   # produce the target .a
./build.sh                                          # produce host plugin + pkg-config
scripts/package.sh --target armv7-unknown-linux-gnueabihf
# -> dist/grpcuds-<version>-cpp-armv7-.../{example,example-c,target,host,sdk}/  (+ .tar.gz)
scripts/package.sh               # native; `./build.sh package` wraps this
```

There is no Rust bundle: Rust consumers take the published crates (`grpcuds`,
`grpcuds-build`, …) from crates.io, keeping one source of truth for the crate
sources.

The crates.io channel is separate: `scripts/release.sh` publishes the
publishable crates in dependency order (dry-run by default, `--execute` to
publish). Publishing is irreversible and **user-owned** — the script never
handles credentials.
