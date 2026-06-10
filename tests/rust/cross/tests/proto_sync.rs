// SPDX-License-Identifier: MIT OR Apache-2.0
//! The Rust examples build from `tests/rust/proto/` and the C++ examples
//! from `tests/cpp/proto/` (which additionally holds the nanopb `.options`
//! and the C++-only `agent_cpp.proto`). The cross-language tests only prove
//! anything if the shared `.proto` files are identical — guard against drift.

use std::path::Path;

#[test]
fn rust_and_cpp_proto_copies_are_identical() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    for name in ["ble.proto", "agent.proto", "x509.proto"] {
        let rust_p = root.join("tests/rust/proto").join(name);
        let cpp_p = root.join("tests/cpp/proto").join(name);
        let rust_s =
            std::fs::read_to_string(&rust_p).unwrap_or_else(|e| panic!("read {rust_p:?}: {e}"));
        let cpp_s =
            std::fs::read_to_string(&cpp_p).unwrap_or_else(|e| panic!("read {cpp_p:?}: {e}"));
        assert_eq!(
            rust_s, cpp_s,
            "{name} differs between tests/rust/proto and tests/cpp/proto — \
             edit both copies (the wire contract must match)"
        );
    }
}
