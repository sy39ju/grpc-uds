// SPDX-License-Identifier: MIT OR Apache-2.0
//! X.509 — grpcuds server + tonic (stock gRPC) client (`cargo run -p x509-gt`).
use x509_domain::proto;

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let sock = uds_harness::sock("x509-gt-demo");
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
    println!("generated cert ({} bytes PEM)", kp.cert_pem.len());

    let info = c
        .inspect(proto::PemCert { pem: kp.cert_pem })
        .await
        .unwrap()
        .into_inner();
    println!("subject: {}", info.subject);

    running.join().unwrap();
}
