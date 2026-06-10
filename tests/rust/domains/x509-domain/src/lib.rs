// SPDX-License-Identifier: MIT OR Apache-2.0
//! X.509 domain logic shared by the `x509-gg` / `x509-gt` / `x509-tg` cells.
//!
//! - [`x509_builder`] — the **real** grpcuds certificate agent (rcgen +
//!   x509-parser): generate / inspect / fingerprint / expiry.
//! - [`spawn_tonic`] — a **deterministic mock** stock-gRPC server (no crypto):
//!   the `tg` row proves a grpcuds *client* talks to a stock server; canned
//!   values keep this crate free of a duplicate cert stack on the tonic side.

use std::sync::Arc;

/// grpcuds-build output: the generated `X509` trait + `add_x509_service`.
pub mod proto_grpcuds {
    include!(concat!(env!("OUT_DIR"), "/grpcuds/x509.rs"));
}

/// Canonical prost messages + tonic client/server stubs.
pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/tonic/x509.rs"));
}

/// gRPC method paths for raw (non-stub) grpcuds clients — the cells use the
/// generated `proto_grpcuds` stubs; sizebench drives these directly.
pub mod paths {
    pub const GENERATE: &str = "/x509.X509/GenerateSelfSigned";
    pub const INSPECT: &str = "/x509.X509/Inspect";
    pub const FINGERPRINT: &str = "/x509.X509/Fingerprint";
    pub const CHECK_EXPIRY: &str = "/x509.X509/CheckExpiry";
}

// ---- grpcuds server (real cert agent) ---------------------------------------

use grpcuds::{MessageWriter, Server, ServerBuilder, Status};
use sha2::Digest;
use x509_parser::prelude::{FromDer, GeneralName, ParsedExtension, X509Certificate};

use proto_grpcuds::{
    CertInfo, CheckExpiryRequest, ExpiryStatus, FingerprintReply, FingerprintRequest,
    GenerateSelfSignedRequest, HashAlgo, KeyPairPem, PemCert, X509,
};

pub struct CertAgent;

fn pem_to_der(pem: &str) -> Result<Vec<u8>, Status> {
    let (_, doc) = x509_parser::pem::parse_x509_pem(pem.as_bytes())
        .map_err(|e| Status::invalid_argument(format!("not a PEM certificate: {e}")))?;
    Ok(doc.contents)
}

fn parse_cert(der: &[u8]) -> Result<X509Certificate<'_>, Status> {
    let (_, cert) = X509Certificate::from_der(der)
        .map_err(|e| Status::invalid_argument(format!("not an X.509 certificate: {e}")))?;
    Ok(cert)
}

fn cert_info(cert: &X509Certificate<'_>) -> CertInfo {
    let mut sans = Vec::new();
    let mut is_ca = false;
    for ext in cert.extensions() {
        match ext.parsed_extension() {
            ParsedExtension::SubjectAlternativeName(s) => {
                for name in &s.general_names {
                    if let GeneralName::DNSName(d) = name {
                        sans.push((*d).to_string());
                    }
                }
            }
            ParsedExtension::BasicConstraints(b) => is_ca = b.ca,
            _ => {}
        }
    }
    CertInfo {
        subject: cert.subject().to_string(),
        issuer: cert.issuer().to_string(),
        serial_hex: cert.raw_serial_as_string().replace(':', ""),
        not_before_unix: cert.validity().not_before.timestamp(),
        not_after_unix: cert.validity().not_after.timestamp(),
        subject_alt_names: sans,
        is_ca,
    }
}

impl X509 for CertAgent {
    fn generate_self_signed(&self, req: GenerateSelfSignedRequest) -> Result<KeyPairPem, Status> {
        if req.common_name.is_empty() {
            return Err(Status::invalid_argument("common_name is required"));
        }
        let days = if req.validity_days == 0 {
            365
        } else {
            req.validity_days
        };

        let mut params = rcgen::CertificateParams::new(req.subject_alt_names.clone())
            .map_err(|e| Status::invalid_argument(format!("bad subject_alt_names: {e}")))?;
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, req.common_name.clone());
        let now = time::OffsetDateTime::now_utc();
        params.not_before = now;
        params.not_after = now + time::Duration::days(days as i64);

