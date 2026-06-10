// SPDX-License-Identifier: MIT OR Apache-2.0
#![cfg_attr(not(test), no_std)]
#![allow(non_camel_case_types)]
// Safety contracts for the extern "C" surface are documented in the C header
// (rust/grpcuds-ffi/include/grpcuds.h) next to each symbol, which is where C
// consumers read them; duplicating them per-fn here adds drift risk.
#![allow(clippy::missing_safety_doc)]

//! Stable C ABI symbols for grpcuds.
//!
//! `rlib`-only crate carrying the `#[no_mangle] extern "C" fn` surface.
//! The `#[panic_handler]` and `#[global_allocator]` items live in the
//! sibling `grpcuds-ffi` crate (cdylib + staticlib) which links this one
//! to produce the public artifacts. The split lets `cargo test` work
//! (test harness uses std) while the production library stays no_std
//! + panic="abort".

extern crate alloc;

mod logging;
pub use logging::*;

#[cfg(feature = "server")]
mod mailbox;

#[cfg(feature = "server")]
mod server;
#[cfg(feature = "server")]
pub use server::*;

// The standard grpc.health.v1 service (Check + Watch). Part of the server
// surface; unused code is dropped by the linker's --gc-sections, so a server
// that never calls grpcuds_health_register pays nothing for it.
#[cfg(feature = "server")]
mod health;
#[cfg(feature = "server")]
pub use health::*;

#[cfg(feature = "client")]
mod client;
#[cfg(feature = "client")]
pub use client::*;
