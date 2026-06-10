<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
# bindings/armv7

Cross-build notes for the **armv7** target. Unlike the host (x86-64) build —
which uses the nghttp2 headers vendored inside the crate at
`grpcuds-sys/vendor/nghttp2/` — cross targets resolve `<nghttp2/nghttp2.h>`
straight from the **cross sysroot**, because the headers are
environment-specific and are not vendored.

There is nothing to stage here. Provide a sysroot that already contains the
`libnghttp2-dev` headers and export it as `SYSROOT`:

```sh
rustup target add armv7-unknown-linux-gnueabihf
export SYSROOT=/abs/path/to/armv7-sysroot
cargo build --release --target armv7-unknown-linux-gnueabihf -p grpcuds-ffi
```

How `grpcuds-sys/build.rs` uses it for a cross `TARGET`:
- passes `--sysroot=$SYSROOT` to clang, so `nghttp2.h` and the libc headers
  it transitively includes (`stdint.h`, `sys/types.h`, …) resolve for the
  target;
- passes `--target=armv7-...`, so generated struct layouts use 32-bit sizes.

Confirm `libnghttp2.so.14` exists under `"$SYSROOT"/usr/lib/` — it is the
dynamic-link target.

**Sysroots assembled from Debian/Ubuntu cross packages** (`libc6-dev-armhf-
cross`): Debian's `libc.so` is a linker *script* containing absolute paths
(`/usr/arm-linux-gnueabihf/lib/...`). Under `--sysroot` the linker resolves
those *inside* the sysroot and fails with "cannot find ... inside $SYSROOT"
unless the path exists there too. Mirror it once:

```sh
mkdir -p "$SYSROOT/usr/arm-linux-gnueabihf"
ln -s ../lib "$SYSROOT/usr/arm-linux-gnueabihf/lib"
```

(The same applies to the CMake flow — `CMAKE_SYSROOT` passes the same
`--sysroot` to the linker.)
