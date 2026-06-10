// SPDX-License-Identifier: MIT OR Apache-2.0
//! X.509 — grpcuds server + grpcuds client (real cert agent; the generated
//! `X509Client`).
use x509_domain::proto_grpcuds as pb;

#[test]
fn x509_grpcuds_to_grpcuds() {
    let sock = uds_harness::sock("x509-gg");
    let running = x509_domain::x509_builder(&sock)
        .build()
        .unwrap()
        .run()
        .unwrap();
    let mut cli = pb::X509Client::connect(&sock).unwrap();

    let kp = cli
        .generate_self_signed(pb::GenerateSelfSignedRequest {
            common_name: "interop.test".into(),
            subject_alt_names: vec!["interop.test".into()],
            validity_days: 30,
        })
        .unwrap();
    assert!(kp.cert_pem.contains("BEGIN CERTIFICATE"));

    let mut statuses = Vec::new();
    {
        let mut st = cli
            .check_expiry(pb::CheckExpiryRequest {
                pems: vec![kp.cert_pem.clone()],
                now_unix: 0,
            })
            .unwrap();
        while let Some(s) = st.message().unwrap() {
            statuses.push(s);
        }
    }
    assert_eq!(statuses.len(), 1);
    assert!(!statuses[0].expired, "a fresh 30-day cert is not expired");
    assert!(statuses[0].subject.contains("interop.test"));

    drop(cli);
    running.join().unwrap();
}
