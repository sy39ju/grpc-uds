// SPDX-License-Identifier: MIT OR Apache-2.0
//! Per-connection state + nghttp2 callback wiring.
//!
//! Split into focused submodules:
//! * `state` — public `Conn`, `ConnState`, `StreamCtx`, `ConnError`, and the
//!   lifecycle/API methods on `Conn` (`new_server`, `register_method`,
//!   `write_call`, `finish_call`, `set_cancel_hook`, `set_stream_policy`,
//!   `recv`, `pull_send`, etc.)
//! * `dispatch` — `Conn::dispatch` and the per-stream `data_provider_read`
//!   callback that drains the OutQueue
//! * `callbacks` — the nghttp2 receive-side callbacks, the `NO_COPY`
//!   `send_data` callback, and the `install_callbacks` helper
//! * `out_queue` — `OutQueue`, `OverflowPolicy`, `Backpressure`
//!
//! Callers `use crate::conn::*` (or the re-exports from the crate root).

mod callbacks;
mod dispatch;
mod out_queue;
mod state;

#[cfg(test)]
mod tests;

pub use out_queue::{Backpressure, OutQueue, OverflowPolicy};
pub use state::{CancelHook, Conn, ConnError, HandlerFn, StreamCtx, StreamState};
