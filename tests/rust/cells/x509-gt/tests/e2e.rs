// SPDX-License-Identifier: MIT OR Apache-2.0
//! X.509 — grpcuds server + tonic (stock gRPC) client (real cert agent).
use x509_domain::proto;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn x509_grpcuds_to_tonic() {
    let sock = uds_harness::sock("x509-gt");
    let running = x509_domain::x509_builder(&sock)
        .build()
        .unwrap()
        .run()
        .unwrap();
    let mut c = x509_domain::tonic_client(sock.clone()).await;

    let kp = c
        .generate_self_signed(proto::GenerateSelfSignedRequest {
            common_name: "interop.test".into(),
            subject_alt_names: vec!["interop.test".into()],
            validity_days: 30,
        })
        .await
        .unwrap()
        .into_inner();
    assert!(kp.cert_pem.contains("BEGIN CERTIFICATE"));

    let mut stream = c
        .check_expiry(proto::CheckExpiryRequest {
            pems: vec![kp.cert_pem.clone()],
            now_unix: 0,
        })
        .await
        .unwrap()
        .into_inner();
    let mut statuses = Vec::new();
    while let Some(s) = stream.message().await.unwrap() {
        statuses.push(s);
    }
    assert_eq!(statuses.len(), 1);
    assert!(!statuses[0].expired);

    running.join().unwrap();
}
