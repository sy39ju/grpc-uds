// SPDX-License-Identifier: MIT OR Apache-2.0
//! BLE — grpcuds server + grpcuds client (run: `cargo run -p ble-gg`).
use ble_domain::proto_grpcuds as pb;

fn main() {
    let sock = uds_harness::sock("ble-gg-demo");
    let running = ble_domain::grpcuds_builder(&sock)
        .build()
        .unwrap()
        .run()
        .unwrap();

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
    running.join().unwrap();
}
