<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
# grpcuds-sys

Raw [bindgen](https://github.com/rust-lang/rust-bindgen)-generated FFI bindings
to the system [libnghttp2](https://nghttp2.org/), the HTTP/2 engine underneath
[`grpcuds-core`](https://crates.io/crates/grpcuds-core). `#![no_std]`.

This crate is an implementation detail of the grpcuds stack; you normally
depend on `grpcuds-core` rather than this directly.

## Why not the existing nghttp2 `-sys` crates?

Because their link model is the opposite of this project's core invariant.
[`libnghttp2-sys`](https://crates.io/crates/libnghttp2-sys) (the actively
maintained binding, built for the curl ecosystem) **always compiles its
vendored nghttp2 and links it statically** — it has no
dynamic-system-library mode at all, by design: it exists so curl can vendor
everything. grpcuds targets embedded devices where `libnghttp2.so` is
already present and the size budget is the whole point, so here the default
is **dynamic linking to the system library** and the static bundle
(+100–180 KB) is the opt-in. Secondary reasons: grpcuds-sys runs bindgen
per target against the deployment sysroot (correct 32-bit armv7 layouts)
instead of shipping one pregenerated binding, and it pins the bundled
source to the version actually on the target rather than following curl's
vendoring cadence. (`nghttp2-sys`, the other name, has been unmaintained
since 2018.)

The bundled source is an in-crate submodule shipped inside the package —
the same model as `libnghttp2-sys`/`curl-sys`/`libgit2-sys` — rather than
an `openssl-src`-style source crate: no `nghttp2-src` exists on crates.io,
and publishing one would add a second release unit for a single consumer
while the submodule frictions are already gone (registry packages carry
the source; git checkouts auto-init).

## Build prerequisites

- **libnghttp2** is **dynamically linked by default**
  (`cargo:rustc-link-lib=dylib=nghttp2`), and the headers and the library are
  always resolved from the **same sysroot** (`$SYSROOT`, or `/` for host
  builds) so they cannot version-skew: install `libnghttp2-dev` (host) or put
  it in the cross sysroot. If the headers are absent the build errors with
  instructions. (docs.rs alone, which links nothing, falls back to the
  submodule's headers — the same pinned source as `bundled`.)
- **libclang** is required at build time because `build.rs` runs bindgen.

## `bundled` feature — build & statically link nghttp2 (opt-in)

The default dynamic link is the project invariant (it keeps the library small;
see the root `CLAUDE.md`). When you instead need a self-contained binary with no
runtime dependency on a system `libnghttp2`, enable the `bundled` feature:

```bash
cargo build -p grpcuds-sys --features bundled
# or from a higher layer that forwards the feature:
cargo build -p grpcuds-ffi --features bundled
```

This builds `libnghttp2` (v1.59.0, matching the vendored headers) with CMake
— lib-only, static, no external deps — from the pinned `vendor/nghttp2-src/` submodule
inside this crate, and statically links the resulting `libnghttp2.a`.
bindgen then runs against the freshly built headers. The published package
ships the needed source subset, so `bundled` works from a crates.io
download too. **Requires** CMake and a C toolchain. In a git checkout the submodule is
auto-initialized by build.rs (or run `git submodule update --init
rust/grpcuds-sys/vendor/nghttp2-src` yourself); published packages ship the
source subset, so registry builds need no git at all.

Trade-off: a static nghttp2 adds ~100–180KB to the binary, so this is opt-in
only. The `bundled` feature is forwarded by `grpcuds-core`, `grpcuds-ffi-impl`,
`grpcuds-ffi`, and `grpcuds`.

## Cross-compiling

Cross targets (e.g. armv7) resolve `<nghttp2/nghttp2.h>` from the cross sysroot
instead of the vendored copy. Export `SYSROOT` so `build.rs` passes
`--sysroot` / `--target` to clang. See `../bindings/armv7/README.md`.
