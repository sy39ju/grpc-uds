// SPDX-License-Identifier: MIT OR Apache-2.0
//! BLE bench server on the grpcuds safe Rust wrapper (byte-level handlers +
//! prost encode/decode in the handler, single-threaded poll loop).

use std::sync::atomic::{AtomicBool, Ordering};

use grpcuds::{Server, Status, StatusCode};
use grpcuds_bench::ble::{InitReply, ScanResultStreamRequest};
use grpcuds_bench::{sample_result, stream_n};
use prost::Message;

// SIGTERM → clean serve() exit, so leak checkers (valgrind/ASan) get a
// normal process exit and can produce their end-of-run report.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn on_term(_sig: i32) {
    SHUTDOWN.store(true, Ordering::Relaxed); // async-signal-safe
}

fn main() {
    let sock = std::env::args()
        .nth(1)
        .expect("usage: grpcuds_server <uds-path>");
    let _ = std::fs::remove_file(&sock);
    let n = stream_n();

    // BENCH_TRIM_SEC=<s>: every s seconds, hand the allocator's retained
    // free memory back to the OS (glibc-only). Demonstrates the idle-trim
    // pattern an embedded app can adopt after bursty workloads.
    if let Some(secs) = std::env::var("BENCH_TRIM_SEC")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        extern "C" {
            fn malloc_trim(pad: usize) -> std::os::raw::c_int;
        }
        std::thread::spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_secs(secs));
            unsafe { malloc_trim(0) };
        });
    }

    let b = Server::builder().bind(&sock);

    let b = b.add_unary("/ble.BleService/Init", |_req: &[u8]| {
        Ok(InitReply { ok: true }.encode_to_vec())
    });

    let b = b.add_server_streaming("/ble.BleService/ScanResultStream", move |req: &[u8], w| {
        if ScanResultStreamRequest::decode(req).is_err() {
            return Status::code_only(StatusCode::Internal);
        }
        for i in 0..n {
            if w.write_owned(sample_result(i).encode_to_vec()).is_err() {
                return Status::ok(); // client gone; stream already dead
            }
        }
        let _ = w.finish(Status::ok());
        Status::ok()
    });

    unsafe {
        libc_signal(15, on_term as *const () as usize); // SIGTERM
    }

    let server = b.build().expect("bind");
    eprintln!("grpcuds_server ready on {sock}");
    server.serve(&SHUTDOWN).expect("serve");
    eprintln!("grpcuds_server: clean shutdown");
}

unsafe fn libc_signal(sig: i32, handler: usize) {
    extern "C" {
        fn signal(sig: i32, handler: usize) -> usize;
    }
    signal(sig, handler);
}
