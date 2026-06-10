#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# grpcuds build driver.
#
#   ./build.sh [command] [options]
#
# Commands:
#   all       (default) lib + plugin + grpcuds.pc — what a fresh checkout needs
#   lib       libgrpcuds_ffi.{a,so}  — the C ABI runtime, native or --target
#   plugin    protoc-gen-grpcudspp   — the host protoc plugin (codegen tool)
#   rlib      the safe Rust API crate (`grpcuds`) as an rlib + its deps
#   examples  build + run the full example matrix natively (Rust + C++ + cross)
#   package      stage the C/C++ dist bundle (+ .tar.gz). Rust has no bundle —
#             crates.io (scripts/release.sh) is its only channel, deliberately.
#
# Options (where they apply):
#   --side server|client|both   which C ABI halves the lib exports (default both;
#                               --server/--client/--both are shortcuts)
#   --bundled                   build libnghttp2 from the pinned submodule and
#                               statically link it — use when the (cross) sysroot
#                               has no libnghttp2.so. Default: dynamic system lib.
#   --wirelog                   DEV ONLY: compile in Wireshark wire logging.
#                               At runtime GRPCUDS_WIRELOG=<path>.pcap captures
#                               all gRPC traffic. Rotation: 1MiB x3 by default,
#                               tune with GRPCUDS_WIRELOG_FILE_KB / _FILES.
#                               Never ship.
#   --target <triple>           cross-compile lib/rlib/package (e.g.
#                               armv7-unknown-linux-gnueabihf). Host tools stay
#                               native. Dynamic mode needs SYSROOT exported.
#
# Examples:
#   ./build.sh                                        # native: lib + plugin + .pc
#   ./build.sh lib --side server                      # server-only .a/.so
#   ./build.sh lib --target armv7-... --bundled       # cross, no libnghttp2.so needed
#   SYSROOT=/abs/sysroot ./build.sh lib --target armv7-...   # cross, dynamic nghttp2
#   ./build.sh examples                               # build + run everything native
#   ./build.sh package --target armv7-...         # dist tar.gz for a new project
#
# Cross builds: toolchain, linker, and sysroot are ENVIRONMENT-SPECIFIC and
# user-owned (rust/.cargo/config.toml has the target block). See docs/BUILDING.md.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$SCRIPT_DIR"
WORKSPACE_DIR="$REPO_ROOT/rust"

say()  { printf '\n\033[1m==> %s\033[0m\n' "$*"; }
warn() { printf '\033[33mwarning:\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[31merror:\033[0m %s\n' "$*" >&2; exit 1; }

# --- args ------------------------------------------------------------------
COMMAND=""
TARGET=""
BUNDLED=0
WIRELOG=0
SIDE="both"
for ((i=1; i<=$#; i++)); do
    arg="${!i}"
    case "$arg" in
        all|lib|plugin|rlib|examples|package)
            [[ -z "$COMMAND" ]] || die "more than one command: $COMMAND, $arg"
            COMMAND="$arg" ;;
        --target)  j=$((i+1)); [[ -n "${!j:-}" ]] || die "--target needs a value"
                   TARGET="${!j}"; i=$j ;;
        --target=*) TARGET="${arg#--target=}" ;;
        --side)    j=$((i+1)); SIDE="${!j:-}"; i=$j ;;
        --side=*)  SIDE="${arg#--side=}" ;;
        --server)  SIDE="server" ;;
        --client)  SIDE="client" ;;
        --both)    SIDE="both" ;;
        --bundled) BUNDLED=1 ;;
        --wirelog) WIRELOG=1 ;;
        -h|--help)
            awk 'NR>2 && /^#/ {sub(/^# ?/,""); print; next} NR>2 {exit}' "${BASH_SOURCE[0]}"
            exit 0 ;;
        *) die "unknown argument: $arg (try --help)" ;;
    esac
done
COMMAND="${COMMAND:-all}"

case "$SIDE" in server|client|both) ;; *)
    die "invalid --side '$SIDE' (expected: server | client | both)" ;;
esac

# Feature flags for the chosen C ABI side + nghttp2 mode.
side_feature_flags() {
    local features=() no_default=0
    case "$SIDE" in
        server) ;;                        # default cargo feature is server
        client) no_default=1; features+=(client) ;;
        both)   features+=(client) ;;     # default server + client
    esac
    [[ "$BUNDLED" -eq 1 ]] && features+=(bundled)
    [[ "$WIRELOG" -eq 1 ]] && features+=(wirelog)
    [[ "$no_default" -eq 1 ]] && printf '%s\n' --no-default-features
    if [[ "${#features[@]}" -gt 0 ]]; then
        printf '%s\n' --features "$(IFS=,; echo "${features[*]}")"
    fi
}

target_flags=()
out_dir="$WORKSPACE_DIR/target/release"
if [[ -n "$TARGET" ]]; then
    target_flags+=(--target "$TARGET")
    out_dir="$WORKSPACE_DIR/target/$TARGET/release"
    # Dynamic nghttp2 (default) needs a sysroot with <nghttp2/nghttp2.h> AND
    # libnghttp2.so; --bundled builds nghttp2 from the submodule and needs
    # neither (only the libc headers for bindgen).
    if [[ "$BUNDLED" -eq 0 && -z "${SYSROOT:-}" ]]; then
        warn "SYSROOT is unset for a cross dynamic build — bindgen/link will likely"
        warn "fail to resolve <nghttp2/nghttp2.h>. export SYSROOT=/abs/sysroot,"
        warn "or pass --bundled if the sysroot has no libnghttp2.so."
    fi
fi

nghttp2_mode="dynamic-nghttp2"; [[ "$BUNDLED" -eq 1 ]] && nghttp2_mode="bundled-nghttp2"

