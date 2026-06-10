#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# Stage the grpcuds C/C++ release bundle (see docs/PACKAGING.md):
#
#   dist/grpcuds-<version>-cpp/               (cross: -cpp-<target> suffix)
#     example/  buildable starter (BLE scanner service + client)      <- start here
#     target/   lib/libgrpcuds_ffi.a (+ .so), lib/pkgconfig/grpcuds.pc <- on device
#     host/     bin/protoc-gen-grpcudspp                               <- build machine
#     sdk/      include/, proto/, nanopb/, docs/ (+ docs/api Doxygen)  <- develop against
#
#   scripts/package.sh
#   scripts/package.sh --target armv7-unknown-linux-gnueabihf
#   scripts/package.sh --out /abs/dist --no-tar
#
# Rust consumers are served by crates.io (scripts/release.sh) — deliberately
# no source bundle, so the crates have one source of truth.
#
# This script does NOT build — run ./build.sh first (the bundle needs the lib
# + plugin). The host bucket always comes from the NATIVE build; the target
# bucket from target/<triple>/release when --target is given.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
WS="$REPO_ROOT/rust"

say()  { printf '\n\033[1m==> %s\033[0m\n' "$*"; }
warn() { printf '\033[33mwarning:\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[31merror:\033[0m %s\n' "$*" >&2; exit 1; }

# --- args ------------------------------------------------------------------
TARGET=""
OUT=""
MAKE_TAR=1
for ((i=1; i<=$#; i++)); do
    arg="${!i}"
    case "$arg" in
        --target)   j=$((i+1)); [[ -n "${!j:-}" ]] || die "--target needs a value"
                    TARGET="${!j}"; i=$j ;;
        --target=*) TARGET="${arg#--target=}" ;;
        --out)      j=$((i+1)); [[ -n "${!j:-}" ]] || die "--out needs a value"
                    OUT="${!j}"; i=$j ;;
        --out=*)    OUT="${arg#--out=}" ;;
        --no-tar)   MAKE_TAR=0 ;;
        -h|--help)  awk 'NR>2 && /^#/ {sub(/^# ?/,""); print; next} NR>2 {exit}' "${BASH_SOURCE[0]}"; exit 0 ;;
        *) die "unknown argument: $arg (try --help)" ;;
    esac
done

VERSION="$(awk -F'"' '/^version[[:space:]]*=/ {print $2; exit}' "$WS/Cargo.toml")"
[[ -n "$VERSION" ]] || die "could not read version from rust/Cargo.toml"

DIST="${OUT:-$REPO_ROOT/dist}"
TARGET_RELEASE="$WS/target/${TARGET:+$TARGET/}release"
HOST_RELEASE="$WS/target/release"

make_tar() {
    local stage="$1"
    if [[ "$MAKE_TAR" -eq 1 ]]; then
        say "Archiving"
        tar -C "$(dirname "$stage")" -czf "$stage.tar.gz" "$(basename "$stage")"
        echo "  -> $stage.tar.gz"
    fi
}

stage_licenses() {
    cp "$REPO_ROOT/LICENSE-MIT" "$REPO_ROOT/LICENSE-APACHE" "$REPO_ROOT/THIRD-PARTY-NOTICES" \
       "$1/" 2>/dev/null || true
}

