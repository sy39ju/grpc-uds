// SPDX-License-Identifier: MIT OR Apache-2.0
//! Dual codegen: grpcuds-build emits the server trait (`OUT_DIR/grpcuds/`),
//! tonic-prost-build emits the canonical prost messages + tonic client/server
//! (`OUT_DIR/tonic/`). Both come from one proto, so they are wire-compatible.
use std::{env, fs, path::PathBuf};

fn main() {
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let grpcuds_out = out.join("grpcuds");
    let tonic_out = out.join("tonic");
    fs::create_dir_all(&grpcuds_out).unwrap();
    fs::create_dir_all(&tonic_out).unwrap();

    let proto = "../../proto/ble.proto";
    let include = "../../proto";

    let mut g = grpcuds_build::configure();
    g.config().out_dir(&grpcuds_out);
    g.compile_protos(&[proto], &[include])
        .expect("grpcuds-build ble.proto");

    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .out_dir(&tonic_out)
        .compile_protos(&[proto], &[include])
        .expect("tonic-prost-build ble.proto");

    println!("cargo:rerun-if-changed={proto}");
}