# --- steps -------------------------------------------------------------------

build_lib() {  # libgrpcuds_ffi.a + libgrpcuds_ffi.so (one cargo build emits both)
    local feature_flags
    mapfile -t feature_flags < <(side_feature_flags)
    say "lib: grpcuds-ffi ${TARGET:+($TARGET) }$SIDE-side $nghttp2_mode"
    (cd "$WORKSPACE_DIR" && cargo build --release -p grpcuds-ffi \
        "${target_flags[@]}" "${feature_flags[@]}")
    [[ -f "$out_dir/libgrpcuds_ffi.a" ]] \
        || die "expected $out_dir/libgrpcuds_ffi.a but it is missing"
    echo "  -> $out_dir/libgrpcuds_ffi.a"
    if [[ -f "$out_dir/libgrpcuds_ffi.so" ]]; then
        echo "  -> $out_dir/libgrpcuds_ffi.so"
    else
        # Don't let a bare [[ ]] && be the last command — under set -e a
        # missing .so would silently abort the whole script here.
        warn "libgrpcuds_ffi.so was not produced (static .a only)"
    fi
}

build_plugin() {  # host protoc plugin (codegen tool — never cross-compiled)
    say "plugin: protoc-gen-grpcudspp (host)"
    (cd "$WORKSPACE_DIR" && cargo build --release -p protoc-gen-grpcudspp)
    echo "  -> $WORKSPACE_DIR/target/release/protoc-gen-grpcudspp"
}

gen_pc() {
    say "pkg-config: grpcuds.pc"
    local pc_args=(--prefix /usr/local -o "$out_dir/grpcuds.pc")
    [[ "$BUNDLED" -eq 1 ]] && pc_args+=(--bundled)
    "$SCRIPT_DIR/scripts/gen-pkgconfig.sh" "${pc_args[@]}"
    echo "  -> $out_dir/grpcuds.pc"
}

build_rlib() {  # the safe Rust API crate as an rlib (typed prost + tokio incl.)
    local features="prost,tokio"
    [[ "$SIDE" != "server" ]] && features+=",client"
    [[ "$BUNDLED" -eq 1 ]] && features+=",bundled"
    [[ "$WIRELOG" -eq 1 ]] && features+=",wirelog"
    say "rlib: grpcuds ${TARGET:+($TARGET) }(features: $features)"
    (cd "$WORKSPACE_DIR" && cargo build --release -p grpcuds \
        "${target_flags[@]}" --features "$features")
    local rlib
    rlib=$(ls "$out_dir"/libgrpcuds*.rlib 2>/dev/null | head -1 || true)
    echo "  -> ${rlib:-$out_dir/libgrpcuds-*.rlib}"
    echo "  (Rust consumers normally depend on the crate via cargo, not the rlib)"
}

run_examples() {  # build + run the full matrix natively: rust, cpp, cross-language
    [[ -z "$TARGET" ]] || die "examples run natively — drop --target"
    SIDE="both"  # the matrix needs both C ABI halves

    build_lib
    build_plugin

    if [[ ! -f "$REPO_ROOT/example/nanopb/pb_decode.h" ]]; then
        say "examples: init nanopb submodule"
        (cd "$REPO_ROOT" && git submodule update --init example/nanopb)
    fi

    say "examples: C++ side (root cmake: wrapper tests + the matrix)"
    cmake -S "$REPO_ROOT" -B "$REPO_ROOT/build" -DCMAKE_BUILD_TYPE=Release
    cmake --build "$REPO_ROOT/build" --parallel
    ctest --test-dir "$REPO_ROOT/build" --output-on-failure

    say "examples: Rust side (9 cells + harness)"
    (cd "$REPO_ROOT/tests/rust" && cargo test --workspace)

    say "examples: cross-language (Rust tonic peer <-> C++ binaries)"
    local B="$REPO_ROOT/build/tests/cpp"
    (cd "$REPO_ROOT/tests/rust" && \
        BLE_GT_SERVER_BIN="$B/ble/ble-gt-server" \
        BLE_TG_CLIENT_BIN="$B/ble/ble-tg-client" \
        AGENT_GT_SERVER_BIN="$B/agent/agent-gt-server" \
        AGENT_TG_CLIENT_BIN="$B/agent/agent-tg-client" \
        X509_GT_SERVER_BIN="$B/x509/x509-gt-server" \
        X509_TG_CLIENT_BIN="$B/x509/x509-tg-client" \
        cargo test -p cross)
}

run_package_cpp() {  # build what package.sh stages, then stage the dist tree
    build_lib
    if [[ -n "$TARGET" ]]; then
        # host bucket always comes from the native build
        say "package: native host tools"
        (cd "$WORKSPACE_DIR" && cargo build --release -p protoc-gen-grpcudspp)
    else
        build_plugin
        gen_pc
    fi
    say "package: stage the dist tree (+ tar.gz)"
    local pkg_args=()
    [[ -n "$TARGET" ]] && pkg_args+=(--target "$TARGET")
    "$SCRIPT_DIR/scripts/package.sh" "${pkg_args[@]}"
}

# --- dispatch ----------------------------------------------------------------
case "$COMMAND" in
    lib)      build_lib ;;
    plugin)   build_plugin ;;
    rlib)     build_rlib ;;
    examples) run_examples ;;
    package) run_package_cpp ;;
    all)
        [[ -n "$TARGET" ]] || warn "no --target: building for the native host."
        build_lib
        if [[ -z "$TARGET" ]]; then
            build_plugin
            gen_pc
        else
            say "Skipping host plugin + pkg-config (cross build)."
            echo "  protoc-gen-grpcudspp is a HOST tool — run './build.sh plugin' for it."
        fi
        ;;
esac

say "Done."
