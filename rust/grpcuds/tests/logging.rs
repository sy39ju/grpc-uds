// SPDX-License-Identifier: MIT OR Apache-2.0
//! End-to-end proof for the log facility: a real server⇄client flow with
//! a sink installed must surface the documented events — and the payload
//! shape (static message + numeric arg) must hold.

#![cfg(feature = "client")]

use std::sync::Mutex;
use std::time::Duration;

use grpcuds::{Client, LogLevel, Server};

mod common;
use common::unique_path;

static EVENTS: Mutex<Vec<(LogLevel, String, i64)>> = Mutex::new(Vec::new());

fn has_event(msg: &str) -> bool {
    EVENTS.lock().unwrap().iter().any(|(_, m, _)| m == msg)
}

#[test]
fn flows_surface_the_documented_events() {
    assert!(grpcuds::set_logger(LogLevel::Debug, |level, msg, arg| {
        EVENTS.lock().unwrap().push((level, msg.to_string(), arg));
    }));
    assert!(
        !grpcuds::set_logger(LogLevel::Debug, |_, _, _| {}),
        "second installation must be refused"
    );

    let sock = unique_path();
    let srv = Server::builder()
        .bind(&sock)
        .add_unary("/echo.Echo/Unary", |req: &[u8]| Ok(req.to_vec()))
        .add_server_streaming("/hang.Hang/Forever", |_req: &[u8], _w| {
            grpcuds::Status::ok() // parked: deadline will expire it
        })
        .build()
        .expect("build")
        .run()
        .expect("run");

    let mut client = Client::connect_wait(&sock, Duration::from_secs(3)).expect("connect");

    // Unimplemented method -> INFO with the stream id as the argument.
    client
        .unary("/echo.Echo/Missing", b"x")
        .expect_err("unimplemented");
    assert!(has_event("unimplemented method called"));

    // Lifecycle events: asserted AFTER a full round trip — connect(2)
    // returns on backlog queueing, before the server thread accept()s.
    assert!(has_event("server listening"));
    assert!(has_event("connection accepted"));

    // Deadline expiry, both sides: the client sends grpc-timeout, the
    // server expires the dispatched call, the client expires locally.
    client.set_timeout(Some(Duration::from_millis(150)));
    client
        .unary("/hang.Hang/Forever", b"x")
        .expect_err("deadline");
    client.set_timeout(None);
    assert!(has_event("call deadline expired") || has_event("call deadline exceeded"));

    // Server restart -> EOF/broken events + lazy reconnect events.
    drop(srv);
    let _ = client.unary("/echo.Echo/Unary", b"down");
    let _srv2 = Server::builder()
        .bind(&sock)
        .add_unary("/echo.Echo/Unary", |req: &[u8]| Ok(req.to_vec()))
        .build()
        .expect("build")
        .run()
        .expect("run");
    assert_eq!(
        client.unary("/echo.Echo/Unary", b"two").expect("reconnect"),
        b"two"
    );
    // NOTE: the safe-Rust Client is its own std implementation — the
    // reconnect events here come from the shared core only when the C ABI
    // client is used. What MUST appear from the safe client path is the
    // second server's lifecycle:
    let listening_count = EVENTS
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, m, _)| m == "server listening")
        .count();
    assert!(listening_count >= 2, "both server starts logged");

    // Payload shape: every event has a non-empty static message.
    assert!(EVENTS.lock().unwrap().iter().all(|(_, m, _)| !m.is_empty()));
}
