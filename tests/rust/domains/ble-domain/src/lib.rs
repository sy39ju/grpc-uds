// SPDX-License-Identifier: MIT OR Apache-2.0
//! BLE domain logic shared by the `ble-gg` / `ble-gt` / `ble-tg` example cells.
//!
//! Two server implementations of the same service:
//! - [`grpcuds_builder`] — the grpcuds server (implements the trait
//!   `grpcuds-build` generates).
//! - [`spawn_tonic`] — a stock-gRPC (tonic) server, the "other peer".
//!
//! Both emit the same deterministic results ([`expect::ble_scan`]), so one set
//! of assertions fits every transport combo. The grpcuds *client* (in the cells)
//! decodes the [`proto`] (tonic-side) prost types, which are wire-compatible
//! with what the grpcuds server emits.

use std::sync::Arc;

/// grpcuds-build output: the generated `BleService` trait + `add_ble_service`
/// (used only by the grpcuds server impl below).
pub mod proto_grpcuds {
    include!(concat!(env!("OUT_DIR"), "/grpcuds/ble.rs"));
}

/// Canonical prost messages + tonic client/server stubs.
pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/tonic/ble.rs"));
}

/// gRPC method paths for raw (non-stub) grpcuds clients — the cells use the
/// generated `proto_grpcuds` stubs; sizebench drives these directly.
pub mod paths {
    pub const INIT: &str = "/ble.BleService/Init";
    pub const SCAN_STREAM: &str = "/ble.BleService/ScanResultStream";
    pub const REMOVE_FILTER: &str = "/ble.BleService/RemoveScanFilter";
}

/// Deterministic fixtures both servers reproduce.
pub mod expect {
    /// The three scan results both BLE servers emit: (mac, rssi, adv_data).
    pub fn ble_scan() -> Vec<(String, i32, Vec<u8>)> {
        (0..3i32)
            .map(|i| {
                (
                    "AA:BB:CC:DD:EE:FF".to_string(),
                    -40 - i,
                    vec![0x02, 0x01, 0x06],
                )
            })
            .collect()
    }
}

// ---- grpcuds server ---------------------------------------------------------

use grpcuds::{MessageWriter, Server, ServerBuilder, Status};
use proto_grpcuds::*;

/// Toy BLE service: echoes the scan lifecycle and streams three results.
pub struct BleSim;

impl BleService for BleSim {
    fn init(&self, _req: InitRequest) -> Result<InitReply, Status> {
        Ok(InitReply { ok: true })
    }

    fn start_le_scan(&self, _req: StartLeScanRequest) -> Result<StartLeScanReply, Status> {
        Ok(StartLeScanReply { ok: true })
    }

    fn stop_le_scan(&self, _req: StopLeScanRequest) -> Result<StopLeScanReply, Status> {
        Ok(StopLeScanReply { ok: true })
    }

    fn scan_result_stream(
        &self,
        _req: ScanResultStreamRequest,
        w: MessageWriter<ScanResult>,
    ) -> Status {
        std::thread::spawn(move || {
            for (mac, rssi, adv) in expect::ble_scan() {
                let r = ScanResult {
                    mac,
                    rssi,
                    adv_data: adv,
                };
                if w.send(&r).is_err() {
                    return;
                }
            }
            let _ = w.finish(Status::ok());
        });
        Status::ok()
    }

    fn adapter_state_change_stream(
        &self,
        _req: AdapterStateChangeStreamRequest,
        w: MessageWriter<AdapterStateChange>,
    ) -> Status {
        let _ = w.finish(Status::ok());
        Status::ok()
    }

    fn add_scan_filter(&self, _req: AddScanFilterRequest) -> Result<AddScanFilterReply, Status> {
        Ok(AddScanFilterReply { filter_id: 7 })
    }

