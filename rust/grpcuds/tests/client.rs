// SPDX-License-Identifier: MIT OR Apache-2.0
//! The grpcuds Client against a grpcuds Server, in-process — proving both
//! sides of the wire without any external dependency. Requires both the
//! `server` and `client` features.

use std::time::Duration;

use grpcuds::{Client, Server, Status, StatusCode};

mod common;
use common::unique_path;

fn echo_server(sock: &str) -> grpcuds::Running {
    Server::builder()
        .bind(sock)
        .add_unary("/echo.Echo/Unary", |req: &[u8]| Ok(req.to_vec()))
        .add_unary("/echo.Echo/NonEmpty", |req: &[u8]| {
            if req.is_empty() {
                Err(Status::invalid_argument("request must not be empty"))
            } else {
                Ok(req.to_vec())
            }
        })
        .add_server_streaming("/echo.Echo/Stream", |req: &[u8], w| {
            for i in 0u8..3 {
                let mut m = req.to_vec();
                m.push(i);
                if w.write(&m).is_err() {
                    return Status::code_only(StatusCode::Aborted);
                }
            }
            let _ = w.finish(Status::ok());
            Status::ok()
        })
        .build()
        .expect("build")
        .run()
        .expect("run")
}

fn connect(sock: &str) -> Client {
    // connect_wait IS the retry loop this helper used to hand-roll.
    Client::connect_wait(sock, Duration::from_secs(3)).expect("connect")
}

#[test]
fn unary_round_trip() {
    let sock = unique_path();
    let _srv = echo_server(&sock);
    let mut client = connect(&sock);

    let reply = client.unary("/echo.Echo/Unary", b"hello").expect("unary");
    assert_eq!(reply, b"hello");
    let reply = client.unary("/echo.Echo/Unary", b"again").expect("unary 2");
    assert_eq!(reply, b"again");
}

#[test]
fn unary_error_status() {
    let sock = unique_path();
    let _srv = echo_server(&sock);
    let mut client = connect(&sock);

    let err = client
        .unary("/echo.Echo/NonEmpty", b"")
        .expect_err("empty must fail");
    assert_eq!(err.code(), StatusCode::InvalidArgument);
    assert_eq!(err.message(), Some("request must not be empty"));

    let ok = client.unary("/echo.Echo/NonEmpty", b"x").expect("recover");
    assert_eq!(ok, b"x");
}

#[test]
fn server_streaming() {
    let sock = unique_path();
    let _srv = echo_server(&sock);
    let mut client = connect(&sock);

    let mut stream = client
        .server_streaming("/echo.Echo/Stream", b"m")
        .expect("stream");
    let mut got = Vec::new();
    while let Some(msg) = stream.message().expect("message") {
        got.push(msg);
    }
    assert_eq!(got, vec![vec![b'm', 0], vec![b'm', 1], vec![b'm', 2]]);
}

#[test]
fn unimplemented_path() {
    let sock = unique_path();
    let _srv = echo_server(&sock);
    let mut client = connect(&sock);

    let err = client
        .unary("/echo.Echo/Missing", b"x")
        .expect_err("unknown method");
    assert_eq!(err.code(), StatusCode::Unimplemented);
}

// ---- client-side deadlines ----------------------------------------------------

/// A server whose handler parks the call forever (returns OK, never
/// finishes) — the shape of a hung/long-running backend.
fn hang_server(sock: &str) -> grpcuds::Running {
    Server::builder()
        .bind(sock)
        .add_server_streaming("/hang.Hang/Forever", |_req: &[u8], _w| {
            // Keep the stream open: no write, no finish.
            Status::ok()
        })
        .build()
        .expect("build")
        .run()
        .expect("run")
}

#[test]
fn unary_times_out_with_deadline_exceeded() {
    let sock = unique_path();
    let server = hang_server(&sock);
    let mut client = connect(&sock);
    client.set_timeout(Some(Duration::from_millis(200)));

    let started = std::time::Instant::now();
    let err = client.unary("/hang.Hang/Forever", b"x").unwrap_err();
    let elapsed = started.elapsed();

    assert_eq!(err.code(), StatusCode::DeadlineExceeded);
    assert!(
        elapsed >= Duration::from_millis(150) && elapsed < Duration::from_secs(3),
        "timeout fired at {elapsed:?}, expected ~200ms"
    );
    drop(client);
    drop(server);
}

