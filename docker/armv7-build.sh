#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# Runs INSIDE the grpcuds-armv7 container (docker/armv7-cross.Dockerfile):
# cross-builds the armv7 C ABI library (dynamic nghttp2 — the project
# invariant) — which exercises the cargo cross flow end to end — measures the
# C-embed contributions with the documented probe method, then cross-builds
# the BLE example via the CMake toolchain file and runs it under qemu-arm as
# a functional smoke test. Artifacts + numbers land in /out.
#
#   docker build -t grpcuds-armv7 -f docker/armv7-cross.Dockerfile docker/
#   docker run --rm -v "$PWD":/src:ro -v /tmp/armv7-out:/out grpcuds-armv7 \
#       /src/docker/armv7-build.sh
set -euo pipefail

# Target-scoped: a global RUSTFLAGS would leak the arm sysroot into HOST
# builds (e.g. the protoc plugin / build scripts) and break their links.
export CARGO_TARGET_ARMV7_UNKNOWN_LINUX_GNUEABIHF_RUSTFLAGS="-C link-arg=--sysroot=/sysroot"
export CARGO_TARGET_DIR=/build
TRIPLE=armv7-unknown-linux-gnueabihf
R=/build/$TRIPLE/release

say() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }

# --- 0. writable source copy --------------------------------------------------
# /src is mounted read-only, and a fresh checkout needs in-tree writes the
# local working tree happens to already have: cargo creates rust/Cargo.lock
# (gitignored — never in a checkout) and the nanopb generator bootstraps its
# proto bindings (nanopb_pb2.py) next to itself on first run. Build from /work.
say "copy sources to /work (the /src mount is read-only)"
mkdir -p /work
tar -C /src \
    --exclude=rust/target \
    --exclude=rust/grpcuds-sys/vendor \
    --exclude='example/ble/build*' --exclude='cpp/build*' \
    -cf - rust example cpp | tar -C /work -xf -

# --- 1. the C ABI library, all three sides (dynamic nghttp2) -----------------
say "libgrpcuds_ffi ($TRIPLE, dynamic nghttp2)"
cd /work/rust
cargo build -q --release --target $TRIPLE -p grpcuds-ffi
cp "$R/libgrpcuds_ffi.a" /tmp/server.a
cargo build -q --release --target $TRIPLE -p grpcuds-ffi --no-default-features --features client
cp "$R/libgrpcuds_ffi.a" /tmp/client.a
cargo build -q --release --target $TRIPLE -p grpcuds-ffi --features client
cp "$R/libgrpcuds_ffi.a" /tmp/both.a
cp "$R/libgrpcuds_ffi.so" /tmp/both.so
file "$R/libgrpcuds_ffi.so" | sed 's/, BuildID.*//'
arm-linux-gnueabihf-readelf -d "$R/libgrpcuds_ffi.so" | grep NEEDED

# --- 2. C-embed contribution probes (the documented method) ------------------
say "C-embed probes (stripped, -Os, --gc-sections)"
cd /tmp
cat > base.c <<'EOF'
int main(void){ return 0; }
EOF
cat > srv.c <<'EOF'
extern void grpcuds_server_bind_uds(void); extern void grpcuds_server_accept(void);
extern void grpcuds_server_register_method(void); extern void grpcuds_conn_tick(void);
extern void grpcuds_call_write(void); extern void grpcuds_call_finish(void);
void *keep[] = {(void*)grpcuds_server_bind_uds,(void*)grpcuds_server_accept,
  (void*)grpcuds_server_register_method,(void*)grpcuds_conn_tick,
  (void*)grpcuds_call_write,(void*)grpcuds_call_finish};
