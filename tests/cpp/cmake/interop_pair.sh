#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# interop_pair.sh <server_bin> <client_bin>
# Start the server, wait for its READY line, run the self-checking client, and
# exit with the client's status. Used as a ctest command for the same-language
# grpcuds ⇄ grpc++ interop cells.
set -eo pipefail
server="$1"
client="$2"
sock="/tmp/grpcuds-interop-$$-${RANDOM}.sock"
log="$(mktemp)"
rm -f "$sock"

"$server" "$sock" >"$log" 2>&1 &
spid=$!
cleanup() { kill "$spid" 2>/dev/null || true; wait "$spid" 2>/dev/null || true; rm -f "$sock" "$log"; }
trap cleanup EXIT

for _ in $(seq 1 100); do
    grep -q READY "$log" 2>/dev/null && break
    kill -0 "$spid" 2>/dev/null || { echo "server exited early:"; cat "$log"; exit 1; }
    sleep 0.05
done

"$client" "$sock"
