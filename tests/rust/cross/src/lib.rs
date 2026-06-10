// SPDX-License-Identifier: MIT OR Apache-2.0
//! Cross-language interop: see `tests/`. This lib only provides the shared
//! helper for locating the C++ example binaries.

use std::path::PathBuf;

/// Locate a C++ example binary built under `tests/cpp/build/<rel>`, honoring
/// `$<env_var>` first. Returns `None` (→ skip the test) when neither exists.
pub fn cpp_bin(env_var: &str, rel: &str) -> Option<PathBuf> {
    let fallback = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../tests/cpp/build")
        .join(rel);
    uds_harness::cpp::locate(env_var, fallback)
}