#[test]
fn streaming_times_out_mid_stream() {
    let sock = unique_path();
    let server = hang_server(&sock);
    let mut client = connect(&sock);
    client.set_timeout(Some(Duration::from_millis(200)));

    let mut stream = client
        .server_streaming("/hang.Hang/Forever", b"x")
        .expect("submit");
    let err = stream.message().unwrap_err();
    assert_eq!(err.code(), StatusCode::DeadlineExceeded);
    drop(client);
    drop(server);
}

#[test]
fn timeout_clears_and_later_calls_succeed() {
    let sock = unique_path();
    let server = echo_server(&sock);
    let mut client = connect(&sock);

    // A generous timeout does not interfere with a fast call...
    client.set_timeout(Some(Duration::from_secs(5)));
    assert_eq!(client.unary("/echo.Echo/Unary", b"hi").unwrap(), b"hi");

    // ...and clearing it restores wait-forever semantics.
    client.set_timeout(None);
    assert_eq!(client.unary("/echo.Echo/Unary", b"yo").unwrap(), b"yo");
    drop(client);
    drop(server);
}

/// Deadline coherence end to end: when the call expires, the server-side
/// producer's writer observes closure (the cancel propagated — by the
/// server's own grpc-timeout expiry and/or the client's RST), so deferred
/// work stops instead of writing into the void.
#[test]
fn expired_call_closes_the_server_side_writer() {
    use std::sync::{Arc, Mutex};
    let sock = unique_path();
    let writer_slot: Arc<Mutex<Option<grpcuds::ServerWriter>>> = Arc::new(Mutex::new(None));
    let remaining_slot: Arc<Mutex<Option<Option<Duration>>>> = Arc::new(Mutex::new(None));
    let slot = writer_slot.clone();
    let rem = remaining_slot.clone();
    let server = Server::builder()
        .bind(&sock)
        .add_server_streaming("/hang.Hang/Forever", move |_req: &[u8], w| {
            // The handler can read the client's grpc-timeout budget.
            *rem.lock().unwrap() = Some(w.time_remaining());
            *slot.lock().unwrap() = Some(w.clone());
            Status::ok() // deferred: never finishes on its own
        })
        .build()
        .expect("build")
        .run()
        .expect("run");

    let mut client = connect(&sock);
    client.set_timeout(Some(Duration::from_millis(200)));
    let err = client.unary("/hang.Hang/Forever", b"x").unwrap_err();
    assert_eq!(err.code(), StatusCode::DeadlineExceeded);

    // The handler saw the 200ms grpc-timeout budget the client sent.
    let seen = remaining_slot.lock().unwrap().take().expect("handler ran");
    let seen = seen.expect("client timeout must arrive as a server-side deadline");
    assert!(
        seen > Duration::ZERO && seen <= Duration::from_millis(200),
        "budget visible to the handler: {seen:?}"
    );

    // The held writer must become unusable shortly after expiry.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        let closed = writer_slot
            .lock()
            .unwrap()
            .as_ref()
            .map(|w| w.write(b"late").is_err())
            .unwrap_or(false);
        if closed {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "writer never observed the cancelled call"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(client);
    drop(server);
}

// ---- connection lifecycle: connect_wait + lazy reconnect ---------------------

/// connect_wait rides out the startup race: the client starts FIRST, the
/// server appears later, and the call still goes through — no hand-rolled
/// retry loop in the application.
#[test]
fn connect_wait_rides_out_late_server_start() {
    let sock = unique_path();
    let sock2 = sock.clone();
    let srv = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        echo_server(&sock2)
    });

    let t0 = std::time::Instant::now();
    let mut client =
        Client::connect_wait(&sock, Duration::from_secs(3)).expect("server appears late");
    assert!(t0.elapsed() >= Duration::from_millis(100), "had to wait");

    let reply = client.unary("/echo.Echo/Unary", b"late").expect("unary");
    assert_eq!(reply, b"late");
    drop(client);
    drop(srv.join().expect("server thread"));
}

/// A zero timeout is exactly one attempt — the documented contract.
#[test]
fn connect_wait_zero_timeout_fails_fast_without_a_server() {
    let sock = unique_path();
    let t0 = std::time::Instant::now();
    assert!(Client::connect_wait(&sock, Duration::ZERO).is_err());
    assert!(t0.elapsed() < Duration::from_millis(100));
}

