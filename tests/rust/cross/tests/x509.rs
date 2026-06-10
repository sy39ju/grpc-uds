// SPDX-License-Identifier: MIT OR Apache-2.0
//! X.509 cross-language: Rust tonic mock peer ⇄ C++ grpcuds binary.
use x509_domain::proto;

/// gt: C++ grpcuds **server** ← Rust tonic **client**.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn x509_gt_cpp_server() {
    let Some(bin) = cross::cpp_bin("X509_GT_SERVER_BIN", "x509/x509-gt-server") else {
        eprintln!("skipping x509_gt: C++ x509-gt-server not built");
        return;
    };
    let sock = uds_harness::sock("x509-gt-cross");
    let _guard = uds_harness::cpp::spawn_server(&bin, &sock);

    let mut c = x509_domain::tonic_client(sock.clone()).await;
    let kp = c
        .generate_self_signed(proto::GenerateSelfSignedRequest {
            common_name: "mock.test".into(),
            validity_days: 10,
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert!(kp.cert_pem.contains("cn=mock.test"));

    let mut stream = c
        .check_expiry(proto::CheckExpiryRequest {
            pems: vec!["a".into(), "b".into()],
            now_unix: 1_000_000,
        })
        .await
        .unwrap()
        .into_inner();
    let mut statuses = Vec::new();
    while let Some(s) = stream.message().await.unwrap() {
        statuses.push(s);
    }
    assert_eq!(statuses.len(), 2);
    assert_eq!(statuses[1].subject, "cert-1");
    assert_eq!(statuses[0].seconds_remaining, 1000);
}

/// tg: Rust tonic mock **server** ← C++ grpcuds **client** (self-checking).
#[test]
fn x509_tg_cpp_client() {
    let Some(bin) = cross::cpp_bin("X509_TG_CLIENT_BIN", "x509/x509-tg-client") else {
        eprintln!("skipping x509_tg: C++ x509-tg-client not built");
        return;
    };
    let sock = uds_harness::sock("x509-tg-cross");
    let server = x509_domain::spawn_tonic(&sock);
    uds_harness::wait_for_sock(&sock);

    let ok = uds_harness::cpp::run_client(&bin, &sock);
    assert!(ok, "C++ x509-tg-client self-check failed");

    server.stop();
}
