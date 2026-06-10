// SPDX-License-Identifier: MIT OR Apache-2.0
//! X.509 — tonic (stock gRPC, deterministic mock) server + grpcuds client
//! (the generated `X509Client`).
use x509_domain::proto_grpcuds as pb;

#[test]
fn x509_tonic_to_grpcuds() {
    let sock = uds_harness::sock("x509-tg");
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
    assert!(kp.cert_pem.contains("cn=mock.test"));

    let mut statuses = Vec::new();
    {
        let mut st = cli
            .check_expiry(pb::CheckExpiryRequest {
                pems: vec!["a".into(), "b".into()],
                now_unix: 1_000_000,
            })
            .unwrap();
        while let Some(s) = st.message().unwrap() {
            statuses.push(s);
        }
    }
    assert_eq!(statuses.len(), 2);
    assert_eq!(statuses[1].subject, "cert-1");
    assert_eq!(statuses[0].seconds_remaining, 1000);

    drop(cli);
    server.stop();
}
