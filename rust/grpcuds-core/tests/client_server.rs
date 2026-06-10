// SPDX-License-Identifier: MIT OR Apache-2.0
//! no_std core client (ClientConn) against the core server stack, in-process.
//! Requires both `server` and `client` features.
#![cfg(all(feature = "server", feature = "client"))]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use grpcuds_core::client::ClientConn;
use grpcuds_core::{Conn, GrpcStatus, Listener, TickStatus};
use std::ffi::c_void;

static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
fn sock() -> String {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    format!("/tmp/grpcuds-core-cs-{}-{}.sock", std::process::id(), n)
}

// A trivial echo handler registered on each accepted connection.
unsafe extern "C" fn echo(
    conn: *mut Conn,
    call_id: i32,
    req: *const u8,
    req_len: usize,
    _ud: *mut c_void,
) -> i32 {
    let c = &mut *conn;
    let r = core::slice::from_raw_parts(req, req_len);
    let _ = c.write_call(call_id, r);
    let _ = c.finish_call(call_id, GrpcStatus::Ok);
    0
}

struct Server {
    shutdown: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn serve(path: &str) -> Server {
    let listener = Listener::bind(path.as_bytes()).ok().expect("bind");
    let shutdown = Arc::new(AtomicBool::new(false));
    let sd = shutdown.clone();
    let handle = thread::spawn(move || {
        let mut conns: Vec<grpcuds_core::Connection> = Vec::new();
        while !sd.load(Ordering::Relaxed) {
            if let Ok(Some(mut conn)) = listener.accept() {
                let ud = core::ptr::null_mut();
                let _ = conn.conn().register_method(b"/echo.Echo/Unary", echo, ud);
                conns.push(conn);
            }
            conns.retain_mut(|c| matches!(c.tick(), Ok(TickStatus::Live)));
            thread::sleep(Duration::from_millis(1));
        }
    });
    Server {
        shutdown,
        handle: Some(handle),
    }
}

#[test]
fn core_client_unary() {
    let path = sock();
    let _srv = serve(&path);
    thread::sleep(Duration::from_millis(50));
    let mut client = ClientConn::connect(path.as_bytes()).expect("connect");
    let mut call = client.unary(b"/echo.Echo/Unary", b"hello").expect("unary");
    assert_eq!(call.status(), GrpcStatus::Ok);
    let msg = call.recv().expect("recv").expect("one message");
    assert_eq!(msg, b"hello");
    assert!(call.recv().expect("end").is_none());
}
