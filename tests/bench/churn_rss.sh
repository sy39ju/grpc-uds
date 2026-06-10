#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# Server-memory regression under abusive client churn: 2000 cycles of
# connect -> call -> SIGKILL the client mid-flight. A correctly-reaping
# server holds flat RSS + fd count. The in-process regression net lives in
# tests/rust client.rs (client_churn_leaves_the_server_healthy /
# abrupt_client_drop_reclaims_the_server_stream); this script is the
# out-of-process RSS proof.
#   ./churn_rss.sh [example/ble/build]
# Abusive client churn: open a connection, send a request, then KILL the
# client mid-call (SIGKILL — no clean close, no RST) over and over. The
# server must reap each dead connection; RSS must not climb.
set -u
BUILD="${1:-example/ble/build}"
SRV="$BUILD/ble-server"
CLI="$BUILD/ble-client"
[ -x "$SRV" ] && [ -x "$CLI" ] || { echo "build example/ble first: cmake -S example/ble -B example/ble/build && cmake --build example/ble/build"; exit 1; }
SOCK=/tmp/churn-$$.sock
rm -f "$SOCK"
"$SRV" "$SOCK" >/dev/null 2>&1 &
SP=$!
trap 'kill $SP 2>/dev/null; rm -f "$SOCK"' EXIT
for _ in $(seq 1 100); do [ -S "$SOCK" ] && break; sleep 0.05; done

rss() { awk '/VmRSS/{print $2}' /proc/$SP/status; }
fds() { ls /proc/$SP/fd 2>/dev/null | wc -l; }

# Warm up, then baseline after a few clean runs.
for _ in 1 2 3; do "$CLI" "$SOCK" >/dev/null 2>&1; done
sleep 0.3
R0=$(rss); F0=$(fds)
echo "baseline: RSS=${R0}KB fds=${F0}"

# 2000 abusive cycles: start client, kill it mid-flight.
for i in $(seq 1 2000); do
    "$CLI" "$SOCK" >/dev/null 2>&1 &
    CP=$!
    # Kill almost immediately — connection established, call in flight.
    kill -9 $CP 2>/dev/null
    wait $CP 2>/dev/null
    if [ $((i % 500)) -eq 0 ]; then
        sleep 0.2
        echo "after $i kills: RSS=$(rss)KB fds=$(fds)"
    fi
done
sleep 0.5
R1=$(rss); F1=$(fds)
echo "final:    RSS=${R1}KB fds=${F1}"
echo "delta:    RSS +$((R1-R0))KB  fds +$((F1-F0))"
