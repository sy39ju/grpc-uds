// SPDX-License-Identifier: MIT OR Apache-2.0
//! BLE client, grpcuds (raw typed `Client`) — footprint comparison.
use ble_domain::{paths, proto};

fn main() {
    let sock = std::env::args().nth(1).unwrap();
    let mut cli = grpcuds::Client::connect(&sock).unwrap();
    let _: proto::InitReply = cli.unary_msg(paths::INIT, &proto::InitRequest {}).unwrap();
    {
        let mut st = cli
            .server_streaming_msg::<_, proto::ScanResult>(
                paths::SCAN_STREAM,
                &proto::ScanResultStreamRequest {},
            )
            .unwrap();
        while st.message().unwrap().is_some() {}
    }
    println!("READY");
    std::thread::sleep(std::time::Duration::from_secs(5));
}