        let key = rcgen::KeyPair::generate()
            .map_err(|e| Status::internal(format!("keygen failed: {e}")))?;
        let cert = params
            .self_signed(&key)
            .map_err(|e| Status::internal(format!("self-sign failed: {e}")))?;
        Ok(KeyPairPem {
            cert_pem: cert.pem(),
            key_pem: key.serialize_pem(),
        })
    }

    fn inspect(&self, req: PemCert) -> Result<CertInfo, Status> {
        let der = pem_to_der(&req.pem)?;
        Ok(cert_info(&parse_cert(&der)?))
    }

    fn fingerprint(&self, req: FingerprintRequest) -> Result<FingerprintReply, Status> {
        let der = pem_to_der(&req.pem)?;
        fn to_hex(bytes: &[u8]) -> String {
            bytes.iter().map(|b| format!("{b:02x}")).collect()
        }
        let hex = match req.algo() {
            HashAlgo::Unspecified | HashAlgo::Sha256 => to_hex(&sha2::Sha256::digest(&der)),
            HashAlgo::Sha512 => to_hex(&sha2::Sha512::digest(&der)),
        };
        Ok(FingerprintReply { hex })
    }

    fn check_expiry(&self, req: CheckExpiryRequest, w: MessageWriter<ExpiryStatus>) -> Status {
        let now = if req.now_unix != 0 {
            req.now_unix
        } else {
            time::OffsetDateTime::now_utc().unix_timestamp()
        };
        std::thread::spawn(move || {
            for (i, pem) in req.pems.iter().enumerate() {
                let status = match pem_to_der(pem).and_then(|der| {
                    let cert = parse_cert(&der)?;
                    Ok((
                        cert.subject().to_string(),
                        cert.validity().not_after.timestamp(),
                    ))
                }) {
                    Ok((subject, not_after)) => ExpiryStatus {
                        index: i as u32,
                        subject,
                        not_after_unix: not_after,
                        seconds_remaining: not_after - now,
                        expired: not_after < now,
                    },
                    Err(_) => ExpiryStatus {
                        index: i as u32,
                        subject: "<unparseable>".into(),
                        ..Default::default()
                    },
                };
                if w.send(&status).is_err() {
                    return;
                }
            }
            let _ = w.finish(Status::ok());
        });
        Status::ok()
    }
}

/// A grpcuds `ServerBuilder` with the certificate agent registered.
pub fn x509_builder(sock: &str) -> ServerBuilder {
    proto_grpcuds::add_x509_service(Server::builder().bind(sock), Arc::new(CertAgent))
}

// ---- tonic (stock-gRPC) mock server -----------------------------------------

use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response};

/// Deterministic mock — no crypto. Proves the grpcuds client ⇄ stock server
/// wire with canned, easily-asserted values.
#[derive(Default)]
pub struct TonicX509;

#[tonic::async_trait]
impl proto::x509_server::X509 for TonicX509 {
    async fn generate_self_signed(
        &self,
        req: Request<proto::GenerateSelfSignedRequest>,
    ) -> Result<Response<proto::KeyPairPem>, tonic::Status> {
        let req = req.into_inner();
        if req.common_name.is_empty() {
            return Err(tonic::Status::invalid_argument("common_name is required"));
        }
        let days = if req.validity_days == 0 {
            365
        } else {
            req.validity_days
        };
        Ok(Response::new(proto::KeyPairPem {
            cert_pem: format!("MOCKCERT cn={} days={}\n", req.common_name, days),
            key_pem: "MOCKKEY\n".into(),
        }))
    }

    async fn inspect(
        &self,
        req: Request<proto::PemCert>,
    ) -> Result<Response<proto::CertInfo>, tonic::Status> {
        let pem = req.into_inner().pem;
        let cn = pem
            .split("cn=")
            .nth(1)
            .and_then(|s| s.split_whitespace().next())
            .unwrap_or("");
        Ok(Response::new(proto::CertInfo {
            subject: format!("CN={cn}"),
            issuer: format!("CN={cn}"),
            ..Default::default()
        }))
    }

    async fn fingerprint(
        &self,
        _req: Request<proto::FingerprintRequest>,
    ) -> Result<Response<proto::FingerprintReply>, tonic::Status> {
        Ok(Response::new(proto::FingerprintReply {
            hex: "00".repeat(32),
        }))
    }

    type CheckExpiryStream = ReceiverStream<Result<proto::ExpiryStatus, tonic::Status>>;
    async fn check_expiry(
        &self,
        req: Request<proto::CheckExpiryRequest>,
    ) -> Result<Response<Self::CheckExpiryStream>, tonic::Status> {
        let req = req.into_inner();
        let now = if req.now_unix != 0 {
            req.now_unix
        } else {
            1_700_000_000
        };
        let (tx, rx) = mpsc::channel(4);
        tokio::spawn(async move {
            for (i, _pem) in req.pems.iter().enumerate() {
                let not_after = now + 1000;
                let st = proto::ExpiryStatus {
                    index: i as u32,
                    subject: format!("cert-{i}"),
                    not_after_unix: not_after,
                    seconds_remaining: not_after - now,
                    expired: false,
                };
                if tx.send(Ok(st)).await.is_err() {
                    return;
                }
            }
        });
        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

/// Start the deterministic mock X.509 server on `sock`.
pub fn spawn_tonic(sock: &str) -> uds_harness::TonicServer {
    let routes = tonic::service::Routes::new(proto::x509_server::X509Server::new(TonicX509));
    uds_harness::serve_routes(sock.to_string(), routes)
}

/// A tonic X.509 client over the UDS at `path`.
pub async fn tonic_client(
    path: String,
) -> proto::x509_client::X509Client<tonic::transport::Channel> {
    proto::x509_client::X509Client::new(uds_harness::connect_uds(path).await)
}
