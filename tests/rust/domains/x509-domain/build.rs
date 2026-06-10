// SPDX-License-Identifier: MIT OR Apache-2.0
//! Dual codegen for x509.proto (grpcuds server trait + tonic client/server).
use std::{env, fs, path::PathBuf};

fn main() {
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let grpcuds_out = out.join("grpcuds");
    let tonic_out = out.join("tonic");
    fs::create_dir_all(&grpcuds_out).unwrap();
    fs::create_dir_all(&tonic_out).unwrap();

    let proto = "../../proto/x509.proto";
    let include = "../../proto";

    let mut g = grpcuds_build::configure();
    g.config().out_dir(&grpcuds_out);
    g.compile_protos(&[proto], &[include])
        .expect("grpcuds-build x509.proto");

    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .out_dir(&tonic_out)
        .compile_protos(&[proto], &[include])
        .expect("tonic-prost-build x509.proto");

    println!("cargo:rerun-if-changed={proto}");
}