/// The daemon-restart story: the call that hits the dead connection fails
/// (its stream is gone — nothing can save it), but the NEXT call finds the
/// restarted server through the lazy reconnect. No client recreation.
#[test]
fn client_reconnects_after_server_restart() {
    let sock = unique_path();
    let srv = echo_server(&sock);
    let mut client = connect(&sock);
    assert_eq!(
        client.unary("/echo.Echo/Unary", b"one").expect("first"),
        b"one"
    );

    drop(srv); // the daemon dies
    client
        .unary("/echo.Echo/Unary", b"down")
        .expect_err("the dead connection must surface an error");

    let _srv2 = echo_server(&sock); // ...and restarts on the same path
    assert_eq!(
        client
            .unary("/echo.Echo/Unary", b"two")
            .expect("reconnected"),
        b"two",
        "the same Client object must work across the restart"
    );
}

// ---- abusive peer behavior: server resource reclaim ------------------------

/// A client that opens a server-stream and then DROPS mid-call (no clean
/// finish, no deadline — the abrupt-disconnect path: socket close → RST /
/// EOF) must be observed server-side: the held writer goes unusable, so the
/// producer stops and the per-call state is reclaimed. This is the
/// server-memory-safety guarantee under client churn, asserted in-process.
#[test]
fn abrupt_client_drop_reclaims_the_server_stream() {
    use std::sync::{Arc, Mutex};
    let sock = unique_path();
    let writer_slot: Arc<Mutex<Option<grpcuds::ServerWriter>>> = Arc::new(Mutex::new(None));
    let slot = writer_slot.clone();
    let server = Server::builder()
        .bind(&sock)
        .add_server_streaming("/scan.Scan/Stream", move |_req: &[u8], w| {
            *slot.lock().unwrap() = Some(w.clone()); // park the call open
            Status::ok()
        })
        .build()
        .expect("build")
        .run()
        .expect("run");

    {
        let mut client = connect(&sock);
        // Open the stream but DO NOT read — a parked server sends nothing,
        // so message() would block forever (the no-deadline contract). The
        // submit itself reaches the server; wait for the handler to park by
        // observing the writer slot, then drop the client abruptly.
        let _stream = client
            .server_streaming("/scan.Scan/Stream", b"x")
            .expect("open stream");
        let armed = std::time::Instant::now() + Duration::from_secs(3);
        while writer_slot.lock().unwrap().is_none() {
            assert!(std::time::Instant::now() < armed, "handler never ran");
            std::thread::sleep(Duration::from_millis(10));
        }
        // No finish, no deadline — drop the whole client abruptly.
        drop(_stream);
        drop(client);
    }

    // The server must notice the gone peer and retire the writer; otherwise
    // a producer would write into a freed call. Bounded wait.
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        let gone = writer_slot
            .lock()
            .unwrap()
            .as_ref()
            .map(|w| w.write(b"late").is_err())
            .unwrap_or(false);
        if gone {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "server never reclaimed the abruptly-dropped stream"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(server);
}

/// Repeated connect → call → abrupt drop must leave the server fully
/// healthy: if dead connections were not reaped (fd or per-call leak), the
/// server would eventually stop answering. After many churn cycles a fresh
/// client must still get a correct reply. (RSS is verified out-of-process
/// in tests/bench; this is the in-process regression net.)
#[test]
fn client_churn_leaves_the_server_healthy() {
    let sock = unique_path();
    let server = echo_server(&sock);

    for _ in 0..300 {
        let mut client = connect(&sock);
        // Fire a call, then drop the client mid-life without draining.
        let _ = client.unary("/echo.Echo/Unary", b"churn");
        drop(client);
    }

    // The server still answers correctly after the churn.
    let mut client = connect(&sock);
    assert_eq!(
        client
            .unary("/echo.Echo/Unary", b"alive")
            .expect("post-churn"),
        b"alive"
    );
    drop(client);
    drop(server);
}

/// Server never responds (handler parks) and the client sets a deadline:
/// the call must fail with DEADLINE_EXCEEDED in roughly the budget, NOT
/// block forever. (The no-deadline case blocks by design — that is the
/// caller's contract, documented; not asserted here because a hang has no
/// bounded test.)
#[test]
fn no_server_response_with_deadline_does_not_hang() {
    let sock = unique_path();
    let server = hang_server(&sock);
    let mut client = connect(&sock);
    client.set_timeout(Some(Duration::from_millis(300)));

    let t0 = std::time::Instant::now();
    let err = client.unary("/hang.Hang/Forever", b"x").unwrap_err();
    let elapsed = t0.elapsed();

    assert_eq!(err.code(), StatusCode::DeadlineExceeded);
    assert!(
        elapsed >= Duration::from_millis(250) && elapsed < Duration::from_secs(2),
        "deadline fired at {elapsed:?}, expected ~300ms (not a hang)"
    );
    drop(client);
    drop(server);
}
