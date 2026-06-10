// SPDX-License-Identifier: MIT OR Apache-2.0
//! Hold N connections, each with one server-streaming call mid-flight (one
//! message read, then no more pumping), so the server-side per-connection
//! heap — nghttp2 session + stream state + outbound queue — stays resident.
//! Used by measure_footprint.sh to derive heap-per-active-connection from
//! the server's RSS delta. Usage: conn_hold <sock> <n> [hold-secs]
fn main() {
    let sock = std::env::args()
        .nth(1)
        .expect("usage: conn_hold <sock> <n>");
    let n: usize = std::env::args().nth(2).expect("n").parse().expect("n");
    let secs: u64 = std::env::args()
        .nth(3)
        .map(|s| s.parse().expect("secs"))
        .unwrap_or(10);
    let mut held = Vec::new();
    for _ in 0..n {
        let mut c = grpcuds::Client::connect(&sock).unwrap();
        {
            let mut st = c
                .server_streaming("/ble.BleService/ScanResultStream", &[])
                .unwrap();
            let _ = st.message().unwrap();
        }
        held.push(c);
    }
    println!("HELD {n}");
    std::thread::sleep(std::time::Duration::from_secs(secs));
    drop(held);
}
