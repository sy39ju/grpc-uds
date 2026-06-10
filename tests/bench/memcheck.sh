#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# Valgrind memcheck sweep: leaks + invalid memory accesses across the stack.
#
#   cd tests/bench && ./memcheck.sh
#
# Three passes:
#   1. grpcuds-core unit/wire tests (incl. in-process nghttp2
#      client<->server exchanges) — widest core coverage, clean exit.
#   2. grpcuds wrapper integration tests (mailbox threading, cancellation,
#      backpressure, typed handlers).
#   3. The bench server on a real UDS socket, driven through unary + a small
#      message stream + a large (16 KB, NO_COPY direct-send) stream, then
#      SIGTERM -> clean shutdown so the leak report covers the full run.
#
# Requires valgrind (apt install valgrind). Exits non-zero on any definite
# leak or memory error.

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"

command -v valgrind >/dev/null || { echo "valgrind not installed" >&2; exit 1; }

VG=(valgrind --quiet --error-exitcode=99 --leak-check=full
    --show-leak-kinds=definite,indirect --errors-for-leak-kinds=definite,indirect)

say() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }

# ---- 1+2: test binaries under memcheck -------------------------------------
say "Build test binaries (debug)"
( cd ../../rust && cargo test -p grpcuds-core -p grpcuds --features prost --no-run )

mapfile -t TEST_BINS < <(
    cd ../../rust && cargo test -p grpcuds-core -p grpcuds --features prost --no-run 2>&1 |
    grep -oE 'target/debug/deps/[a-z0-9_]+-[a-f0-9]+' | sort -u
)
for bin in "${TEST_BINS[@]}"; do
    say "memcheck: $bin"
    "${VG[@]}" "../../rust/$bin" --test-threads=1
done

# ---- 3: real-socket server under memcheck ----------------------------------
say "Build bench (release) + run server under memcheck"
cargo build --release
SOCK=/tmp/grpcuds-memcheck.sock
rm -f "$SOCK"
BENCH_STREAM_N=2000 "${VG[@]}" ./target/release/grpcuds_server "$SOCK" &
VG_PID=$!
for _ in $(seq 1 100); do [[ -S "$SOCK" ]] && break; sleep 0.2; done
[[ -S "$SOCK" ]] || { echo "server socket never appeared" >&2; kill $VG_PID; exit 1; }

say "drive: unary + 2k small messages"
BENCH_STREAM_N=2000 BENCH_EXTERNAL_SOCK="$SOCK" ./target/release/runner
say "drive: unary + 100 x 16KB messages (NO_COPY path)"
# NOTE: the server keeps its spawn-time BENCH_STREAM_N; restart for the large run.
kill -TERM $VG_PID; wait $VG_PID
rm -f "$SOCK"
BENCH_STREAM_N=100 BENCH_MSG_SIZE=16384 "${VG[@]}" ./target/release/grpcuds_server "$SOCK" &
VG_PID=$!
for _ in $(seq 1 100); do [[ -S "$SOCK" ]] && break; sleep 0.2; done
[[ -S "$SOCK" ]] || { echo "server socket never appeared (16KB run)" >&2; kill $VG_PID; exit 1; }
BENCH_STREAM_N=100 BENCH_MSG_SIZE=16384 BENCH_EXTERNAL_SOCK="$SOCK" ./target/release/runner
kill -TERM $VG_PID
wait $VG_PID
rm -f "$SOCK"

say "memcheck sweep PASSED (no definite leaks, no memory errors)"
