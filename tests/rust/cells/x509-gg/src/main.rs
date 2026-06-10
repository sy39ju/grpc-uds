// SPDX-License-Identifier: MIT OR Apache-2.0
//! X.509 — grpcuds server + grpcuds client (`cargo run -p x509-gg`).
use x509_domain::proto_grpcuds as pb;

fn main() {
    let sock = uds_harness::sock("x509-gg-demo");
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
    println!("generated cert ({} bytes PEM)", kp.cert_pem.len());

    let info = cli.inspect(pb::PemCert { pem: kp.cert_pem }).unwrap();
    println!("subject: {}", info.subject);

    drop(cli);
    running.join().unwrap();
}
