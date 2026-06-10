// SPDX-License-Identifier: MIT OR Apache-2.0
//! BLE cross-language: Rust tonic peer ⇄ C++ grpcuds binary.
use ble_domain::{expect, proto};

/// gt: C++ grpcuds **server** ← Rust tonic **client**.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ble_gt_cpp_server() {
    let Some(bin) = cross::cpp_bin("BLE_GT_SERVER_BIN", "ble/ble-gt-server") else {
        eprintln!("skipping ble_gt: C++ ble-gt-server not built");
        return;
    };
    let sock = uds_harness::sock("ble-gt-cross");
    let _guard = uds_harness::cpp::spawn_server(&bin, &sock); // waits for READY

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
}

/// tg: Rust tonic **server** ← C++ grpcuds **client** (self-checking, exit 0).
#[test]
fn ble_tg_cpp_client() {
    let Some(bin) = cross::cpp_bin("BLE_TG_CLIENT_BIN", "ble/ble-tg-client") else {
        eprintln!("skipping ble_tg: C++ ble-tg-client not built");
        return;
    };
    let sock = uds_harness::sock("ble-tg-cross");
    let server = ble_domain::spawn_tonic(&sock);
    uds_harness::wait_for_sock(&sock);

    let ok = uds_harness::cpp::run_client(&bin, &sock);
    assert!(ok, "C++ ble-tg-client self-check failed");

    server.stop();
}
