<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
# grpcuds-core

`#![no_std]` core of **grpcuds** — a lightweight gRPC **server** transport over
UNIX domain sockets that is **wire-compatible with standard gRPC clients**
(HTTP/2 via system libnghttp2 + gRPC length-prefixed framing). UDS-only, no TLS
(designed for local IPC on a single host).

The core owns gRPC framing, the UDS listener/connection driver, and the
per-stream state machine. It does **not** know about message serialization — it
deals in framed bytes + `:path` + stream id, so it is independent of any
particular `.proto` (nanopb encode/decode happens in the C stub layer).

Design constraints: `panic = "abort"` and panic-free (all error paths are
`Result`), no `core::fmt`, system malloc as the global allocator. See the
repository `DESIGN.md` for the rationale.

## Layering

```
grpcuds-sys   raw nghttp2 FFI
grpcuds-core  ← this crate: safe framing + UDS + stream state machine (no_std)
grpcuds-ffi   C ABI surface (cdylib + staticlib)
```
