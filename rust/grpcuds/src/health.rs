// SPDX-License-Identifier: MIT OR Apache-2.0
//! The standard gRPC health checking service (`grpc.health.v1.Health`) —
//! the opt-in `health` feature.
//!
//! Registers `Check` (unary) and `Watch` (server-streaming) so stock
//! tooling — `grpc_health_probe`, `grpcurl`, tonic-health clients,
//! orchestrator probes — can ask "is this daemon serving?" without any
//! custom protocol:
//!
//! ```no_run
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use grpcuds::health::{HealthReporter, ServingStatus};
//!
//! let reporter = HealthReporter::new(); // "" (overall) starts SERVING
//! let builder = grpcuds::Server::builder().bind("/run/svc.sock");
//! let builder = grpcuds::health::add_health_service(builder, &reporter);
//! let running = builder.build()?.run()?;
//!
//! reporter.set_status("ble.BleService", ServingStatus::Serving);
//! // ... later, e.g. when the adapter dies:
//! reporter.set_status("ble.BleService", ServingStatus::NotServing);
//! # Ok(()) }
//! ```
//!
//! The message types are tiny and defined here with prost derives — no
//! protoc run, no build-script codegen. Semantics follow the official
//! health-checking protocol: `Check` on an unregistered service fails with
//! `NOT_FOUND`; `Watch` answers immediately (with `SERVICE_UNKNOWN` for
//! unregistered names) and then streams every status change.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::MessageWriter;
use crate::{ServerBuilder, Status};

/// `grpc.health.v1` message types (prost-derived, wire-identical).
pub mod pb {
    /// `grpc.health.v1.HealthCheckRequest`
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct HealthCheckRequest {
        /// The service to query; `""` means the server overall.
        #[prost(string, tag = "1")]
        pub service: String,
    }

    /// `grpc.health.v1.HealthCheckResponse`
    #[derive(Clone, Copy, PartialEq, prost::Message)]
    pub struct HealthCheckResponse {
        /// A [`super::ServingStatus`] as its wire integer.
        #[prost(enumeration = "super::ServingStatus", tag = "1")]
        pub status: i32,
    }
}

/// `grpc.health.v1.HealthCheckResponse.ServingStatus`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, prost::Enumeration)]
#[repr(i32)]
pub enum ServingStatus {
    /// Health unknown.
    Unknown = 0,
    /// Up and serving.
    Serving = 1,
    /// Registered but refusing work.
    NotServing = 2,
    /// `Watch`-only: the watched service name is not registered.
    ServiceUnknown = 3,
}

struct Inner {
    statuses: HashMap<String, ServingStatus>,
    watchers: HashMap<String, Vec<MessageWriter<pb::HealthCheckResponse>>>,
}

/// Shared handle for publishing health: clone it anywhere (it is an `Arc`),
/// flip statuses from any thread; open `Watch` streams hear every change.
#[derive(Clone)]
pub struct HealthReporter {
    inner: Arc<Mutex<Inner>>,
}

impl Default for HealthReporter {
    fn default() -> Self {
        Self::new()
    }
}

impl HealthReporter {
    /// A reporter whose overall service (`""`) starts as `SERVING`.
    pub fn new() -> Self {
        let mut statuses = HashMap::new();
        statuses.insert(String::new(), ServingStatus::Serving);
        Self {
            inner: Arc::new(Mutex::new(Inner {
                statuses,
                watchers: HashMap::new(),
            })),
        }
    }

    /// Set (or register) `service`'s status and notify its watchers.
    /// Use `""` for the server overall.
    pub fn set_status(&self, service: impl Into<String>, status: ServingStatus) {
        let service = service.into();
        let mut inner = self.inner.lock().expect("health registry poisoned");
        inner.statuses.insert(service.clone(), status);
        if let Some(ws) = inner.watchers.get_mut(&service) {
            let resp = pb::HealthCheckResponse {
                status: status as i32,
            };
            // Drop watchers whose call is finished or whose client is gone.
            ws.retain(|w| w.send(&resp).is_ok());
        }
    }

