// SPDX-License-Identifier: MIT OR Apache-2.0
//! Bench orchestrator: spawns each server binary, drives it with the same
//! tonic client over UDS, and prints a markdown summary.
//!
//!   cargo build --release && ./target/release/runner
//!
//! Measures, per server:
//!   - unary    : p50 / p99 / mean latency + RPS (sequential, single conn)
//!   - streaming: msgs/s and MB/s draining one ScanResultStream call
//!   - RSS      : server VmRSS after the workload
//!   - size     : stripped release binary size on disk

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use grpcuds_bench::ble::ble_service_client::BleServiceClient;
use grpcuds_bench::ble::{InitRequest, ScanResultStreamRequest};
use grpcuds_bench::{channel_for, stream_n};

const UNARY_WARMUP: usize = 500;
const STREAM_RUNS: usize = 3;

fn unary_n() -> usize {
    std::env::var("BENCH_UNARY_N")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5_000)
}

struct ServerUnderTest {
    name: &'static str,
    bin: PathBuf,
    child: Child,
    sock: String,
}

impl ServerUnderTest {
    fn spawn(name: &'static str, bin_dir: &std::path::Path) -> Self {
        let bin = bin_dir.join(name);
        let sock = format!("/tmp/grpcuds-bench-{}-{}.sock", name, std::process::id());
        let _ = std::fs::remove_file(&sock);
        let child = Command::new(&bin)
            .arg(&sock)
            .stderr(Stdio::null())
            .spawn()
            .unwrap_or_else(|e| panic!("spawn {}: {e}", bin.display()));
        // Wait for the socket to appear.
        let deadline = Instant::now() + Duration::from_secs(10);
        while !std::path::Path::new(&sock).exists() {
            assert!(Instant::now() < deadline, "{name}: socket never appeared");
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(100)); // listen() settles
        ServerUnderTest {
            name,
            bin,
            child,
            sock,
        }
    }

    fn rss_kb(&self) -> Option<u64> {
        let status = std::fs::read_to_string(format!("/proc/{}/status", self.child.id())).ok()?;
        let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
        line.split_whitespace().nth(1)?.parse().ok()
    }

    fn bin_size_kb(&self) -> u64 {
        std::fs::metadata(&self.bin)
            .map(|m| m.len() / 1024)
            .unwrap_or(0)
    }
}

impl Drop for ServerUnderTest {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.sock);
    }
}

struct Numbers {
    p50_us: u128,
    p99_us: u128,
    mean_us: u128,
    rps: f64,
    stream_msgs_per_s: f64,
    stream_mb_per_s: f64,
    rss_unary_kb: u64,
    rss_kb: u64,
    bin_kb: u64,
}

async fn bench_one(srv: &ServerUnderTest) -> Numbers {
    let channel = channel_for(srv.sock.clone()).await.expect("connect");
    let mut client = BleServiceClient::new(channel);

    // ---- unary --------------------------------------------------------
    for _ in 0..UNARY_WARMUP {
        client.init(InitRequest {}).await.expect("warmup init");
    }
    let n_unary = unary_n();
    let mut lat = Vec::with_capacity(n_unary);
    let t0 = Instant::now();
    for _ in 0..n_unary {
        let t = Instant::now();
        let r = client.init(InitRequest {}).await.expect("init");
        assert!(r.into_inner().ok);
        lat.push(t.elapsed());
    }
    let total = t0.elapsed();
    lat.sort();
    let p50_us = lat[n_unary / 2].as_micros();
    let p99_us = lat[n_unary * 99 / 100].as_micros();
    let mean_us = (total.as_micros()) / n_unary as u128;
    let rps = n_unary as f64 / total.as_secs_f64();
    let rss_unary_kb = srv.rss_kb().unwrap_or(0);

    // ---- streaming ------------------------------------------------------
    let n = stream_n();
    let mut best_msgs_per_s = 0f64;
    let mut best_mb_per_s = 0f64;
    for _ in 0..STREAM_RUNS {
        let t = Instant::now();
        let mut stream = client
            .scan_result_stream(ScanResultStreamRequest {})
            .await
            .expect("stream open")
            .into_inner();
        let mut count = 0usize;
        let mut bytes = 0usize;
        while let Some(msg) = stream.message().await.expect("stream msg") {
            count += 1;
            bytes += msg.mac.len() + msg.adv_data.len() + 8;
        }
        assert_eq!(count, n, "{}: stream truncated", srv.name);
        let dt = t.elapsed().as_secs_f64();
        best_msgs_per_s = best_msgs_per_s.max(count as f64 / dt);
        best_mb_per_s = best_mb_per_s.max(bytes as f64 / dt / 1e6);
    }

    Numbers {
        p50_us,
        p99_us,
        mean_us,
        rps,
        stream_msgs_per_s: best_msgs_per_s,
        stream_mb_per_s: best_mb_per_s,
        rss_unary_kb,
        rss_kb: srv.rss_kb().unwrap_or(0),
        bin_kb: srv.bin_size_kb(),
    }
}

