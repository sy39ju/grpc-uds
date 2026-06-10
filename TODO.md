<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
# TODO — assessed-and-deferred work

Ideas that were evaluated, judged worthwhile, and parked. Each entry keeps
the design conclusion from when it was assessed, so picking it up later
does not restart the discussion. (Architectural phases live elsewhere:
client-streaming is `docs/THREADING.md` "Phase 2".)

## Runtime stats — per-connection / per-service / per-method counters

The useful core of gRPC's Channelz, without Channelz. Full
`grpc.channelz.v1` conformance was assessed and rejected: its proto needs
`google.protobuf.Any`/`Timestamp`/`Duration` (well-known types — the
nanopb path explicitly does not support `Any`), and its real purpose is
live operational introspection, not the wire-content debugging that
`wirelog` already covers better.

What to build instead, when needed:

- **Core counters**, maintained per connection and per registered method:
  active streams, calls started/completed by grpc-status, messages and
  bytes in/out, last-call timestamp. Size cost lands in the no_std core —
  measure against the budget; consider a `stats` cargo feature if it is
  not single-digit KB.
- **C ABI getters** (`grpcuds_conn_stats(...)`, `grpcuds_server_stats(...)`
  filling caller-owned structs) — no allocation, no protobuf.
- Optionally a tiny **self-describing debug service** (own `.proto`, plain
  fields only — the `health.h` hand-coded-wire pattern) so stock clients
  can query a live daemon. No well-known types, no conformance claim.
