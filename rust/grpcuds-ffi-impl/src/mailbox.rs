// SPDX-License-Identifier: MIT OR Apache-2.0
//! Thread-safe outbound mailbox for the server C ABI.
//!
//! The core is single-threaded: the nghttp2 session may only be touched on the
//! I/O thread. Real servers produce stream data on other threads (a BLE
//! callback, a worker pool). This mailbox is the cross-thread boundary —
//! producers enqueue a copy of the payload + poke a wakeup `eventfd`; the I/O
//! thread drains it ([`grpcuds_mailbox_drain`]) and makes the real
//! `grpcuds_call_*` calls. Off the I/O thread the core is never touched.
//!
//! It lives here (the FFI shim), NOT in `grpcuds-core`: the core keeps its
//! single-threaded, lock-free invariant; this is the one layer where the
//! cross-thread handoff is allowed. See `docs/THREADING.md`.
//!
//! Process-global singleton (one Server per process — the reference topology).
//! The lock is `pthread_mutex` (futex-backed: sleeps on contention, safe on a
//! single core; a spinlock would burn a uniprocessor timeslice). The critical
//! section is an O(1), syscall-free `push`/`swap`; the `eventfd` poke and the
//! core replay happen outside the lock.

use core::cell::UnsafeCell;
use core::ffi::{c_int, c_void};
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use grpcuds_core::{Conn, GrpcStatus};

/// One queued outbound op for a call. `payload` carries the write bytes, or the
/// (optional) `grpc-message` for a finish. `call` is the opaque call handle
/// (`*mut Conn`); a freed connection nulls it (see the tombstone registry).
struct Item {
    call: *mut c_void,
    call_id: i32,
    payload: Vec<u8>,
    finish: bool,
    status: GrpcStatus,
}

/// The process-global outbound mailbox.
pub(crate) struct Mailbox {
    /// Serializes access to `queue` + `dead`. `pthread_mutex`, const-init.
    lock: UnsafeCell<libc::pthread_mutex_t>,
    /// Pending ops, FIFO. Guarded by `lock`.
    queue: UnsafeCell<VecDeque<Item>>,
    /// Tombstoned call handles (freed connections). Guarded by `lock`. A small
    /// `Vec` (few connections freed-but-not-readdress-reused at once); linear
    /// scan, `try_reserve` to stay alloc-failure-safe.
    dead: UnsafeCell<Vec<usize>>,
    /// Wakeup `eventfd`, `-1` until lazily created. Atomic so `poke`/`drain`
    /// read it without the lock.
    wakeup: AtomicI32,
    /// Whether an I/O thread has been registered. Before that, every thread is
    /// treated as the I/O thread (zero-setup single-threaded servers).
    io_registered: AtomicBool,
    /// The registered I/O thread's `pthread_t` (as bits). Valid iff
    /// `io_registered`.
    io_thread: AtomicUsize,
}

// SAFETY: every access to the non-atomic interior (`queue`, `dead`, `lock`) is
// serialized by the `pthread_mutex` in `lock`; `wakeup`/`io_*` are atomics.
unsafe impl Sync for Mailbox {}

pub(crate) static MAILBOX: Mailbox = Mailbox::new();

/// RAII guard: unlocks the mutex on drop.
struct Guard<'a>(&'a Mailbox);
impl Drop for Guard<'_> {
    fn drop(&mut self) {
        // SAFETY: paired with the `pthread_mutex_lock` in `Mailbox::lock`.
        unsafe { libc::pthread_mutex_unlock(self.0.lock.get()) };
    }
}

impl Mailbox {
    const fn new() -> Self {
        Mailbox {
            lock: UnsafeCell::new(libc::PTHREAD_MUTEX_INITIALIZER),
            queue: UnsafeCell::new(VecDeque::new()),
            dead: UnsafeCell::new(Vec::new()),
            wakeup: AtomicI32::new(-1),
            io_registered: AtomicBool::new(false),
            io_thread: AtomicUsize::new(0),
        }
    }

