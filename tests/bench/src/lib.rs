// SPDX-License-Identifier: MIT OR Apache-2.0
//! Shared pieces: generated proto types + a tonic channel over UDS.

pub mod ble {
    tonic::include_proto!("ble");
}

use std::path::PathBuf;

use tonic::transport::{Channel, Endpoint};
use tower::service_fn;

/// Messages streamed per `ScanResultStream` call (servers read this env var).
pub const STREAM_N_ENV: &str = "BENCH_STREAM_N";

pub fn stream_n() -> usize {
    std::env::var(STREAM_N_ENV)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(50_000)
}

/// `adv_data` size per streamed message (BENCH_MSG_SIZE, default 24 B —
/// a realistic BLE advertisement; set e.g. 16384 to bench large messages).
pub fn msg_size() -> usize {
    static SIZE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *SIZE.get_or_init(|| {
        std::env::var("BENCH_MSG_SIZE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(24)
    })
}

/// One scan result: MAC string + RSSI + `msg_size()` bytes of adv payload
/// (~50 B encoded at the default).
pub fn sample_result(i: usize) -> ble::ScanResult {
    ble::ScanResult {
        mac: "AA:BB:CC:DD:EE:FF".to_string(),
        rssi: -((40 + (i & 0x3f)) as i32),
        adv_data: vec![0x5a; msg_size()],
    }
}

/// tonic `Channel` over a UNIX domain socket (authority is a placeholder).
pub async fn channel_for(path: impl Into<PathBuf>) -> Result<Channel, tonic::transport::Error> {
    let path = path.into();
    Endpoint::try_from("http://unix")?
        .connect_with_connector(service_fn(move |_| {
            let path = path.clone();
            async move {
                let stream = tokio::net::UnixStream::connect(&path).await?;
                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(stream))
            }
        }))
        .await
}
