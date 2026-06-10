#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# Helgrind race-detector pass for the C-ABI outbound mailbox.
#
#   cd tests/bench && ./helgrind.sh
#
# Builds the server libgrpcuds_ffi.a, compiles helgrind_mailbox.c (N producer
# threads hammering grpcuds_call_write off the I/O thread while main drains, plus
# a connection freed mid-flight), and runs it under helgrind. The mailbox's
# cross-thread sync is a pthread_mutex, which helgrind models exactly, so a clean
# run (exit 0) means the mailbox is data-race free. Exits non-zero on any race.
#
# NOTE: helgrind is run on this dedicated C harness, NOT on the cargo test
# binaries — the Rust test harness's lock-free internals (mpmc channels, atomics)
# helgrind cannot model and would report as false positives. Pure pthread + the
# mailbox mutex gives a clean, trustworthy signal.
#
# Requires valgrind (apt install valgrind).

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"

command -v valgrind >/dev/null || { echo "valgrind not installed" >&2; exit 1; }

ROOT=$(cd ../.. && pwd)
say() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }

say "Build libgrpcuds_ffi.a (server)"
(cd "$ROOT/rust" && cargo build --release -p grpcuds-ffi)
A="$ROOT/rust/target/release/libgrpcuds_ffi.a"

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

say "Compile mailbox race harness"
cc -O2 -I"$ROOT/rust/grpcuds-ffi/include" \
    helgrind_mailbox.c "$A" \
    -lnghttp2 -lpthread -ldl -lm \
    -o "$TMP/helgrind_mailbox"

say "helgrind: mailbox concurrency (producers + drain + teardown race)"
valgrind --tool=helgrind --error-exitcode=1 \
    "$TMP/helgrind_mailbox" "$TMP/hg.sock"

say "helgrind: clean — no data races in the outbound mailbox"
