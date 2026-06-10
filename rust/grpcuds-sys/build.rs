// SPDX-License-Identifier: MIT OR Apache-2.0
//! grpcuds-sys build: bindgen over the nghttp2 headers + the link directive.
//!
//! Header/library resolution matrix (header and lib must come from the SAME
//! place — mixing, say, vendored headers with a different system lib is how
//! silent version skew happens):
//!
//! | nghttp2 in sysroot | link mode          | resolution                      |
//! |--------------------|--------------------|---------------------------------|
//! | yes                | dynamic (default)  | sysroot headers + sysroot lib   |
//! | yes                | `bundled`          | submodule source (sysroot unused) |
//! | no                 | dynamic (default)  | **error** — install libnghttp2-dev or use `bundled` |
//! | no                 | `bundled`          | submodule source                |
//!
//! "sysroot" is `$SYSROOT` when set (required for cross targets), else `/`
//! for host builds. The `bundled` source is the pinned `vendor/nghttp2-src/`
//! submodule; in a git checkout it is auto-initialized, and the published
//! package ships the needed subset so registry builds never touch git.
//!
//! docs.rs (no -dev package, no network, links nothing) is the one extra
//! case: it falls back to the SAME submodule headers, with `nghttp2ver.h`
//! rendered from its template — so every path reads nghttp2 headers from
//! exactly one pinned source.

use std::env;
use std::path::{Path, PathBuf};

fn main() {
    let target = env::var("TARGET").unwrap_or_default();
    let host = env::var("HOST").unwrap_or_default();
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let is_cross = target != host;
    let bundled = env::var_os("CARGO_FEATURE_BUNDLED").is_some();

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=wrapper.h");
    println!(
        "cargo:rerun-if-changed={}",
        manifest_dir.join("vendor").display()
    );
    println!("cargo:rerun-if-env-changed=SYSROOT");
    println!("cargo:rerun-if-env-changed=DOCS_RS");

    let mut builder =
        bindgen::Builder::default().header(manifest_dir.join("wrapper.h").to_string_lossy());
    if is_cross {
        builder = builder.clang_arg(format!("--target={target}"));
    }

    let sysroot = env::var("SYSROOT").ok().filter(|s| !s.is_empty());
    if let Some(s) = &sysroot {
        // For bindgen's libc header resolution in cross builds, and for the
        // nghttp2 header itself in the dynamic mode.
        builder = builder.clang_arg(format!("--sysroot={s}"));
    }

    if bundled {
        // Bundled: the submodule is the single source for headers AND code —
        // whatever the sysroot has is deliberately ignored.
        let include = build_bundled_nghttp2(&manifest_dir);
        builder = builder.clang_arg(format!("-I{}", include.display()));
    } else {
        // Dynamic: header + lib must both come from the sysroot.
        println!("cargo:rustc-link-lib=dylib=nghttp2");
        let root = match (&sysroot, is_cross) {
            (Some(s), _) => PathBuf::from(s),
            (None, false) => PathBuf::from("/"),
            (None, true) => panic!(
                "grpcuds-sys: cross dynamic build needs SYSROOT set to the \
                 target sysroot (for <nghttp2/nghttp2.h> and libnghttp2.so), \
                 or use --features bundled"
            ),
        };
        if sysroot_nghttp2_header(&root).is_some() {
            // Headers resolve via clang's (sys)root search; also help the
            // linker find the lib inside an explicit sysroot.
            if sysroot.is_some() {
                for lib in ["usr/lib", "usr/lib64", "lib"] {
                    let dir = root.join(lib);
                    if dir.is_dir() {
                        println!("cargo:rustc-link-search=native={}", dir.display());
                    }
                }
            }
        } else if env::var_os("DOCS_RS").is_some() {
            // docs.rs: no -dev package, no network, nothing gets linked.
            // Use the submodule headers (shipped in the package) and render
            // nghttp2ver.h from its template — same single source as bundled.
            let include = docs_rs_include(&manifest_dir);
            builder = builder.clang_arg(format!("-I{}", include.display()));
        } else {
            panic!(
                "grpcuds-sys: dynamic link requested but <nghttp2/nghttp2.h> \
                 was not found under {} — install the nghttp2 dev package \
                 into the sysroot (e.g. libnghttp2-dev), or build with \
                 --features bundled to compile nghttp2 from the vendored \
                 source instead",
                root.display()
            );
        }
    }

    let bindings = builder
        // no_std friendly output
        .use_core()
        .ctypes_prefix("::core::ffi")
        // Symbol surface (DESIGN.md §3.1)
        .allowlist_function("nghttp2_.*")
        .allowlist_type("nghttp2_.*")
        .allowlist_var("NGHTTP2_.*")
        // Keep generated code small / deterministic
        .layout_tests(false)
        .derive_default(false)
        .derive_debug(false)
        .merge_extern_blocks(true)
        .generate()
        .expect("bindgen: failed to generate nghttp2 bindings");

    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap()).join("bindings.rs");
    bindings
        .write_to_file(&out_path)
        .expect("bindgen: failed to write bindings.rs");
}

