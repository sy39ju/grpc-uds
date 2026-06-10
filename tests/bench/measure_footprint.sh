#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# Reproduces the root README "Size & memory" table (x86-64):
#
#   1. C-embed contributions  — link a minimal C probe referencing each
#      side's entry symbols against the side-only libgrpcuds_ffi.a
#      (-Os -ffunction-sections -fdata-sections -Wl,--gc-sections -s),
#      delta over an empty-main baseline.
#   2. Rust std floor         — fn main(){} under the same size profile
#      the Rust comparison binaries use (opt-level=z, lto, cu=1, strip).
#   3. example/ble binaries   — THE C++ example, stripped.
#   4. Heap per active connection — grpcuds_server RSS delta while
#      conn_hold keeps N connections open, each with a server-streaming
#      call mid-flight (session + stream state + outbound queue resident).
#   5. Server idle PSS        — smaps_rollup with one idle connection.
#
# The Rust standalone / vs-tonic / vs-grpc++ tables come from
# ./measure_tables.sh; latency/throughput from ./target/release/runner.
#
#   ./measure_footprint.sh          # everything (builds what it needs)
set -euo pipefail
cd "$(dirname "$0")"
ROOT=$(cd ../.. && pwd)
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

say() { printf '\n\033[1m== %s\033[0m\n' "$*"; }

# ---- 1. C-embed contributions -------------------------------------------------
say "C-embed contributions (probe method)"
(cd "$ROOT/rust" && cargo build -q --release -p grpcuds-ffi)
cp "$ROOT/rust/target/release/libgrpcuds_ffi.a" "$TMP/server.a"
(cd "$ROOT/rust" && cargo build -q --release -p grpcuds-ffi --no-default-features --features client)
cp "$ROOT/rust/target/release/libgrpcuds_ffi.a" "$TMP/client.a"
(cd "$ROOT/rust" && cargo build -q --release -p grpcuds-ffi --features client)  # restore both

cat > "$TMP/base.c" <<'EOF'
int main(void){ return 0; }
EOF
cat > "$TMP/srv.c" <<'EOF'
extern void grpcuds_server_bind_uds(void); extern void grpcuds_server_accept(void);
extern void grpcuds_server_register_method(void); extern void grpcuds_conn_tick(void);
extern void grpcuds_call_write(void); extern void grpcuds_call_finish(void);
void *keep[] = {(void*)grpcuds_server_bind_uds,(void*)grpcuds_server_accept,
  (void*)grpcuds_server_register_method,(void*)grpcuds_conn_tick,
  (void*)grpcuds_call_write,(void*)grpcuds_call_finish};
int main(void){ return !!keep[0]; }
EOF
cat > "$TMP/cli.c" <<'EOF'
extern void grpcuds_client_connect(void); extern void grpcuds_client_unary(void);
extern void grpcuds_client_server_streaming(void); extern void grpcuds_stream_next(void);
extern void grpcuds_response_body(void); extern void grpcuds_response_status(void);
void *keep[] = {(void*)grpcuds_client_connect,(void*)grpcuds_client_unary,
  (void*)grpcuds_client_server_streaming,(void*)grpcuds_stream_next,
  (void*)grpcuds_response_body,(void*)grpcuds_response_status};
int main(void){ return !!keep[0]; }
EOF
CC="gcc -Os -ffunction-sections -fdata-sections"
LD="-Wl,--gc-sections -s -lnghttp2 -lpthread -ldl -lm"
$CC "$TMP/base.c" -o "$TMP/base" $LD
$CC "$TMP/srv.c" "$TMP/server.a" -o "$TMP/srv" $LD
$CC "$TMP/cli.c" "$TMP/client.a" -o "$TMP/cli" $LD
# Two views of the same probes. FILE delta is what lands on flash but
# quantizes at 4 KB page boundaries (a tiny probe can under- or over-state
# by a page when .text crosses one). CONTENT delta (section-byte sum via
# size -A) is the steady code+data cost and is the primary figure.
content() { size -A "$1" | awk '/Total/{print $2}'; }
b=$(stat -c%s "$TMP/base"); s=$(stat -c%s "$TMP/srv"); c=$(stat -c%s "$TMP/cli")
bc=$(content "$TMP/base"); sc=$(content "$TMP/srv"); cc=$(content "$TMP/cli")
printf 'baseline probe:      %7d B file, %6d B content\n' "$b" "$bc"
printf 'server contribution: %7d B content (~%d KB)   [file delta %d B]\n' \
    "$((sc-bc))" "$(( (sc-bc+512)/1024 ))" "$((s-b))"
