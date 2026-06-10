// SPDX-License-Identifier: MIT OR Apache-2.0
//! grpc.health.v1 (the `health` feature) end to end over a real socket:
//! the protocol semantics a stock health prober relies on.
#![cfg(all(feature = "health", feature = "client"))]

use std::time::Duration;

use grpcuds::health::{add_health_service, pb, HealthReporter, ServingStatus};
use grpcuds::{Server, StatusCode};

mod common;
use common::unique_path;

fn connect(sock: &str) -> grpcuds::Client {
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        match grpcuds::Client::connect(sock) {
            Ok(c) => return c,
            Err(_) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10))
            }
            Err(e) => panic!("connect: {e}"),
        }
    }
}

#[test]
fn check_follows_the_health_protocol() {
    let sock = unique_path();
    let reporter = HealthReporter::new();
    let server = add_health_service(Server::builder().bind(&sock), &reporter)
        .build()
        .expect("build")
        .run()
        .expect("run");
    let mut client = connect(&sock);

    // "" (overall) starts SERVING.
    let resp: pb::HealthCheckResponse = client
        .unary_msg(
            "/grpc.health.v1.Health/Check",
            &pb::HealthCheckRequest::default(),
        )
        .unwrap();
    assert_eq!(resp.status, ServingStatus::Serving as i32);

    // Unregistered names fail with NOT_FOUND (the spec'd behavior).
    let err = client
        .unary_msg::<_, pb::HealthCheckResponse>(
            "/grpc.health.v1.Health/Check",
            &pb::HealthCheckRequest {
                service: "no.such.Service".into(),
            },
        )
        .unwrap_err();
    assert_eq!(err.code(), StatusCode::NotFound);

    // Registered services report their own status, including NOT_SERVING.
    reporter.set_status("ble.BleService", ServingStatus::NotServing);
    let resp: pb::HealthCheckResponse = client
        .unary_msg(
            "/grpc.health.v1.Health/Check",
            &pb::HealthCheckRequest {
                service: "ble.BleService".into(),
            },
        )
        .unwrap();
    assert_eq!(resp.status, ServingStatus::NotServing as i32);

    drop(client);
    drop(server);
}

#[test]
fn watch_streams_the_current_status_then_changes() {
    let sock = unique_path();
    let reporter = HealthReporter::new();
    let server = add_health_service(Server::builder().bind(&sock), &reporter)
        .build()
        .expect("build")
        .run()
        .expect("run");
    let mut client = connect(&sock);

    let mut stream = client
        .server_streaming_msg::<_, pb::HealthCheckResponse>(
            "/grpc.health.v1.Health/Watch",
            &pb::HealthCheckRequest {
                service: "svc".into(),
            },
        )
        .unwrap();

    // Immediate answer: not registered yet -> SERVICE_UNKNOWN.
    let first = stream.message().unwrap().expect("immediate status");
    assert_eq!(first.status, ServingStatus::ServiceUnknown as i32);

    // Registration and flips arrive as stream updates, in order.
    reporter.set_status("svc", ServingStatus::Serving);
    let up = stream.message().unwrap().expect("serving update");
    assert_eq!(up.status, ServingStatus::Serving as i32);

    reporter.set_status("svc", ServingStatus::NotServing);
    let down = stream.message().unwrap().expect("not-serving update");
    assert_eq!(down.status, ServingStatus::NotServing as i32);

    drop(client);
    drop(server);
}

/// A watcher whose client went away is pruned on the next status change —
/// the reporter must not grow forever or panic on dead streams, and live
/// watchers keep receiving.
#[test]
fn dead_watchers_are_pruned_and_live_ones_keep_receiving() {
    let sock = unique_path();
    let reporter = HealthReporter::new();
    let server = add_health_service(Server::builder().bind(&sock), &reporter)
        .build()
        .expect("build")
        .run()
        .expect("run");

    reporter.set_status("svc", ServingStatus::Serving);

    // Watcher A subscribes, then its whole connection goes away.
    {
        let mut dead_client = connect(&sock);
        let mut s = dead_client
            .server_streaming_msg::<_, pb::HealthCheckResponse>(
                "/grpc.health.v1.Health/Watch",
                &pb::HealthCheckRequest {
                    service: "svc".into(),
                },
            )
            .unwrap();
        let _ = s.message().unwrap();
    } // client dropped: connection closed

    // Watcher B stays live.
    let mut live_client = connect(&sock);
    let mut live = live_client
        .server_streaming_msg::<_, pb::HealthCheckResponse>(
            "/grpc.health.v1.Health/Watch",
            &pb::HealthCheckRequest {
                service: "svc".into(),
            },
        )
        .unwrap();
    let _ = live.message().unwrap(); // immediate status

    // Give the I/O loop a beat to reap the dead connection, then flip —
    // twice: the first send to the dead writer fails and prunes it.
    std::thread::sleep(Duration::from_millis(50));
    reporter.set_status("svc", ServingStatus::NotServing);
    let upd = live.message().unwrap().expect("live watcher still fed");
    assert_eq!(upd.status, ServingStatus::NotServing as i32);

    reporter.set_status("svc", ServingStatus::Serving);
    let upd = live.message().unwrap().expect("subsequent updates flow");
    assert_eq!(upd.status, ServingStatus::Serving as i32);

    drop(live_client);
    drop(server);
}
