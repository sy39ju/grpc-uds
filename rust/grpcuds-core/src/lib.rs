// SPDX-License-Identifier: MIT OR Apache-2.0
#![cfg_attr(not(test), no_std)]

//! Internal `no_std` substrate of the grpcuds stack: gRPC framing, the UDS
//! listener/connection driver and per-connection stream state machine over
//! nghttp2 (the `server` feature), plus a matching blocking client session
//! (the `client` feature).
//!
//! **This is an internal implementation crate, not a public API.** It is
//! published to crates.io only because the crates that *are* public — the
//! C ABI (`grpcuds-ffi`) and the safe Rust crate
//! [`grpcuds`](https://crates.io/crates/grpcuds) — depend on it, and
//! crates.io requires every dependency to be on the index. Depend on one of
//! those, not on this. Its items are `#[doc(hidden)]` and may change in any
//! release; semver discipline applies to the consuming crates' surfaces, not
//! to these internals.
//!
//! The genuinely reusable vocabulary types are re-exported (and kept stable)
//! by the `grpcuds` crate: [`GrpcStatus`], [`Backpressure`],
//! [`OverflowPolicy`].

extern crate alloc;

// Core invariants (see CLAUDE.md / DESIGN.md):
//   - panic-free: no unwrap/expect/panicking indexing in non-test code.
//   - no core::fmt: do not pull formatting machinery into the binary.
//   - core treats messages as opaque bytes + path + stream id; nanopb lives in C.

// Shared substrate (both server and client).
#[doc(hidden)]
pub mod allocator;
#[doc(hidden)]
pub mod framing;
#[doc(hidden)]
pub mod headers;
#[doc(hidden)]
pub mod logging;
#[doc(hidden)]
pub mod session;

// Server-side machinery.
#[cfg(feature = "server")]
#[doc(hidden)]
pub mod conn;
#[cfg(feature = "server")]
#[doc(hidden)]
pub mod uds;

// Client-side (no_std blocking client).
#[cfg(feature = "client")]
#[doc(hidden)]
pub mod client;

// Dev-only wire logging (pcap with synthetic TCP/IP; default OFF).
#[cfg(feature = "wirelog")]
#[doc(hidden)]
pub mod wirelog;

// Vocabulary types shared with (and re-exported by) the `grpcuds` crate.
#[cfg(feature = "server")]
pub use conn::{Backpressure, OverflowPolicy};
pub use headers::GrpcStatus;

#[cfg(feature = "server")]
#[doc(hidden)]
pub use conn::{CancelHook, Conn, ConnError, HandlerFn, OutQueue, StreamCtx, StreamState};
#[doc(hidden)]
pub use framing::{decode_header, encode_header, FrameError, FrameHeader, FRAME_HEADER_LEN};
#[doc(hidden)]
pub use headers::{response_headers, trailer};
#[doc(hidden)]
pub use session::{Callbacks, Session, SessionError};
#[cfg(feature = "server")]
#[doc(hidden)]
pub use uds::{Connection, IoError, Listener, TickStatus};

#[cfg(feature = "client")]
#[doc(hidden)]
pub use client::{ClientCall, ClientConn, ClientError};

/// Milliseconds on the monotonic clock — deadline arithmetic for both the
/// client (`set_timeout`) and the server (`grpc-timeout`).
pub(crate) fn monotonic_ms() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    (ts.tv_sec as u64).saturating_mul(1000) + (ts.tv_nsec as u64) / 1_000_000
}