# =============================================================================
package_cpp() {
    local STAGE="$DIST/grpcuds-${VERSION}-cpp${TARGET:+-$TARGET}"
    say "Packaging grpcuds $VERSION (C/C++, ${TARGET:-native}) -> $STAGE"
    rm -rf "$STAGE"
    mkdir -p "$STAGE"/{target/lib/pkgconfig,host/bin,sdk}

    # --- target/: deploy on device -------------------------------------------
    say "target/  (deploy on device)"
    local FFI_A="$TARGET_RELEASE/libgrpcuds_ffi.a"
    [[ -f "$FFI_A" ]] || die "missing $FFI_A — run: ./build.sh ${TARGET:+--target $TARGET}"
    cp "$FFI_A" "$STAGE/target/lib/"
    echo "  + lib/libgrpcuds_ffi.a"
    if [[ -f "$TARGET_RELEASE/libgrpcuds_ffi.so" ]]; then
        cp "$TARGET_RELEASE/libgrpcuds_ffi.so" "$STAGE/target/lib/"
        echo "  + lib/libgrpcuds_ffi.so"
    fi
    if [[ -f "$TARGET_RELEASE/grpcuds.pc" ]]; then
        cp "$TARGET_RELEASE/grpcuds.pc" "$STAGE/target/lib/pkgconfig/"
        echo "  + lib/pkgconfig/grpcuds.pc"
    else
        warn "no grpcuds.pc in $TARGET_RELEASE — generate it for the target prefix with"
        warn "  scripts/gen-pkgconfig.sh --prefix <target-prefix> -o $STAGE/target/lib/pkgconfig/grpcuds.pc"
    fi

    # --- host/: build-machine tools ------------------------------------------
    say "host/  (run on the build machine)"
    local PLUGIN="$HOST_RELEASE/protoc-gen-grpcudspp"
    if [[ -f "$PLUGIN" ]]; then
        cp "$PLUGIN" "$STAGE/host/bin/"
        echo "  + bin/protoc-gen-grpcudspp"
    else
        warn "missing $PLUGIN (host plugin) — run a native './build.sh' to produce it."
    fi

    # --- sdk/: what a consumer develops against ------------------------------
    say "sdk/  (headers + nanopb + proto + docs)"
    mkdir -p "$STAGE/sdk/include"
    cp "$REPO_ROOT/rust/grpcuds-ffi/include/grpcuds.h" "$STAGE/sdk/include/"
    cp -r "$REPO_ROOT/cpp/include/grpcudspp" "$STAGE/sdk/include/"
    echo "  + include/grpcuds.h, include/grpcudspp/"

    mkdir -p "$STAGE/sdk/proto"
    cp "$REPO_ROOT/example/ble/proto/"* "$STAGE/sdk/proto/"
    echo "  + proto/ble.proto, proto/ble.options"

    mkdir -p "$STAGE/sdk/docs"
    local d
    for d in C_API_GUIDE CPP_API_GUIDE MIGRATING_FROM_GRPC_CPP THREADING; do
        cp "$REPO_ROOT/docs/$d.md" "$STAGE/sdk/docs/"
    done
    echo "  + docs/{C_API_GUIDE,CPP_API_GUIDE,MIGRATING_FROM_GRPC_CPP,THREADING}.md"

    # Browsable API reference (Doxygen): regenerated fresh so it matches the
    # packaged headers. Skipped (with a warning) when doxygen isn't installed.
    if command -v doxygen >/dev/null 2>&1; then
        (cd "$REPO_ROOT" && doxygen Doxyfile >/dev/null)
        cp -r "$REPO_ROOT/docs/doxygen/html" "$STAGE/sdk/docs/api"
        echo "  + docs/api/ (Doxygen symbol reference — open index.html)"
    else
        warn "doxygen not installed — sdk/docs/api (symbol reference) skipped."
    fi

    # nanopb: generator + runtime from the pinned submodule (codegen + codec).
    local NANOPB_SRC="$REPO_ROOT/example/nanopb"
    if [[ -f "$NANOPB_SRC/pb.h" ]]; then
        mkdir -p "$STAGE/sdk/nanopb"
        cp "$NANOPB_SRC"/pb*.c "$NANOPB_SRC"/pb*.h "$STAGE/sdk/nanopb/" 2>/dev/null || true
        cp -r "$NANOPB_SRC/generator" "$STAGE/sdk/nanopb/"
        echo "  + nanopb/ (pb_*.c runtime + generator)"
    else
        warn "nanopb is empty — run: git submodule update --init example/nanopb"
        warn "SDK bundle will lack nanopb (server-side codegen + runtime)."
    fi

    # --- example/: the buildable examples, at the BUNDLE ROOT -----------------
    say "example/  (start developing here)"
    cp -r "$REPO_ROOT/example/ble" "$STAGE/example"
    rm -rf "$STAGE/example/build" "$STAGE/example/build-"* "$STAGE/example/proto"
    echo "  + example/ (C++: BLE scanner service + client; builds from sdk/proto/ble.proto)"
    cp -r "$REPO_ROOT/example/c" "$STAGE/example-c"
    rm -rf "$STAGE/example-c/build" "$STAGE/example-c/build-"*
    echo "  + example-c/ (plain C: echo server + client; self-contained proto)"

    stage_licenses "$STAGE"
    cat > "$STAGE/README.txt" <<'EOF'
grpcuds — wire-compatible gRPC server+client over UNIX domain sockets (C/C++).

  example/ complete BLE service + client (C++)      -> START HERE
  example-c/ the same BLE service in plain C
  target/  lib/libgrpcuds_ffi.{a,so} (+ grpcuds.pc
           in native bundles)                        -> deploy/link on the device
  host/    bin/protoc-gen-grpcudspp                  -> codegen tool, build machine
  sdk/     include/, proto/, nanopb/, docs/          -> what you develop against

Prerequisites on the build machine: cmake, a C/C++ toolchain, protoc
(e.g. apt install protobuf-compiler), python3 + the protobuf package
(apt install python3-protobuf — for the bundled nanopb generator).

Start here:

  cd example
  cmake -S . -B build && cmake --build build
  ./run_demo.sh build        # BLE scan round-trip -> "example: OK"

Then make it yours: edit sdk/proto/ble.proto + .options and the two mains
— swap the simulated radio for your platform's API and the contract stays
(example/README.md walks through it). API reference: sdk/docs/api/index.html.
EOF
    make_tar "$STAGE"
}

# --- dispatch ----------------------------------------------------------------
package_cpp

say "Done."
