// SPDX-License-Identifier: MIT OR Apache-2.0
//! BLE — tonic (stock gRPC) server + grpcuds client (`cargo run -p ble-tg`).
use ble_domain::proto_grpcuds as pb;

fn main() {
    let sock = uds_harness::sock("ble-tg-demo");
    let server = ble_domain::spawn_tonic(&sock);
    uds_harness::wait_for_sock(&sock);

    let mut cli = pb::BleServiceClient::connect(&sock).unwrap();
    let init = cli.init(pb::InitRequest {}).unwrap();
    println!("Init -> ok={}", init.ok);

    {
        let mut st = cli
            .scan_result_stream(pb::ScanResultStreamRequest {})
            .unwrap();
        while let Some(m) = st.message().unwrap() {
            println!("scan {} rssi={}", m.mac, m.rssi);
        }
    }

    drop(cli);
    server.stop();
}
