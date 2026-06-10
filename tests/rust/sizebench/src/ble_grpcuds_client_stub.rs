// SPDX-License-Identifier: MIT OR Apache-2.0
//! BLE client, grpcuds (generated `BleServiceClient` stub) — measures the
//! stub's footprint delta vs the raw typed `Client` (ble_grpcuds_client.rs).
use ble_domain::proto_grpcuds as pb;

fn main() {
    let sock = std::env::args().nth(1).unwrap();
    let mut cli = pb::BleServiceClient::connect(&sock).unwrap();
    let _ = cli.init(pb::InitRequest {}).unwrap();
    {
        let mut st = cli
            .scan_result_stream(pb::ScanResultStreamRequest {})
            .unwrap();
        while st.message().unwrap().is_some() {}
    }
    println!("READY");
    std::thread::sleep(std::time::Duration::from_secs(5));
}
