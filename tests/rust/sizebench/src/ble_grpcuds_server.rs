// SPDX-License-Identifier: MIT OR Apache-2.0
//! BLE server, grpcuds — one side of the footprint comparison.
fn main() {
    let sock = std::env::args().nth(1).unwrap();
    let _ = std::fs::remove_file(&sock);
    let _running = ble_domain::grpcuds_builder(&sock)
        .build()
        .unwrap()
        .run()
        .unwrap();
    println!("READY");
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
