// SPDX-License-Identifier: MIT OR Apache-2.0
//! Stock-tooling conformance for the `health` feature: tonic-health's
//! generated client (the same stack grpc_health_probe-style probers use)
//! drives a grpcuds server's grpc.health.v1 service over UDS.
use grpcuds::health::{add_health_service, HealthReporter, ServingStatus};
use tonic_health::pb::health_client::HealthClient;
use tonic_health::pb::HealthCheckRequest;
use tonic_health::ServingStatus as TonicServing;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tonic_health_client_speaks_to_grpcuds_health() {
    let sock = uds_harness::sock("health-stock");
    let reporter = HealthReporter::new();
    let server = add_health_service(grpcuds::Server::builder().bind(&sock), &reporter)
        .build()
        .expect("build")
        .run()
        .expect("run");
    uds_harness::wait_for_sock(&sock);

    let channel = uds_harness::connect_uds(sock.clone()).await;
    let mut client = HealthClient::new(channel);

    // Overall server: SERVING.
    let resp = client
        .check(HealthCheckRequest {
            service: String::new(),
        })
        .await
        .expect("check")
        .into_inner();
    assert_eq!(resp.status, TonicServing::Serving as i32);

    // Unknown service: NOT_FOUND, exactly what stock probers expect.
    let err = client
        .check(HealthCheckRequest {
            service: "ghost.Service".into(),
        })
        .await
        .expect_err("unknown service must fail");
    assert_eq!(err.code(), tonic::Code::NotFound);

    // Watch: immediate status, then a live flip.
    reporter.set_status("ble.BleService", ServingStatus::Serving);
    let mut watch = client
        .watch(HealthCheckRequest {
            service: "ble.BleService".into(),
        })
        .await
        .expect("watch")
        .into_inner();
    let first = watch.message().await.expect("recv").expect("first");
    assert_eq!(first.status, TonicServing::Serving as i32);

    reporter.set_status("ble.BleService", ServingStatus::NotServing);
    let second = watch.message().await.expect("recv").expect("update");
    assert_eq!(second.status, TonicServing::NotServing as i32);

    drop(watch);
    drop(client);
    drop(server);
}
