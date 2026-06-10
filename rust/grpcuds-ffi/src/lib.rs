// SPDX-License-Identifier: MIT OR Apache-2.0
// `--all-targets` builds this lib as a test harness, which links `std` and
// would clash with our `#[panic_handler]`/`no_std`. Drop both under `test`.
#![cfg_attr(not(test), no_std)]

//! cdylib + staticlib shell. The exported C ABI symbols (grpcuds_*) come
//! from [`grpcuds_ffi_impl`] which is an `rlib` we link in. This crate
//! itself only contributes `#[global_allocator]` + `#[panic_handler]`
//! and re-exports the symbols so the linker keeps them in the final
//! library.

// Re-export the entire C ABI surface. `#[no_mangle]` symbols on the
// `extern "C" fn`s in grpcuds-ffi-impl are preserved through the rlib →
// cdylib link and become part of this library's exported symbol table.
pub use grpcuds_ffi_impl::*;

// System-malloc-backed global allocator. Installed here because cdylib /
// staticlib is the binary boundary.
#[global_allocator]
static GLOBAL: grpcuds_core::allocator::SystemAllocator = grpcuds_core::allocator::SystemAllocator;

// panic = "abort" + no_std cdylib/staticlib needs a #[panic_handler] symbol.
// We can't catch_unwind anyway, so the handler just aborts via libc.
// Skipped under `test`, where `std` already provides the panic handler.
#[cfg(not(test))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    extern "C" {
        fn abort() -> !;
    }
    unsafe { abort() }
}

// On 32-bit ARM, compiler_builtins objects (e.g. the 64-bit division
// helpers a C++ consumer's `std::chrono` pulls in) carry .ARM.extab entries
// that reference this personality symbol even under panic = "abort", where
// no unwinding can ever happen. Without it, linking the staticlib with g++
// fails with "undefined reference to rust_eh_personality" the moment any
// such helper is used. A no-op satisfies the reference; it is never called.
#[cfg(not(test))]
#[no_mangle]
extern "C" fn rust_eh_personality() {}
