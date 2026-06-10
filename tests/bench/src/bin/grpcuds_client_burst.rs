// SPDX-License-Identifier: MIT OR Apache-2.0
//! Client-side burst memory: drain one `ScanResultStream` call (BENCH_STREAM_N
//! messages, default 50k) with the grpcuds blocking client, decoding every
//! message, then self-report memory. Usage: grpcuds_client_burst <sock>
use prost::Message;

use grpcuds_bench::ble::{ScanResult, ScanResultStreamRequest};
use grpcuds_bench::stream_n;

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

fn main() {
    let sock = std::env::args()
        .nth(1)
        .expect("usage: grpcuds_client_burst <sock>");
    let n = stream_n();

    let mut client = grpcuds::Client::connect(&sock).expect("connect");
    let req = ScanResultStreamRequest {}.encode_to_vec();
    let mut stream = client
        .server_streaming("/ble.BleService/ScanResultStream", &req)
        .expect("stream open");
    let mut count = 0usize;
    let mut rssi_sum = 0i64;
    while let Some(bytes) = stream.message().expect("stream msg") {
        let msg = ScanResult::decode(&bytes[..]).expect("decode");
        rssi_sum += msg.rssi as i64; // consume the message for real
        count += 1;
    }
    assert_eq!(count, n, "stream truncated");

    println!(
        "grpcuds_client_burst: drained={count} rssi_sum={rssi_sum} \
         pss_kb={} vmrss_kb={} vmhwm_kb={}",
        pss_kb(),
        proc_kb("/proc/self/status", "VmRSS:"),
        proc_kb("/proc/self/status", "VmHWM:"),
    );
}
