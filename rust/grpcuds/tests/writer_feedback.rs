// SPDX-License-Identifier: MIT OR Apache-2.0
//! The two writer feedback paths a real app depends on:
//!
//!   1. **Cancellation** — a producer thread streaming to a client that died
//!      must observe `write() == false` (and `is_cancelled()`) instead of
//!      spinning forever on a dead stream.
//!   2. **Backpressure** — `set_backpressure` bounds the per-stream outbound
//!      queue: `DropOldest` keeps the newest N messages; `Reject` refuses the
//!      excess and reports it via the sticky `overflowed()` flag.

use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

use grpcuds::{Backpressure, OverflowPolicy, Server, ServerBuilder, ServerWriter, Status};
use grpcuds_core::{decode_header, FRAME_HEADER_LEN};

mod common;
use common::{call, call_then_vanish, unique_path};

struct ServerHarness {
    sock: String,
    shutdown: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl ServerHarness {
    fn start(build: impl FnOnce(ServerBuilder) -> ServerBuilder) -> Self {
        let sock = unique_path();
        let server = build(Server::builder().bind(&sock)).build().expect("build");
        let shutdown = Arc::new(AtomicBool::new(false));
        let sd = shutdown.clone();
        let handle = thread::spawn(move || {
            server.serve(&sd).expect("serve");
        });
        ServerHarness {
            sock,
            shutdown,
            handle: Some(handle),
        }
    }
}

impl Drop for ServerHarness {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Split the response body into unframed messages.
fn messages(mut data: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    while !data.is_empty() {
        let h = decode_header(data, 4 * 1024 * 1024)
            .ok()
            .expect("decode_header");
        let pl = h.payload_len as usize;
        out.push(data[FRAME_HEADER_LEN..FRAME_HEADER_LEN + pl].to_vec());
        data = &data[FRAME_HEADER_LEN + pl..];
    }
    out
}

#[test]
fn dead_client_stops_the_producer() {
    // The producer thread reports how it exited through this channel.
    let (tx, rx) = mpsc::channel::<(bool, u32)>(); // (saw_cancel, writes_done)

    let srv = ServerHarness::start(|b| {
        let tx = Mutex::new(tx);
        b.add_server_streaming(
            "/echo.Echo/Forever",
            move |_req: &[u8], w: &ServerWriter| {
                let w = w.clone();
                let tx = tx.lock().unwrap().clone();
                thread::spawn(move || {
                    // An endless producer: only `write` returning false stops it.
                    let mut n = 0u32;
                    loop {
                        thread::sleep(Duration::from_millis(5));
                        if w.write(b"tick").is_err() {
                            let _ = tx.send((w.is_cancelled(), n));
                            return;
                        }
                        n += 1;
                        if n > 2_000 {
                            let _ = tx.send((false, n)); // runaway producer: feedback never arrived
                            return;
                        }
                    }
                });
                Status::ok()
            },
        )
    });

    // Client receives the first message, then vanishes without closing the
    // stream — exactly the "phone walked out of BLE range" shape.
    let seen = call_then_vanish(&srv.sock, b"/echo.Echo/Forever", b"go");
    assert!(
        !seen.is_empty(),
        "client should have received at least one frame"
    );

    // The producer must notice within a couple of poll cycles, not run away.
    let (saw_cancel, writes) = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("producer never exited — writer feedback is broken");
    assert!(saw_cancel, "producer exited but is_cancelled() was false");
    assert!(
        writes < 1_000,
        "producer spun {writes} times before noticing"
    );
}

#[test]
fn drop_oldest_keeps_the_newest_messages() {
    // Handler queues 8 messages inline with a capacity-2 DropOldest bound.
    // All 8 ops apply in one mailbox batch (before any flush), so the queue
    // deterministically ends up holding exactly the newest two.
    let srv = ServerHarness::start(|b| {
        b.add_server_streaming("/echo.Echo/Latest", |_req: &[u8], w: &ServerWriter| {
            w.set_backpressure(Backpressure::Bounded {
                capacity: NonZeroUsize::new(2).unwrap(),
                policy: OverflowPolicy::DropOldest,
            })
            .expect("policy on a live call");
            for i in 0u8..8 {
                let _ = w.write(&[b'm', i]);
            }
            let _ = w.finish(Status::ok());
            Status::ok()
        })
    });

    let st = call(&srv.sock, b"/echo.Echo/Latest", b"go");
    assert_eq!(st.grpc_status.as_deref(), Some(&b"0"[..]));
    assert_eq!(
        messages(&st.data),
        vec![vec![b'm', 6], vec![b'm', 7]],
        "DropOldest/capacity=2 must keep exactly the newest two messages"
    );
}

#[test]
fn reject_drops_excess_and_sets_overflow() {
    // Same burst, Reject policy: the first two messages fit, the rest bounce
    // and flip the sticky overflow flag on the writer.
    let writer_slot: Arc<Mutex<Option<ServerWriter>>> = Arc::new(Mutex::new(None));
    let slot = writer_slot.clone();

    let srv = ServerHarness::start(move |b| {
        b.add_server_streaming("/echo.Echo/NoDrop", move |_req: &[u8], w: &ServerWriter| {
            *slot.lock().unwrap() = Some(w.clone());
            w.set_backpressure(Backpressure::Bounded {
                capacity: NonZeroUsize::new(2).unwrap(),
                policy: OverflowPolicy::Reject,
            })
            .expect("policy on a live call");
            for i in 0u8..8 {
                let _ = w.write(&[b'r', i]);
            }
            let _ = w.finish(Status::ok());
            Status::ok()
        })
    });

    let st = call(&srv.sock, b"/echo.Echo/NoDrop", b"go");
    assert_eq!(st.grpc_status.as_deref(), Some(&b"0"[..]));
    assert_eq!(
        messages(&st.data),
        vec![vec![b'r', 0], vec![b'r', 1]],
        "Reject/capacity=2 must deliver exactly the first two messages"
    );

    let w = writer_slot.lock().unwrap().take().expect("handler ran");
    assert!(
        w.overflowed(),
        "rejected writes must set the sticky overflow flag"
    );
    assert!(!w.is_cancelled(), "QueueFull is not a cancellation");
}
