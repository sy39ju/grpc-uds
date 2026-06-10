// SPDX-License-Identifier: MIT OR Apache-2.0
//! End-to-end test for the C-ABI grpc.health.v1 service — in particular the
//! Watch status-change PUSH: a subscribed client receives a second message
//! after `grpcuds_health_set_status` flips the status from another thread.
//!
//! A C-ABI server runs in a background thread (the same poll/accept/tick/drain
//! shape as example/c/server.c); the main thread drives a grpcuds client and
//! flips the status. Requires the `client` feature for the client symbols.
#![allow(clippy::missing_safety_doc)]

use std::ffi::CString;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use grpcuds_ffi_impl::*;

const SERVING: i32 = 1;
const NOT_SERVING: i32 = 2;

fn unique_path() -> String {
    static N: AtomicU32 = AtomicU32::new(0);
    let p = format!(
        "/tmp/grpcuds-health-{}-{}.sock",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&p);
    p
}

/// A minimal C-ABI server: bind, register health, then poll/accept/tick/drain
/// until `stop`. Ticks every connection each iteration (simple, not efficient).
unsafe fn run_server(path: CString, stop: Arc<AtomicBool>) {
    let server = grpcuds_server_new();
    assert_eq!(grpcuds_server_bind_uds(server, path.as_ptr()), 0);
    assert_eq!(grpcuds_health_register(server), 0);
    grpcuds_mailbox_register_io_thread();
    let wakeup = grpcuds_mailbox_wakeup_fd();
    let listener = grpcuds_server_listener_fd(server);

    let mut conns: Vec<*mut grpcuds_conn> = Vec::new();
    while !stop.load(Ordering::Acquire) {
        let mut fds = std::vec![
            libc::pollfd {
                fd: listener,
                events: libc::POLLIN,
                revents: 0
            },
            libc::pollfd {
                fd: wakeup,
                events: libc::POLLIN,
                revents: 0
            },
        ];
        for c in &conns {
            fds.push(libc::pollfd {
                fd: grpcuds_conn_fd(*c),
                events: libc::POLLIN | libc::POLLOUT,
                revents: 0,
            });
        }
        libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, 50);
        grpcuds_mailbox_drain();

        if fds[0].revents & libc::POLLIN != 0 {
            loop {
                let c = grpcuds_server_accept(server);
                if c.is_null() {
                    break;
                }
                conns.push(c);
            }
        }
        let mut i = 0;
        while i < conns.len() {
            if grpcuds_conn_tick(conns[i]) != 0 {
                grpcuds_conn_free(conns[i]);
                conns.remove(i);
            } else {
                i += 1;
            }
        }
    }
    grpcuds_mailbox_drain();
    for c in conns {
        grpcuds_conn_tick(c);
        grpcuds_conn_free(c);
    }
    grpcuds_server_free(server);
}

unsafe fn connect_wait(path: &str) -> *mut grpcuds_client {
    let cpath = CString::new(path).unwrap();
    for _ in 0..300 {
        let c = grpcuds_client_connect(cpath.as_ptr());
        if !c.is_null() {
            return c;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("client never connected");
}

/// `HealthCheckRequest{service}` = 0x0A <len> <bytes>.
fn check_request(service: &str) -> Vec<u8> {
    let mut v = std::vec![0x0A, service.len() as u8];
    v.extend_from_slice(service.as_bytes());
    v
}

#[test]
fn watch_receives_status_change_push() {
    let path = unique_path();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_srv = stop.clone();
    let srv_path = CString::new(path.clone()).unwrap();
    let server = thread::spawn(move || unsafe { run_server(srv_path, stop_srv) });

    unsafe {
        let svc = CString::new("watched.svc").unwrap();
        // Register the watched service as SERVING before anyone subscribes.
        grpcuds_health_set_status(svc.as_ptr(), SERVING);

        let client = connect_wait(&path);

        // Subscribe: Watch{service="watched.svc"} — streams the current status,
        // then every change.
        let req = check_request("watched.svc");
        let wpath = CString::new("/grpc.health.v1.Health/Watch").unwrap();
        let stream =
            grpcuds_client_server_streaming(client, wpath.as_ptr(), req.as_ptr(), req.len());
        assert!(!stream.is_null());

        // Immediate status = SERVING (0x08 0x01).
        let mut len = 0usize;
        let m = grpcuds_stream_next(stream, &mut len);
        assert!(!m.is_null(), "no initial Watch message");
        assert_eq!(
            std::slice::from_raw_parts(m, len),
            &[0x08, 0x01],
            "initial SERVING"
        );

        // Flip the status from THIS (client) thread. The push rides the mailbox:
        // set_status enqueues to the watcher, the server's drain delivers it.
        grpcuds_health_set_status(svc.as_ptr(), NOT_SERVING);

        // The PUSH: a second message arrives with the new status.
        let m2 = grpcuds_stream_next(stream, &mut len);
        assert!(!m2.is_null(), "no pushed Watch update");
        assert_eq!(
            std::slice::from_raw_parts(m2, len),
            &[0x08, 0x02],
            "pushed NOT_SERVING"
        );

        grpcuds_stream_free(stream);
        grpcuds_client_free(client);
    }

    stop.store(true, Ordering::Release);
    server.join().unwrap();
    let _ = std::fs::remove_file(&path);
}