/// `<root>/.../nghttp2/nghttp2.h` if the sysroot carries the dev headers.
fn sysroot_nghttp2_header(root: &Path) -> Option<PathBuf> {
    for inc in ["usr/include", "usr/local/include", "include"] {
        let h = root.join(inc).join("nghttp2/nghttp2.h");
        if h.is_file() {
            return Some(h);
        }
    }
    None
}

/// docs.rs-only include dir: the submodule's `lib/includes` plus a shim dir
/// holding `nghttp2/nghttp2ver.h` rendered from the in-tree template (CMake
/// would normally generate it; a docs build never runs CMake).
fn docs_rs_include(manifest_dir: &Path) -> PathBuf {
    let src = ensure_bundled_source(manifest_dir);
    let includes = src.join("lib/includes");
    let tpl = std::fs::read_to_string(includes.join("nghttp2/nghttp2ver.h.in"))
        .expect("nghttp2ver.h.in missing from the nghttp2 source");
    let cml = std::fs::read_to_string(src.join("CMakeLists.txt"))
        .expect("nghttp2 CMakeLists.txt missing");
    let version = cml
        .lines()
        .find_map(|l| l.trim().strip_prefix("project(nghttp2 VERSION "))
        .and_then(|v| v.strip_suffix(')'))
        .expect("could not parse nghttp2 version from CMakeLists.txt")
        .trim()
        .to_string();
    let mut num = 0u32;
    for part in version.split('.') {
        num = (num << 8) | part.parse::<u32>().expect("numeric version part");
    }
    let rendered = tpl
        .replace("@PACKAGE_VERSION@", &version)
        .replace("@PACKAGE_VERSION_NUM@", &format!("{num:#08x}"));
    let shim = PathBuf::from(env::var("OUT_DIR").unwrap()).join("docsrs-include");
    std::fs::create_dir_all(shim.join("nghttp2")).expect("create docs.rs include shim");
    std::fs::write(shim.join("nghttp2/nghttp2ver.h"), rendered)
        .expect("write rendered nghttp2ver.h");
    // Two -I dirs are needed; fold them by symlink-free copy of nghttp2.h.
    std::fs::copy(
        includes.join("nghttp2/nghttp2.h"),
        shim.join("nghttp2/nghttp2.h"),
    )
    .expect("copy nghttp2.h into the docs.rs shim");
    shim
}

/// Make sure `vendor/nghttp2-src` has the nghttp2 source. The published
/// package ships it; a fresh git checkout may not have initialized the
/// submodule yet, so do what the docs say automatically (a local-pin
/// `git submodule update --init`, never an arbitrary network fetch).
fn ensure_bundled_source(manifest_dir: &Path) -> PathBuf {
    let src = manifest_dir.join("vendor/nghttp2-src");
    if src.join("CMakeLists.txt").exists() {
        return src;
    }
    let ran = std::process::Command::new("git")
        .arg("-C")
        .arg(manifest_dir)
        .args(["submodule", "update", "--init", "--", "vendor/nghttp2-src"])
        .status();
    match ran {
        Ok(s) if s.success() && src.join("CMakeLists.txt").exists() => src,
        _ => panic!(
            "grpcuds-sys: bundled nghttp2 source missing at {} and automatic \
             submodule init failed — run: git submodule update --init \
             rust/grpcuds-sys/vendor/nghttp2-src",
            src.display()
        ),
    }
}

/// Build libnghttp2 (lib-only, static) from the pinned submodule and emit the
/// static link directives. Returns the install `include` dir for bindgen.
#[cfg(feature = "bundled")]
fn build_bundled_nghttp2(manifest_dir: &std::path::Path) -> PathBuf {
    let src = ensure_bundled_source(manifest_dir);
    println!("cargo:rerun-if-changed={}", src.join("lib").display());

    // Core libnghttp2 needs no external deps; disable everything but the static
    // lib. Pin CMAKE_INSTALL_LIBDIR so the archive lands in a known `lib/` dir
    // (GNUInstallDirs would otherwise pick lib64 on some distros).
    let dst = cmake::Config::new(&src)
        .define("ENABLE_LIB_ONLY", "ON")
        .define("ENABLE_STATIC_LIB", "ON")
        .define("ENABLE_SHARED_LIB", "OFF")
        .define("ENABLE_DOC", "OFF")
        .define("ENABLE_APP", "OFF")
        .define("ENABLE_HPACK_TOOLS", "OFF")
        .define("ENABLE_EXAMPLES", "OFF")
        .define("BUILD_TESTING", "OFF")
        .define("CMAKE_INSTALL_LIBDIR", "lib")
        .build();

    println!(
        "cargo:rustc-link-search=native={}",
        dst.join("lib").display()
    );
    println!("cargo:rustc-link-lib=static=nghttp2");
    // Expose the built artifacts to dependents (e.g. for pkg-config generation).
    println!("cargo:root={}", dst.display());
    println!("cargo:include={}", dst.join("include").display());

    dst.join("include")
}

#[cfg(not(feature = "bundled"))]
fn build_bundled_nghttp2(_manifest_dir: &std::path::Path) -> PathBuf {
    unreachable!("build_bundled_nghttp2 is only called when the `bundled` feature is enabled")
}