    fn remove_scan_filter(
        &self,
        req: RemoveScanFilterRequest,
    ) -> Result<RemoveScanFilterReply, Status> {
        if req.filter_id != 7 {
            return Err(Status::not_found("unknown filter id"));
        }
        Ok(RemoveScanFilterReply { ok: true })
    }
}

/// A grpcuds `ServerBuilder` with the BLE service registered.
pub fn grpcuds_builder(sock: &str) -> ServerBuilder {
    add_ble_service(Server::builder().bind(sock), Arc::new(BleSim))
}

// ---- tonic (stock-gRPC) server ----------------------------------------------

use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response};

/// The same service implemented as a stock tonic server.
#[derive(Default)]
pub struct TonicBle;

#[tonic::async_trait]
impl proto::ble_service_server::BleService for TonicBle {
    async fn init(
        &self,
        _req: Request<proto::InitRequest>,
    ) -> Result<Response<proto::InitReply>, tonic::Status> {
        Ok(Response::new(proto::InitReply { ok: true }))
    }

    async fn start_le_scan(
        &self,
        _req: Request<proto::StartLeScanRequest>,
    ) -> Result<Response<proto::StartLeScanReply>, tonic::Status> {
        Ok(Response::new(proto::StartLeScanReply { ok: true }))
    }

    async fn stop_le_scan(
        &self,
        _req: Request<proto::StopLeScanRequest>,
    ) -> Result<Response<proto::StopLeScanReply>, tonic::Status> {
        Ok(Response::new(proto::StopLeScanReply { ok: true }))
    }

    type ScanResultStreamStream = ReceiverStream<Result<proto::ScanResult, tonic::Status>>;
    async fn scan_result_stream(
        &self,
        _req: Request<proto::ScanResultStreamRequest>,
    ) -> Result<Response<Self::ScanResultStreamStream>, tonic::Status> {
        let (tx, rx) = mpsc::channel(4);
        tokio::spawn(async move {
            for (mac, rssi, adv) in expect::ble_scan() {
                let r = proto::ScanResult {
                    mac,
                    rssi,
                    adv_data: adv,
                };
                if tx.send(Ok(r)).await.is_err() {
                    return;
                }
            }
        });
        Ok(Response::new(ReceiverStream::new(rx)))
    }

    type AdapterStateChangeStreamStream =
        ReceiverStream<Result<proto::AdapterStateChange, tonic::Status>>;
    async fn adapter_state_change_stream(
        &self,
        _req: Request<proto::AdapterStateChangeStreamRequest>,
    ) -> Result<Response<Self::AdapterStateChangeStreamStream>, tonic::Status> {
        let (_tx, rx) = mpsc::channel(1);
        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn add_scan_filter(
        &self,
        _req: Request<proto::AddScanFilterRequest>,
    ) -> Result<Response<proto::AddScanFilterReply>, tonic::Status> {
        Ok(Response::new(proto::AddScanFilterReply { filter_id: 7 }))
    }

    async fn remove_scan_filter(
        &self,
        req: Request<proto::RemoveScanFilterRequest>,
    ) -> Result<Response<proto::RemoveScanFilterReply>, tonic::Status> {
        if req.into_inner().filter_id != 7 {
            return Err(tonic::Status::not_found("unknown filter id"));
        }
        Ok(Response::new(proto::RemoveScanFilterReply { ok: true }))
    }
}

/// Start the stock-gRPC BLE server on `sock` (background thread).
pub fn spawn_tonic(sock: &str) -> uds_harness::TonicServer {
    let routes =
        tonic::service::Routes::new(proto::ble_service_server::BleServiceServer::new(TonicBle));
    uds_harness::serve_routes(sock.to_string(), routes)
}

/// A tonic BLE client over the UDS at `path`.
pub async fn tonic_client(
    path: String,
) -> proto::ble_service_client::BleServiceClient<tonic::transport::Channel> {
    proto::ble_service_client::BleServiceClient::new(uds_harness::connect_uds(path).await)
}
