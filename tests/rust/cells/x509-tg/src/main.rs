// SPDX-License-Identifier: MIT OR Apache-2.0
//! X.509 — tonic (stock gRPC, deterministic mock) server + grpcuds client
//! (`cargo run -p x509-tg`). The mock returns canned data — row tg is about the
//! grpcuds *client* reaching a stock server, not re-implementing crypto.
use x509_domain::proto_grpcuds as pb;

fn main() {
    let sock = uds_harness::sock("x509-tg-demo");
    let server = x509_domain::spawn_tonic(&sock);
    uds_harness::wait_for_sock(&sock);

    let mut cli = pb::X509Client::connect(&sock).unwrap();
    let kp = cli
        .generate_self_signed(pb::GenerateSelfSignedRequest {
            common_name: "mock.test".into(),
            validity_days: 10,
            ..Default::default()
        })
        .unwrap();
    println!("mock cert_pem: {}", kp.cert_pem.trim());

    drop(cli);
    server.stop();
}
