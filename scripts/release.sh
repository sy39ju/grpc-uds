#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# Publish the grpcuds workspace crates to a cargo registry, in dependency order.
#
#   ./scripts/release.sh              # DRY RUN (default): verify only, publish nothing
#   ./scripts/release.sh --execute    # actually publish (irreversible — see note below)
#   ./scripts/release.sh --help
#
# Why an ordered script: crates.io accepts one crate at a time and each crate's
# dependencies must already be on the index. grpcuds-core depends on
# grpcuds-sys, so sys must be live before core can even be verified. The order
# below encodes that. grpcuds-ffi / grpcuds-ffi-impl are `publish = false`
# (staticlib/cdylib + internal impl — not consumable via cargo), so they are
# skipped; their compiled .a/.so ship through other channels.
#
# NOTE ON OWNERSHIP: publishing is irreversible (a version can be yanked but
# never re-uploaded). This script defaults to a dry run and requires an explicit
# --execute. Even with --execute you must have a registry token configured
# (CARGO_REGISTRY_TOKEN or `cargo login`); the script never handles credentials.

set -euo pipefail

# --- locate the workspace --------------------------------------------------
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
WORKSPACE_DIR="$REPO_ROOT/rust"

# Publish order: dependencies first. Keep in sync with publishable members.
PUBLISH_ORDER=(grpcuds-sys grpcuds-core grpcuds grpcuds-build protoc-gen-grpcudspp)
# Crates whose dependency on an *unpublished* local crate means they can only be
# verified during the real ordered publish (not in a standalone dry run).
DEFERRED_VERIFY=(grpcuds-core grpcuds)

# --- args ------------------------------------------------------------------
EXECUTE=0
ALLOW_DIRTY=0
for arg in "$@"; do
    case "$arg" in
        --execute)     EXECUTE=1 ;;
        --allow-dirty) ALLOW_DIRTY=1 ;;
        -h|--help)
            awk 'NR>2 && /^#/ {sub(/^# ?/,""); print; next} NR>2 {exit}' "${BASH_SOURCE[0]}"
            exit 0 ;;
        *) echo "unknown argument: $arg (try --help)" >&2; exit 2 ;;
    esac
done

say()  { printf '\n\033[1m==> %s\033[0m\n' "$*"; }
warn() { printf '\033[33mwarning:\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[31merror:\033[0m %s\n' "$*" >&2; exit 1; }

cd "$WORKSPACE_DIR"

# --- preflight: the in-crate nghttp2 submodule must be present --------------
# `cargo package` silently packs whatever is on disk; without the submodule
# the published grpcuds-sys would ship a non-functional `bundled` feature.
if [[ ! -f "$WORKSPACE_DIR/grpcuds-sys/vendor/nghttp2-src/CMakeLists.txt" ]]; then
    say "Initializing the grpcuds-sys nghttp2 submodule"
    git -C "$REPO_ROOT" submodule update --init rust/grpcuds-sys/vendor/nghttp2-src \
        || die "could not init rust/grpcuds-sys/vendor/nghttp2-src — bundled sources would be missing from the package"
fi

# --- preflight: clean tree -------------------------------------------------
if [[ -n "$(git -C "$REPO_ROOT" status --porcelain)" ]]; then
    if [[ "$ALLOW_DIRTY" -eq 1 ]]; then
        warn "working tree is dirty; continuing because --allow-dirty was passed"
    else
        die "working tree is dirty. Commit or stash first, or pass --allow-dirty."
    fi
fi

# --- preflight: license gate (the GPL guard) -------------------------------
say "License & advisory gate (cargo-deny)"
if command -v cargo-deny >/dev/null 2>&1; then
    cargo deny check licenses bans sources advisories \
        || die "cargo deny failed — a dependency violates the license/ban policy in rust/deny.toml"
else
    warn "cargo-deny not installed (cargo install cargo-deny). Skipping the license gate."
    warn "The GPL/copyleft guard in rust/deny.toml is NOT enforced without it."
fi

# --- preflight: build + test -----------------------------------------------
say "Build + test the workspace"
cargo build --workspace --all-targets
cargo test  --workspace

# --- per-crate package verification (where possible) -----------------------
pkg_flags=(--no-verify)
[[ "$ALLOW_DIRTY" -eq 1 ]] && pkg_flags+=(--allow-dirty)

is_deferred() { local c; for c in "${DEFERRED_VERIFY[@]}"; do [[ "$c" == "$1" ]] && return 0; done; return 1; }

say "Packaging check (crates with no unpublished local deps)"
for crate in "${PUBLISH_ORDER[@]}"; do
    if is_deferred "$crate"; then
        echo "  - $crate: deferred (depends on a local crate not yet on the registry; verified during ordered publish)"
        continue
    fi
    echo "  - $crate: cargo package ${pkg_flags[*]}"
    cargo package "${pkg_flags[@]}" -p "$crate" >/dev/null
done

# --- publish ---------------------------------------------------------------
if [[ "$EXECUTE" -ne 1 ]]; then
    say "DRY RUN complete. Nothing was published."
    cat <<EOF

To publish for real (irreversible), ensure a registry token is configured, then:

    ./scripts/release.sh --execute

Publish order that will be used:
    ${PUBLISH_ORDER[*]}
(grpcuds-ffi / grpcuds-ffi-impl are publish=false and are skipped.)
EOF
    exit 0
fi

publish_flags=()
[[ "$ALLOW_DIRTY" -eq 1 ]] && publish_flags+=(--allow-dirty)

say "PUBLISHING (--execute). Each step waits for the index before the next."
for crate in "${PUBLISH_ORDER[@]}"; do
    say "publish $crate"
    # Modern cargo blocks until the new version is visible in the index, so the
    # next (dependent) crate resolves cleanly without a manual sleep.
    cargo publish "${publish_flags[@]}" -p "$crate"
done

say "All crates published: ${PUBLISH_ORDER[*]}"