int main(void){ return !!keep[0]; }
EOF
cat > cli.c <<'EOF'
extern void grpcuds_client_connect(void); extern void grpcuds_client_unary(void);
extern void grpcuds_client_server_streaming(void); extern void grpcuds_stream_next(void);
extern void grpcuds_response_body(void); extern void grpcuds_response_status(void);
void *keep[] = {(void*)grpcuds_client_connect,(void*)grpcuds_client_unary,
  (void*)grpcuds_client_server_streaming,(void*)grpcuds_stream_next,
  (void*)grpcuds_response_body,(void*)grpcuds_response_status};
int main(void){ return !!keep[0]; }
EOF
CC="arm-linux-gnueabihf-gcc --sysroot=/sysroot -Os -ffunction-sections -fdata-sections"
LD="-Wl,--gc-sections -s -lnghttp2 -lpthread -ldl -lm"
$CC base.c -o base $LD
$CC srv.c server.a -o srv $LD
$CC cli.c client.a -o cli $LD
b=$(stat -c%s base); s=$(stat -c%s srv); c=$(stat -c%s cli)
arm-linux-gnueabihf-strip -o both-stripped.so both.so
{
    echo "armv7 measurements ($(date -u +%Y-%m-%d), docker ubuntu-ports sysroot, qemu-verified)"
    echo "  baseline probe:                  $b B"
    echo "  server contribution:             $((s-b)) B (~$(( (s-b+512)/1024 )) KB)"
    echo "  client contribution:             $((c-b)) B (~$(( (c-b+512)/1024 )) KB)"
    echo "  libgrpcuds_ffi.so (both, strip): $(stat -c%s both-stripped.so) B (~$(( ($(stat -c%s both-stripped.so)+512)/1024 )) KB)"
} | tee /out/armv7-sizes.txt

# --- 3. the CMake toolchain file: example/ble cross + qemu -------------------
say "example/ble via cmake/armv7-linux-gnueabihf.cmake, on qemu-arm"
(cd /work/rust && CARGO_TARGET_DIR=/build cargo build -q --release -p protoc-gen-grpcudspp)
cd /work/example/ble
cmake -S . -B /work/build-armv7 \
    -DCMAKE_TOOLCHAIN_FILE=cmake/armv7-linux-gnueabihf.cmake \
    -DGRPCUDS_FFI=/build/$TRIPLE/release/libgrpcuds_ffi.a \
    -DGRPCUDSPP_PLUGIN=/build/release/protoc-gen-grpcudspp \
    -DNANOPB_DIR=/work/example/nanopb >/dev/null
cmake --build /work/build-armv7 --parallel >/dev/null
SOCK=/tmp/ble-armv7.sock
qemu-arm /work/build-armv7/ble-server "$SOCK" > /tmp/sc.log 2>&1 &
SPID=$!
for _ in $(seq 1 100); do grep -q READY /tmp/sc.log 2>/dev/null && break; sleep 0.1; done
qemu-arm /work/build-armv7/ble-client "$SOCK"
kill "$SPID" 2>/dev/null || true
arm-linux-gnueabihf-strip -o /tmp/bs-cpp /work/build-armv7/ble-server
arm-linux-gnueabihf-strip -o /tmp/bc-cpp /work/build-armv7/ble-client
{
    echo "  C++ ble-server (stripped):       $(stat -c%s /tmp/bs-cpp) B (~$(( ($(stat -c%s /tmp/bs-cpp)+512)/1024 )) KB)"
    echo "  C++ ble-client (stripped):       $(stat -c%s /tmp/bc-cpp) B (~$(( ($(stat -c%s /tmp/bc-cpp)+512)/1024 )) KB)"
} | tee -a /out/armv7-sizes.txt
cp /tmp/bs-cpp /out/ble-server-arm 2>/dev/null || true
cp /tmp/bc-cpp /out/ble-client-arm 2>/dev/null || true

# --- artifacts ----------------------------------------------------------------
cp /tmp/server.a /tmp/client.a /tmp/both.a /tmp/both-stripped.so /out/ 2>/dev/null || true
chmod -R a+rw /out 2>/dev/null || true
say "Done — artifacts + armv7-sizes.txt in /out"
