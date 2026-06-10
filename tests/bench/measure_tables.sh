#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# Apple-to-apple footprint: grpcuds (C++) vs grpcuds (Rust) vs tonic (Rust),
# same role + same service logic, for the domains whose transports run identical
# logic (BLE, AI agent). Prints two markdown tables: stripped binary size and
# idle PSS (one connection at steady state). x86-64.
#
# The C++ column needs tests/cpp built Release first (from the repo root;
# nanopb is auto-found at example/nanopb):
#   cmake -S tests/cpp -B tests/cpp/build -DCMAKE_BUILD_TYPE=Release \
#         && cmake --build tests/cpp/build
set -eo pipefail
cd "$(dirname "$0")"

# The comparison binaries live in the sizebench crate (its own workspace
# with the shared aggressive size profile).
(cd ../rust/sizebench && cargo build --release >/dev/null 2>&1)
SB=../rust/sizebench/target/release
CPP=${CPP_BUILD:-../cpp/build}
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

pss()  { awk '/^Pss:/{s+=$2} END{print s+0}' "/proc/$1/smaps_rollup" 2>/dev/null || echo 0; }
ksize() { [ -f "$1" ] && { cp "$1" "$TMP/x"; strip "$TMP/x" 2>/dev/null || true; echo "$(( ($(stat -c%s "$TMP/x")+512)/1024 )) KB"; } || echo "—"; }
wait_ready() { for _ in $(seq 1 100); do grep -q READY "$1" 2>/dev/null && return 0; sleep 0.05; done; }

# Build a statically-linked grpc++ binary once, into $TMP/static, so BOTH the
# size and PSS tables describe the same self-contained executable (grpc++ is
# otherwise a small app stub over multi-MB shared libs). Path on success, else
# empty. <domain> <role: server|client> <proto-basename>
GLIB=$(dirname "$(find /usr/lib -name libgrpc++.a 2>/dev/null | head -1)" 2>/dev/null || true)
mkdir -p "$TMP/static"
build_gpp_static() {
    local dom="$1" role="$2" proto="$3" out="$TMP/static/$1-$2"
    [ -x "$out" ] && { echo "$out"; return; }
    [ -n "$GLIB" ] && [ -f "$GLIB/libgrpc++.a" ] || return
    local gen="$CPP/$dom/generated_grpcpp" src="../cpp/$dom/grpcpp/${role}_main.cc"
    [ -d "$gen" ] && [ -f "$src" ] || return
    g++ -O2 -std=c++17 -ffunction-sections -fdata-sections -Wl,--gc-sections \
        -o "$out" "$src" "$gen/$proto.pb.cc" "$gen/$proto.grpc.pb.cc" \
        -I "$gen" -I "../cpp/common" \
        -Wl,--start-group "$GLIB"/libgrpc++.a "$GLIB"/libgrpc.a "$GLIB"/libprotobuf.a \
        "$GLIB"/libupb.a "$GLIB"/libabsl_*.a "$GLIB"/libaddress_sorting.a "$GLIB"/libgpr.a \
        -Wl,--end-group -lcares -lre2 -lz -lssl -lcrypto -lpthread -ldl -lm 2>/dev/null \
        && strip "$out" && echo "$out" || return
}

# pss_of <server_exe> <client_exe> <server|client> [client_extra_arg] → PSS
pss_of() {
    local sexe="$1" cexe="$2" which="$3" extra="${4:-}" sock="$TMP/m.sock"
    [ -x "$sexe" ] && [ -x "$cexe" ] || { echo "—"; return; }
    rm -f "$sock"
    "$sexe" "$sock" >"$TMP/s.log" 2>&1 & local spid=$!
    wait_ready "$TMP/s.log"
    "$cexe" "$sock" $extra >"$TMP/c.log" 2>&1 & local cpid=$!
    wait_ready "$TMP/c.log"
    sleep 0.3
    if [ "$which" = server ]; then echo "$(pss "$spid") KB"; else echo "$(pss "$cpid") KB"; fi
    kill "$spid" "$cpid" 2>/dev/null || true
    wait "$spid" "$cpid" 2>/dev/null || true
}

proto_of() { [ "$1" = agent ] && echo agent || echo ble; }

echo "### Binary size (stripped, statically linked)"
echo
echo "_Self-contained code size, apple-to-apple. grpc++ is linked STATICALLY"
echo "(libgrpc++/libgrpc/libprotobuf/absl .a, --start-group; openssl stays the"
echo "system .so); dynamically it is only a ~280 KB app binary but needs ~17 MB"
echo "of those shared libs on the device. grpcuds C++ statically links its ~20 KB"
echo "core and links the system libnghttp2 (~166 KB) dynamically; the Rust columns"
echo "statically link std + their stack._"
echo
echo "| role | grpcuds (C++) | grpc++ (C++) | grpcuds (Rust) | tonic (Rust) |"
echo "|------|--------------:|-------------:|---------------:|-------------:|"
for dom in ble agent; do
    p=$(proto_of "$dom")
    printf '| %-12s | %s | %s | %s | %s |\n' "$dom server" \
        "$(ksize "$CPP/$dom/$dom-gt-server")" "$(ksize "$(build_gpp_static "$dom" server "$p")")" \
        "$(ksize "$SB/$dom-grpcuds-server")" "$(ksize "$SB/$dom-tonic-server")"
    printf '| %-12s | %s | %s | %s | %s |\n' "$dom client" \
        "$(ksize "$CPP/$dom/$dom-measure-client")" "$(ksize "$(build_gpp_static "$dom" client "$p")")" \
        "$(ksize "$SB/$dom-grpcuds-client")" "$(ksize "$SB/$dom-tonic-client")"
done

echo
echo "### Idle PSS (one connection)"
echo
echo "_grpc++ measured on the same statically-linked binary as the size table._"
echo
echo "| role | grpcuds (C++) | grpc++ (C++) | grpcuds (Rust) | tonic (Rust) |"
echo "|------|--------------:|-------------:|---------------:|-------------:|"
for dom in ble agent; do
    p=$(proto_of "$dom")
    gsrv=$(build_gpp_static "$dom" server "$p") || true
    gcli=$(build_gpp_static "$dom" client "$p") || true
    printf '| %-12s | %s | %s | %s | %s |\n' "$dom server" \
        "$(pss_of "$CPP/$dom/$dom-gt-server" "$SB/$dom-grpcuds-client" server)" \
        "$(pss_of "${gsrv:-/nonexistent}" "$SB/$dom-grpcuds-client" server)" \
        "$(pss_of "$SB/$dom-grpcuds-server" "$SB/$dom-grpcuds-client" server)" \
        "$(pss_of "$SB/$dom-tonic-server" "$SB/$dom-tonic-client" server)"
    printf '| %-12s | %s | %s | %s | %s |\n' "$dom client" \
        "$(pss_of "$SB/$dom-grpcuds-server" "$CPP/$dom/$dom-measure-client" client)" \
        "$(pss_of "$SB/$dom-grpcuds-server" "${gcli:-/nonexistent}" client hold)" \
        "$(pss_of "$SB/$dom-grpcuds-server" "$SB/$dom-grpcuds-client" client)" \
        "$(pss_of "$SB/$dom-tonic-server" "$SB/$dom-tonic-client" client)"
done
