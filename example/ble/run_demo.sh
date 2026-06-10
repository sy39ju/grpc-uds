#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
# Start the BLE server, wait for READY, run the self-checking client.
#   ./run_demo.sh build                    # binaries in <build-dir>/
#   ./run_demo.sh <server-bin> <client-bin>
set -eo pipefail

if [[ $# -eq 2 ]]; then
    server="$1"; client="$2"
else
    dir="${1:-build}"
    server="$dir/ble-server"; client="$dir/ble-client"
fi
if [[ ! -x "$server" || ! -x "$client" ]]; then
    echo "build first:  cmake -S . -B build && cmake --build build" >&2
    exit 1
fi

sock="/tmp/grpcuds-ble-$$.sock"
log="$(mktemp)"
"$server" "$sock" >"$log" 2>&1 &
spid=$!
cleanup() { kill "$spid" 2>/dev/null || true; wait "$spid" 2>/dev/null || true; rm -f "$sock" "$log"; }
trap cleanup EXIT

for _ in $(seq 1 100); do
    grep -q READY "$log" 2>/dev/null && break
    kill -0 "$spid" 2>/dev/null || { echo "server exited early:" >&2; cat "$log" >&2; exit 1; }
    sleep 0.05
done

"$client" "$sock"
