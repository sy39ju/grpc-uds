// SPDX-License-Identifier: MIT OR Apache-2.0
//! BLE — grpcuds server + tonic (stock gRPC) client.
use ble_domain::{expect, proto};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ble_grpcuds_to_tonic() {
    let sock = uds_harness::sock("ble-gt");
    let running = ble_domain::grpcuds_builder(&sock)
        .build()
        .unwrap()
        .run()
        .unwrap();
    let mut c = ble_domain::tonic_client(sock.clone()).await;

    assert!(c.init(proto::InitRequest {}).await.unwrap().into_inner().ok);

    let mut stream = c
        .scan_result_stream(proto::ScanResultStreamRequest {})
        .await
        .unwrap()
        .into_inner();
    let mut got = Vec::new();
    while let Some(m) = stream.message().await.unwrap() {
        got.push((m.mac, m.rssi, m.adv_data));
    }
    assert_eq!(got, expect::ble_scan());

    let err = c
        .remove_scan_filter(proto::RemoveScanFilterRequest { filter_id: 99 })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::NotFound);

    running.join().unwrap();
}
