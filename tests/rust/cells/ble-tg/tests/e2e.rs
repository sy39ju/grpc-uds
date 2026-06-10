// SPDX-License-Identifier: MIT OR Apache-2.0
//! BLE — tonic (stock gRPC) server + grpcuds client (the generated
//! `BleServiceClient`).
use ble_domain::{expect, proto_grpcuds as pb};

#[test]
fn ble_tonic_to_grpcuds() {
    let sock = uds_harness::sock("ble-tg");
    let server = ble_domain::spawn_tonic(&sock);
    uds_harness::wait_for_sock(&sock);
    let mut cli = pb::BleServiceClient::connect(&sock).unwrap();

    let r = cli.init(pb::InitRequest {}).unwrap();
    assert!(r.ok);

    let mut got = Vec::new();
    {
        let mut st = cli
            .scan_result_stream(pb::ScanResultStreamRequest {})
            .unwrap();
        while let Some(m) = st.message().unwrap() {
            got.push((m.mac, m.rssi, m.adv_data));
        }
    }
    assert_eq!(got, expect::ble_scan());

    let err = cli
        .remove_scan_filter(pb::RemoveScanFilterRequest { filter_id: 99 })
        .unwrap_err();
    assert_eq!(err.code(), grpcuds::StatusCode::NotFound);

    drop(cli);
    server.stop();
}
