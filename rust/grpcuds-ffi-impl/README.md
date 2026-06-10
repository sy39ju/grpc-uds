<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
# grpcuds-ffi-impl

The `extern "C"` symbol implementation (`grpcuds_*`) for the grpcuds stack,
built on top of [`grpcuds-core`](https://crates.io/crates/grpcuds-core).

This is an **rlib** so the C ABI symbols can be unit-tested from Rust. The
actual linkable artifacts (cdylib + staticlib) are produced by
[`grpcuds-ffi`](https://crates.io/crates/grpcuds-ffi), which simply re-exports
this crate's symbols. You almost certainly want `grpcuds-ffi`, not this crate
directly.
