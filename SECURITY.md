# Security Policy

grpcuds is a local-IPC transport: it deliberately has no TLS or
authentication — the UNIX socket's filesystem permissions are the security
boundary, and the peer is assumed to be a same-device process. Reports that
amount to "there is no TLS" are by design (see the feature table in the
README). In scope are memory-safety issues, wire-parsing bugs reachable by
a local peer (malformed HTTP/2 / gRPC framing), and anything that lets one
connection **corrupt** another. Plain resource exhaustion is **out of
scope** by design: the transport caps per-message size but not connection
count or (by default) outbound queue depth, so a hostile peer can exhaust
memory — bounding that is the integrator's job (see the README Security
section's resource-limits note). A report is in scope only if it shows
memory *corruption* or cross-connection interference beyond mere
contention for memory/CPU.

## Reporting a vulnerability

Please report privately via
[GitHub security advisories](https://github.com/sy39ju/grpc-uds/security/advisories/new)
rather than a public issue. Include a reproduction (a byte sequence or a
client snippet is ideal). You should receive an acknowledgement within a
week; fixes ship as a patch release of the affected crates with credit
unless you prefer otherwise.

## Supported versions

The latest published minor release line receives fixes.
