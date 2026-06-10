// SPDX-License-Identifier: MIT OR Apache-2.0
//! BLE client, tonic — the stock-gRPC side of the footprint comparison.
use ble_domain::proto;

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let sock = std::env::args().nth(1).unwrap();
    let mut c = ble_domain::tonic_client(sock).await;
    let _ = c.init(proto::InitRequest {}).await.unwrap();
    let mut s = c
        .scan_result_stream(proto::ScanResultStreamRequest {})
        .await
        .unwrap()
        .into_inner();
    while s.message().await.unwrap().is_some() {}
    println!("READY");
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
}