    fn check(&self, service: &str) -> Option<ServingStatus> {
        self.inner
            .lock()
            .expect("health registry poisoned")
            .statuses
            .get(service)
            .copied()
    }

    fn watch(&self, service: String, writer: MessageWriter<pb::HealthCheckResponse>) {
        let mut inner = self.inner.lock().expect("health registry poisoned");
        let current = inner
            .statuses
            .get(&service)
            .copied()
            .unwrap_or(ServingStatus::ServiceUnknown);
        // Register only if the initial send lands: a writer whose call is
        // already gone would otherwise sit in the list until the service's
        // next set_status (which may never come).
        let sent = writer.send(&pb::HealthCheckResponse {
            status: current as i32,
        });
        if sent.is_ok() {
            inner.watchers.entry(service).or_default().push(writer);
        }
    }
}

/// Register `grpc.health.v1.Health/{Check,Watch}` on `builder`. Statuses
/// are published through the `reporter` (clone it into your app).
pub fn add_health_service(builder: ServerBuilder, reporter: &HealthReporter) -> ServerBuilder {
    let check = reporter.clone();
    let watch = reporter.clone();
    builder
        .add_unary_msg(
            "/grpc.health.v1.Health/Check",
            move |req: pb::HealthCheckRequest| match check.check(&req.service) {
                Some(status) => Ok(pb::HealthCheckResponse {
                    status: status as i32,
                }),
                // The protocol: unknown service names fail with NOT_FOUND.
                None => Err(Status::not_found("unknown service")),
            },
        )
        .add_server_streaming_msg(
            "/grpc.health.v1.Health/Watch",
            move |req: pb::HealthCheckRequest, w: &MessageWriter<pb::HealthCheckResponse>| {
                watch.watch(req.service, w.clone());
                Status::ok() // stream stays open; the reporter feeds it
            },
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message as _;

    /// The wire values are the grpc.health.v1 contract — a transposed
    /// variant would compile silently and break every stock prober.
    #[test]
    fn serving_status_wire_values_match_the_protocol() {
        assert_eq!(ServingStatus::Unknown as i32, 0);
        assert_eq!(ServingStatus::Serving as i32, 1);
        assert_eq!(ServingStatus::NotServing as i32, 2);
        assert_eq!(ServingStatus::ServiceUnknown as i32, 3);
    }

    /// The hand-written derives must produce the canonical encoding:
    /// field 1 as a length-delimited string / varint enum.
    #[test]
    fn pb_types_encode_with_the_protocol_field_tags() {
        let req = pb::HealthCheckRequest {
            service: "svc".into(),
        };
        // tag 1, wire type 2 (len-delimited) = 0x0A, len 3, "svc".
        assert_eq!(req.encode_to_vec(), b"\x0a\x03svc");

        let resp = pb::HealthCheckResponse {
            status: ServingStatus::Serving as i32,
        };
        // tag 1, wire type 0 (varint) = 0x08, value 1.
        assert_eq!(resp.encode_to_vec(), b"\x08\x01");
        // Default (UNKNOWN = 0) encodes empty, proto3 style.
        assert_eq!(pb::HealthCheckResponse::default().encode_to_vec(), b"");
    }

    #[test]
    fn reporter_registry_semantics() {
        let r = HealthReporter::new();
        // "" (overall) is born SERVING; everything else is unregistered.
        assert_eq!(r.check(""), Some(ServingStatus::Serving));
        assert_eq!(r.check("ghost"), None);

        // set_status registers and updates.
        r.set_status("svc", ServingStatus::Serving);
        assert_eq!(r.check("svc"), Some(ServingStatus::Serving));
        r.set_status("svc", ServingStatus::NotServing);
        assert_eq!(r.check("svc"), Some(ServingStatus::NotServing));

        // The overall entry can be flipped too.
        r.set_status("", ServingStatus::NotServing);
        assert_eq!(r.check(""), Some(ServingStatus::NotServing));
    }
}