printf 'client contribution: %7d B content (~%d KB)   [file delta %d B]\n' \
    "$((cc-bc))" "$(( (cc-bc+512)/1024 ))" "$((c-b))"

# ---- 2. Rust std floor ---------------------------------------------------------
say "Rust std floor (fn main(){}, size profile)"
mkdir -p "$TMP/hw/src"
cat > "$TMP/hw/Cargo.toml" <<'EOF'
[package]
name = "hw"
version = "0.0.0"
edition = "2021"
[workspace]
[profile.release]
opt-level = "z"
lto = true
codegen-units = 1
strip = true
panic = "abort"
EOF
echo 'fn main() {}' > "$TMP/hw/src/main.rs"
(cd "$TMP/hw" && cargo build -q --release)
printf 'std floor: %d B (~%d KB)\n' "$(stat -c%s "$TMP/hw/target/release/hw")" \
    "$(( ($(stat -c%s "$TMP/hw/target/release/hw")+512)/1024 ))"

# ---- 3. example/ble binaries ----------------------------------------------------
say "example/ble binaries (stripped, section-sum)"
# Measure the STANDALONE example/ble build, which defaults to MinSizeRel
# (the README figure). The root aggregate build ($ROOT/build) may be
# Release/Debug — don't measure that here.
BLE="$ROOT/example/ble/build"
if [ ! -x "$BLE/ble-server" ]; then
    echo "example/ble not built — cmake -S $ROOT/example/ble -B $ROOT/example/ble/build" \
         "&& cmake --build $ROOT/example/ble/build  (defaults to MinSizeRel)" >&2
else
    for bin in ble-server ble-client; do
        cp "$BLE/$bin" "$TMP/x" && strip "$TMP/x"
        # section-sum (size -A Total), same metric as the C-embed rows.
        sz=$(content "$TMP/x")
        printf '%-12s %7d B (~%d KB)\n' "$bin" "$sz" "$(( (sz+500)/1000 ))"
    done
fi

# ---- 4 + 5. heap per active connection, idle PSS --------------------------------
say "heap per active connection + idle PSS (grpcuds_server)"
cargo build -q --release
SK="$TMP/fp.sock"
# Small per-call stream: the held state is the session + stream machinery +
# a few queued messages — not the 50k-message bench burst.
BENCH_STREAM_N=8 ./target/release/grpcuds_server "$SK" & SP=$!
sleep 0.5
rss() { awk '/VmRSS/{print $2}' "/proc/$SP/status"; }
R0=$(rss)
N=32
./target/release/conn_hold "$SK" $N 6 & CH=$!
# Wait (bounded) for the held connections to show up in the server's RSS.
for _ in $(seq 1 50); do
    [ "$(rss)" != "$R0" ] && break
    kill -0 "$CH" 2>/dev/null || { echo "conn_hold exited before connecting" >&2; kill "$SP"; exit 1; }
    sleep 0.2
done
sleep 2   # all N streams mid-flight
R1=$(rss)
printf 'server RSS: idle %d KB -> %d conns held %d KB  => ~%d KB per active conn\n' \
    "$R0" "$N" "$R1" "$(( (R1-R0)/N ))"
PSS=$(awk '/^Pss:/{print $2}' "/proc/$SP/smaps_rollup")
printf 'server PSS with %d idle-ish conns: %d KB\n' "$N" "$PSS"
kill "$CH" 2>/dev/null || true; wait "$CH" 2>/dev/null || true
sleep 0.5
PSS0=$(awk '/^Pss:/{print $2}' "/proc/$SP/smaps_rollup")
printf 'server PSS after conns closed (idle): %d KB\n' "$PSS0"
kill "$SP" 2>/dev/null || true; wait "$SP" 2>/dev/null || true

say "done — vs-tonic / vs-grpc++ tables: ./measure_tables.sh; perf: ./target/release/runner"
