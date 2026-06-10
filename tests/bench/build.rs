// SPDX-License-Identifier: MIT OR Apache-2.0
fn main() {
    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&["../rust/proto/ble.proto"], &["../rust/proto"])
        .expect("tonic-build: compile ble.proto");
    println!("cargo:rerun-if-changed=../rust/proto/ble.proto");
}
