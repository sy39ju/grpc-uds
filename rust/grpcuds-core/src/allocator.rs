// SPDX-License-Identifier: MIT OR Apache-2.0
//! System-malloc-backed `GlobalAlloc`.
//!
//! POSIX malloc only guarantees alignment up to `max_align_t`; for
//! over-aligned layouts we fall through to `posix_memalign` — the classic
//! `GlobalAlloc`-over-malloc pitfall. The instance must be installed by the
//! final binary boundary (cdylib/staticlib) — see `grpcuds-ffi`.

use core::alloc::{GlobalAlloc, Layout};
use core::ffi::c_void;
use core::mem;
use core::ptr;

extern "C" {
    fn malloc(size: usize) -> *mut c_void;
    fn free(ptr: *mut c_void);
    fn posix_memalign(memptr: *mut *mut c_void, alignment: usize, size: usize) -> i32;
}

/// `max_align_t` is at least `2 * usize` on every platform we target.
/// Stay conservative and only take the cheap `malloc` path when the
/// requested alignment fits in this bound.
const NATURAL_ALIGN: usize = mem::align_of::<usize>() * 2;

pub struct SystemAllocator;

unsafe impl GlobalAlloc for SystemAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if layout.size() == 0 {
            return layout.align() as *mut u8;
        }
        if layout.align() <= NATURAL_ALIGN {
            malloc(layout.size()) as *mut u8
        } else {
            let mut p: *mut c_void = ptr::null_mut();
            if posix_memalign(&mut p, layout.align(), layout.size()) == 0 {
                p as *mut u8
            } else {
                ptr::null_mut()
            }
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if layout.size() == 0 {
            return;
        }
        free(ptr as *mut c_void);
    }
}

#[cfg(test)]
mod tests {
    use super::SystemAllocator;
    use core::alloc::{GlobalAlloc, Layout};

    #[test]
    fn alloc_roundtrip_is_readable_and_writable() {
        let a = SystemAllocator;
        let layout = Layout::from_size_align(64, 8).unwrap();
        unsafe {
            let p = a.alloc(layout);
            assert!(!p.is_null());
            for i in 0..64 {
                *p.add(i) = i as u8;
            }
            for i in 0..64 {
                assert_eq!(*p.add(i), i as u8);
            }
            a.dealloc(p, layout);
        }
    }

    #[test]
    fn zero_size_alloc_returns_dangling_and_dealloc_is_a_noop() {
        let a = SystemAllocator;
        let layout = Layout::from_size_align(0, 16).unwrap();
        unsafe {
            let p = a.alloc(layout);
            // The contract: non-null, aligned, never dereferenced, and
            // dealloc must not free() it (it was never malloc'd).
            assert_eq!(p as usize, 16);
            a.dealloc(p, layout);
        }
    }

    #[test]
    fn over_aligned_layouts_take_the_memalign_path() {
        let a = SystemAllocator;
        for align in [64usize, 256, 4096] {
            let layout = Layout::from_size_align(32, align).unwrap();
            unsafe {
                let p = a.alloc(layout);
                assert!(!p.is_null());
                assert_eq!(p as usize % align, 0, "align {align}");
                *p = 0xAB; // must be writable
                a.dealloc(p, layout);
            }
        }
    }

    #[test]
    fn default_alloc_zeroed_yields_zeroed_memory() {
        let a = SystemAllocator;
        let layout = Layout::from_size_align(128, 8).unwrap();
        unsafe {
            let p = a.alloc_zeroed(layout);
            assert!(!p.is_null());
            for i in 0..128 {
                assert_eq!(*p.add(i), 0, "byte {i}");
            }
            a.dealloc(p, layout);
        }
    }
}
