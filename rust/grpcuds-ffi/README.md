<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
# grpcuds-ffi

The C ABI surface of **grpcuds**: builds the `cdylib` + `staticlib` that expose
the `grpcuds_*` C functions for consumers in C, C++, or any language with a C
FFI. The accompanying header is `include/grpcuds.h`.

This crate produces linkable artifacts only — it has **no Rust library API of
its own**. Depending on it from Rust is not useful; use it to build the
`libgrpcuds_ffi.{a,so}` outputs and link them from your native code (the C++
`grpcudspp` wrapper and protoc-generated stubs bottom out here).

```sh
cargo build --release -p grpcuds-ffi
# -> target/release/libgrpcuds_ffi.a  and  libgrpcuds_ffi.so
```

libnghttp2 is dynamically linked by default; the runtime target needs
`libnghttp2.so` available. To instead build and statically link nghttp2 from the
pinned submodule (a self-contained artifact, no runtime `libnghttp2.so`
dependency), enable the opt-in `bundled` feature:

```sh
cargo build --release -p grpcuds-ffi --features bundled
```

See [`grpcuds-sys`](https://crates.io/crates/grpcuds-sys) for build
prerequisites and the size trade-off.

## pkg-config

`pkgconfig/grpcuds.pc.in` is a pkg-config template for consumers that link the C
ABI. Generate a concrete `grpcuds.pc` for an install prefix with
[`scripts/gen-pkgconfig.sh`](../../scripts/gen-pkgconfig.sh):

```sh
# dynamic (default): nghttp2 is a Requires.private on the system libnghttp2
./scripts/gen-pkgconfig.sh --prefix /usr/local -o /usr/local/lib/pkgconfig/grpcuds.pc

# bundled (static): nghttp2 archive named directly, no system .pc required
./scripts/gen-pkgconfig.sh --prefix /opt/grpcuds --bundled -o /opt/grpcuds/lib/pkgconfig/grpcuds.pc
```

Consumers then resolve flags the usual way: `pkg-config --cflags --libs grpcuds`.