    fn lock(&self) -> Guard<'_> {
        // SAFETY: `lock` is a valid, const-initialized pthread_mutex.
        unsafe { libc::pthread_mutex_lock(self.lock.get()) };
        Guard(self)
    }

    /// True if the caller may touch the core directly. Before any
    /// [`register_io_thread`](Self::register_io_thread), every thread qualifies
    /// (so single-threaded users keep the direct path with zero setup).
    pub(crate) fn on_io_thread(&self) -> bool {
        if !self.io_registered.load(Ordering::Acquire) {
            return true;
        }
        let stored = self.io_thread.load(Ordering::Relaxed) as libc::pthread_t;
        // SAFETY: pthread_self/equal are always safe to call.
        unsafe { libc::pthread_equal(stored, libc::pthread_self()) != 0 }
    }

    /// Mark the calling thread as the I/O thread; eagerly create the wakeup fd
    /// so producers can `poke` it immediately.
    pub(crate) fn register_io_thread(&self) {
        self.ensure_wakeup();
        // SAFETY: always safe.
        let me = unsafe { libc::pthread_self() };
        self.io_thread.store(me as usize, Ordering::Relaxed);
        self.io_registered.store(true, Ordering::Release);
    }

    /// The wakeup `eventfd` (created on first use). Add it to your poll set.
    pub(crate) fn wakeup_fd(&self) -> c_int {
        self.ensure_wakeup()
    }

    fn ensure_wakeup(&self) -> c_int {
        let fd = self.wakeup.load(Ordering::Acquire);
        if fd >= 0 {
            return fd;
        }
        let _g = self.lock();
        let fd = self.wakeup.load(Ordering::Relaxed);
        if fd >= 0 {
            return fd;
        }
        // SAFETY: standard eventfd creation; -1 on failure is stored as-is.
        let newfd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        self.wakeup.store(newfd, Ordering::Release);
        newfd
    }

    fn poke(&self) {
        let fd = self.wakeup.load(Ordering::Acquire);
        if fd < 0 {
            return;
        }
        let one: u64 = 1;
        // SAFETY: 8-byte write to a valid eventfd; EAGAIN only on counter
        // overflow (2^64-1), which we ignore.
        unsafe {
            libc::write(fd, core::ptr::addr_of!(one).cast::<c_void>(), 8);
        }
    }

    /// Queue a write. Returns `Err` only on allocation failure.
    pub(crate) fn enqueue_write(
        &self,
        call: *mut c_void,
        call_id: i32,
        data: &[u8],
    ) -> Result<(), ()> {
        let mut payload = Vec::new();
        if payload.try_reserve(data.len()).is_err() {
            return Err(());
        }
        payload.extend_from_slice(data);
        self.push(Item {
            call,
            call_id,
            payload,
            finish: false,
            status: GrpcStatus::Ok,
        })
    }

    /// Queue a finish (status + optional `grpc-message`). `Err` on alloc fail.
    pub(crate) fn enqueue_finish(
        &self,
        call: *mut c_void,
        call_id: i32,
        status: GrpcStatus,
        msg: &[u8],
    ) -> Result<(), ()> {
        let mut payload = Vec::new();
        if payload.try_reserve(msg.len()).is_err() {
            return Err(());
        }
        payload.extend_from_slice(msg);
        self.push(Item {
            call,
            call_id,
            payload,
            finish: true,
            status,
        })
    }

    fn push(&self, item: Item) -> Result<(), ()> {
        {
            let _g = self.lock();
            // SAFETY: under the lock.
            let q = unsafe { &mut *self.queue.get() };
            if q.try_reserve(1).is_err() {
                return Err(());
            }
            q.push_back(item);
        }
        self.poke();
        Ok(())
    }

    /// Drain on the I/O thread: clear the wakeup counter, then replay every
    /// queued op into the core in FIFO order, skipping freed connections.
    pub(crate) fn drain(&self) {
        let fd = self.wakeup.load(Ordering::Acquire);
        if fd >= 0 {
            let mut buf: u64 = 0;
            loop {
                // SAFETY: 8-byte read from a non-blocking eventfd; loops until
                // it would block (the counter is drained).
                let n = unsafe { libc::read(fd, core::ptr::addr_of_mut!(buf).cast::<c_void>(), 8) };
                if n != 8 {
                    break;
                }
            }
        }
        let items = {
            let _g = self.lock();
            // SAFETY: under the lock.
            let q = unsafe { &mut *self.queue.get() };
            core::mem::take(q)
        };
        for item in items {
            let dead = {
                let _g = self.lock();
                // SAFETY: under the lock.
                let d = unsafe { &*self.dead.get() };
                item.call.is_null() || d.contains(&(item.call as usize))
            };
            if dead {
                continue;
            }
            // SAFETY: the call handle is live (not tombstoned) and we are on the
            // I/O thread, the only thread allowed to touch the core. Errors
            // (stream already closed, etc.) are best-effort — ignored, as the
            // C++ mailbox does.
            unsafe {
                let conn = &mut *(item.call as *mut Conn);
                if item.finish {
                    let _ = conn.finish_call_msg(item.call_id, item.status, &item.payload);
                } else {
                    let _ = conn.write_call(item.call_id, &item.payload);
                }
            }
        }
    }

    /// Clear a tombstone for `call` (on accept — the allocator may reuse a
    /// freed connection's address).
    pub(crate) fn register_call(&self, call: *mut c_void) {
        let key = call as usize;
        let _g = self.lock();
        // SAFETY: under the lock.
        let d = unsafe { &mut *self.dead.get() };
        if let Some(pos) = d.iter().position(|&k| k == key) {
            d.swap_remove(pos);
        }
    }

    /// Tombstone `call` and scrub anything already queued for it (on
    /// `grpcuds_conn_free`, on the I/O thread). Later enqueues for the same
    /// handle are dropped at drain, never dereferenced.
    pub(crate) fn unregister_call(&self, call: *mut c_void) {
        let key = call as usize;
        let _g = self.lock();
        // SAFETY: under the lock.
        let q = unsafe { &mut *self.queue.get() };
        for it in q.iter_mut() {
            if it.call == call {
                it.call = core::ptr::null_mut();
            }
        }
        // SAFETY: under the lock.
        let d = unsafe { &mut *self.dead.get() };
        if !d.contains(&key) && d.try_reserve(1).is_ok() {
            d.push(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grpcuds_core::Conn;
    use std::boxed::Box;
    use std::sync::atomic::AtomicBool;
    use std::thread;
    use std::vec::Vec as StdVec;

    fn qlen(mb: &Mailbox) -> usize {
        let _g = mb.lock();
        unsafe { (*mb.queue.get()).len() }
    }
    fn dead_has(mb: &Mailbox, call: *mut c_void) -> bool {
        let _g = mb.lock();
        unsafe { (*mb.dead.get()).contains(&(call as usize)) }
    }
    /// A real but stream-less Conn. `drain`'s `write_call`/`finish_call_msg`
    /// return `Err(StreamNotFound)` (ignored), so dereferencing it is safe and
    /// the mailbox's concurrency surface is exercised without a full round-trip.
    fn leak_conn() -> *mut c_void {
        let conn = match Conn::new_server() {
            Ok(c) => c,
            Err(_) => panic!("Conn::new_server failed"),
        };
        Box::into_raw(Box::new(conn)) as *mut c_void
    }
    unsafe fn free_conn(p: *mut c_void) {
        drop(Box::from_raw(p as *mut Conn));
    }

    #[test]
    fn tombstone_scrubs_queued_and_drops_later() {
        let mb = Mailbox::new();
        // Fake handles: this test never drains, so they are never dereferenced.
        let a = 0xA000_usize as *mut c_void;
        let b = 0xB000_usize as *mut c_void;
        mb.enqueue_write(a, 1, b"x").unwrap();
        mb.enqueue_write(b, 1, b"y").unwrap();
        mb.enqueue_write(a, 2, b"z").unwrap();
        assert_eq!(qlen(&mb), 3);

        // Freeing A scrubs its queued items and tombstones it.
        mb.unregister_call(a);
        assert!(dead_has(&mb, a));
        {
            let _g = mb.lock();
            let q = unsafe { &*mb.queue.get() };
            assert_eq!(q.iter().filter(|it| it.call == a).count(), 0, "A scrubbed");
            assert_eq!(q.iter().filter(|it| it.call.is_null()).count(), 2);
            assert_eq!(q.iter().filter(|it| it.call == b).count(), 1, "B untouched");
        }
        // Re-accepting at A's reused address clears the tombstone.
        mb.register_call(a);
        assert!(!dead_has(&mb, a));
    }

    #[test]
    fn drain_into_real_streamless_conn_is_safe() {
        let mb = Mailbox::new();
        let conn = leak_conn();
        for i in 0..50u8 {
            mb.enqueue_write(conn, 1, &[i]).unwrap();
        }
        mb.enqueue_finish(conn, 1, GrpcStatus::Ok, b"").unwrap();
        assert_eq!(qlen(&mb), 51);
        mb.drain(); // stream-less conn -> Err, ignored; queue empties
        assert_eq!(qlen(&mb), 0);
        unsafe { free_conn(conn) };
    }

    /// The race the feature exists for: many producer threads enqueue off the
    /// I/O thread while one thread drains. The Conn is dereferenced only by the
    /// draining thread (the single-threaded-core invariant); producers touch
    /// only the mutex-guarded queue. Run under TSAN/helgrind to prove it
    /// race-free; standalone it proves nothing is lost and nothing crashes.
    #[test]
    fn concurrent_producers_with_single_drainer() {
        const PRODUCERS: usize = 8;
        const PER: usize = 2000;
        let mb = Mailbox::new();
        let mbr = &mb; // share &Mailbox (Sync) across threads, not the value
        let conn = leak_conn();
        let conn_addr = conn as usize;
        let done = AtomicBool::new(false);
        let doner = &done;

        thread::scope(|s| {
            let drainer = s.spawn(move || {
                while !doner.load(Ordering::Acquire) {
                    mbr.drain();
                }
                mbr.drain(); // final sweep after producers stop
            });
            let mut producers = StdVec::new();
            for p in 0..PRODUCERS {
                producers.push(s.spawn(move || {
                    let c = conn_addr as *mut c_void;
                    for i in 0..PER {
                        mbr.enqueue_write(c, 1, &[p as u8, i as u8]).unwrap();
                    }
                }));
            }
            for h in producers {
                h.join().unwrap();
            }
            done.store(true, Ordering::Release);
            drainer.join().unwrap();
        });

        assert_eq!(qlen(&mb), 0, "every enqueued item was drained");
        unsafe { free_conn(conn) };
    }

    /// Teardown racing producers: while producers enqueue for a connection, the
    /// I/O side tombstones + frees it. No drain dereferences a freed handle.
    #[test]
    fn unregister_races_producers_without_use_after_free() {
        const PER: usize = 5000;
        let mb = Mailbox::new();
        let mbr = &mb;
        let conn = leak_conn();
        let conn_addr = conn as usize;

        thread::scope(|s| {
            // Producer: hammer enqueues for the connection.
            s.spawn(move || {
                let c = conn_addr as *mut c_void;
                for i in 0..PER {
                    let _ = mbr.enqueue_write(c, 1, &[i as u8]);
                }
            });
            // I/O side: drain a few times, then tombstone the connection. Any
            // item enqueued after this must be dropped, never replayed.
            s.spawn(move || {
                let c = conn_addr as *mut c_void;
                for _ in 0..10 {
                    mbr.drain();
                }
                mbr.unregister_call(c);
                mbr.drain();
            });
        });
        // After unregister, drain must not replay onto the (about-to-be-)freed
        // conn. Scrub-and-drain once more, then free.
        mb.unregister_call(conn);
        mb.drain();
        assert_eq!(qlen(&mb), 0);
        unsafe { free_conn(conn) };
    }
}
