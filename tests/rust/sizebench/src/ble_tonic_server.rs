// SPDX-License-Identifier: MIT OR Apache-2.0
//! BLE server, tonic — the stock-gRPC side of the footprint comparison.
fn main() {
    let sock = std::env::args().nth(1).unwrap();
    let _server = ble_domain::spawn_tonic(&sock);
    uds_harness::wait_for_sock(&sock);
    println!("READY");
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
