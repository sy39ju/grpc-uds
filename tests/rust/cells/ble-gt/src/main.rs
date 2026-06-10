// SPDX-License-Identifier: MIT OR Apache-2.0
//! BLE — grpcuds server + tonic (stock gRPC) client (`cargo run -p ble-gt`).
use ble_domain::proto;

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let sock = uds_harness::sock("ble-gt-demo");
    let running = ble_domain::grpcuds_builder(&sock)
        .build()
        .unwrap()
        .run()
        .unwrap();

    let mut c = ble_domain::tonic_client(sock.clone()).await;
    let init = c.init(proto::InitRequest {}).await.unwrap().into_inner();
    println!("Init -> ok={}", init.ok);

    let mut stream = c
        .scan_result_stream(proto::ScanResultStreamRequest {})
        .await
        .unwrap()
        .into_inner();
    while let Some(m) = stream.message().await.unwrap() {
        println!("scan {} rssi={}", m.mac, m.rssi);
    }

    running.join().unwrap();
}
