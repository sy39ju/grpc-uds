// SPDX-License-Identifier: MIT OR Apache-2.0
//! BLE bench server on tonic (prost messages, tokio multi-thread runtime —
//! tonic's production defaults), serving the same proto over the same UDS.

use grpcuds_bench::ble::ble_service_server::{BleService, BleServiceServer};
use grpcuds_bench::ble::*;
use grpcuds_bench::{sample_result, stream_n};
use tokio_stream::wrappers::UnixListenerStream;
use tonic::{Request, Response, Status};

struct Impl {
    stream_n: usize,
}

type Stream<T> =
    std::pin::Pin<Box<dyn tokio_stream::Stream<Item = Result<T, Status>> + Send + 'static>>;

#[tonic::async_trait]
impl BleService for Impl {
    async fn init(&self, _r: Request<InitRequest>) -> Result<Response<InitReply>, Status> {
        Ok(Response::new(InitReply { ok: true }))
    }

    async fn start_le_scan(
        &self,
        _r: Request<StartLeScanRequest>,
    ) -> Result<Response<StartLeScanReply>, Status> {
        Ok(Response::new(StartLeScanReply { ok: true }))
    }

    async fn stop_le_scan(
        &self,
        _r: Request<StopLeScanRequest>,
    ) -> Result<Response<StopLeScanReply>, Status> {
        Ok(Response::new(StopLeScanReply { ok: true }))
    }

    type ScanResultStreamStream = Stream<ScanResult>;
    async fn scan_result_stream(
        &self,
        _r: Request<ScanResultStreamRequest>,
    ) -> Result<Response<Self::ScanResultStreamStream>, Status> {
        let n = self.stream_n;
        let it = (0..n).map(|i| Ok(sample_result(i)));
        Ok(Response::new(Box::pin(tokio_stream::iter(it))))
    }

    type AdapterStateChangeStreamStream = Stream<AdapterStateChange>;
    async fn adapter_state_change_stream(
        &self,
        _r: Request<AdapterStateChangeStreamRequest>,
    ) -> Result<Response<Self::AdapterStateChangeStreamStream>, Status> {
        Ok(Response::new(Box::pin(tokio_stream::iter(
            std::iter::empty(),
        ))))
    }

    async fn add_scan_filter(
        &self,
        _r: Request<AddScanFilterRequest>,
    ) -> Result<Response<AddScanFilterReply>, Status> {
        Ok(Response::new(AddScanFilterReply { filter_id: 1 }))
    }

    async fn remove_scan_filter(
        &self,
        _r: Request<RemoveScanFilterRequest>,
    ) -> Result<Response<RemoveScanFilterReply>, Status> {
        Ok(Response::new(RemoveScanFilterReply { ok: true }))
    }
}

#[tokio::main]
async fn main() {
    let sock = std::env::args()
        .nth(1)
        .expect("usage: tonic_server <uds-path>");
    let _ = std::fs::remove_file(&sock);
    let listener = tokio::net::UnixListener::bind(&sock).expect("bind");
    eprintln!("tonic_server ready on {sock}");
    tonic::transport::Server::builder()
        .add_service(BleServiceServer::new(Impl {
            stream_n: stream_n(),
        }))
        .serve_with_incoming(UnixListenerStream::new(listener))
        .await
        .expect("serve");
}
