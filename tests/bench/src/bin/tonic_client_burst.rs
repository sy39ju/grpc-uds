// SPDX-License-Identifier: MIT OR Apache-2.0
//! Client-side burst memory, tonic edition: identical drain to
//! grpcuds_client_burst (one ScanResultStream call, BENCH_STREAM_N messages),
//! then self-report memory. Usage: tonic_client_burst <sock>
use grpcuds_bench::ble::ble_service_client::BleServiceClient;
use grpcuds_bench::ble::ScanResultStreamRequest;
use grpcuds_bench::{channel_for, stream_n};

fn proc_kb(file: &str, key: &str) -> u64 {
    std::fs::read_to_string(file)
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with(key))
                .and_then(|l| l.split_whitespace().nth(1)?.parse().ok())
        })
        .unwrap_or(0)
}

fn pss_kb() -> u64 {
    std::fs::read_to_string("/proc/self/smaps_rollup")
        .ok()
        .map(|s| {
            s.lines()
                .filter(|l| l.starts_with("Pss:"))
                .filter_map(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
                .sum()
        })
        .unwrap_or(0)
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let sock = std::env::args()
        .nth(1)
        .expect("usage: tonic_client_burst <sock>");
    let n = stream_n();

    let channel = channel_for(sock).await.expect("connect");
    let mut client = BleServiceClient::new(channel);
    let mut stream = client
        .scan_result_stream(ScanResultStreamRequest {})
        .await
        .expect("stream open")
        .into_inner();
    let mut count = 0usize;
    let mut rssi_sum = 0i64;
    while let Some(msg) = stream.message().await.expect("stream msg") {
        rssi_sum += msg.rssi as i64; // consume the message for real
        count += 1;
    }
    assert_eq!(count, n, "stream truncated");

    println!(
        "tonic_client_burst: drained={count} rssi_sum={rssi_sum} \
         pss_kb={} vmrss_kb={} vmhwm_kb={}",
        pss_kb(),
        proc_kb("/proc/self/status", "VmRSS:"),
        proc_kb("/proc/self/status", "VmHWM:"),
    );
}