#[tokio::main]
async fn main() {
    // Escape hatch for profiling: bench one EXTERNAL server (already running,
    // e.g. under strace/perf) instead of spawning the pair.
    if let Ok(sock) = std::env::var("BENCH_EXTERNAL_SOCK") {
        let channel = channel_for(sock.clone()).await.expect("connect external");
        let mut client = BleServiceClient::new(channel);
        // A few unary calls so memcheck runs exercise that path too.
        for _ in 0..50 {
            let r = client.init(InitRequest {}).await.expect("init");
            assert!(r.into_inner().ok);
        }
        let n = stream_n();
        let t = Instant::now();
        let mut stream = client
            .scan_result_stream(ScanResultStreamRequest {})
            .await
            .expect("stream open")
            .into_inner();
        let mut count = 0usize;
        while let Some(_msg) = stream.message().await.expect("stream msg") {
            count += 1;
        }
        let dt = t.elapsed().as_secs_f64();
        println!(
            "external {sock}: {count} msgs in {dt:.3}s = {:.0} msgs/s",
            count as f64 / dt
        );
        assert_eq!(count, n);
        return;
    }

    let bin_dir = std::env::current_exe()
        .expect("exe")
        .parent()
        .unwrap()
        .to_path_buf();
    let mut results = Vec::new();
    for name in ["grpcuds_server", "tonic_server"] {
        let srv = ServerUnderTest::spawn(name, &bin_dir);
        let nums = bench_one(&srv).await;
        results.push((name, nums));
    }

    println!("\nstream messages per call: {}\n", stream_n());
    println!("| metric | {} | {} |", results[0].0, results[1].0);
    println!("| --- | --- | --- |");
    let (a, b) = (&results[0].1, &results[1].1);
    println!("| unary p50 (us) | {} | {} |", a.p50_us, b.p50_us);
    println!("| unary p99 (us) | {} | {} |", a.p99_us, b.p99_us);
    println!("| unary mean (us) | {} | {} |", a.mean_us, b.mean_us);
    println!("| unary RPS | {:.0} | {:.0} |", a.rps, b.rps);
    println!(
        "| stream msgs/s | {:.0} | {:.0} |",
        a.stream_msgs_per_s, b.stream_msgs_per_s
    );
    println!(
        "| stream payload MB/s | {:.1} | {:.1} |",
        a.stream_mb_per_s, b.stream_mb_per_s
    );
    println!(
        "| server RSS after unary (KB) | {} | {} |",
        a.rss_unary_kb, b.rss_unary_kb
    );
    println!(
        "| server RSS after stream (KB) | {} | {} |",
        a.rss_kb, b.rss_kb
    );
    println!("| stripped binary (KB) | {} | {} |", a.bin_kb, b.bin_kb);
}
