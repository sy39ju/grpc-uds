#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# Generate a concrete grpcuds.pc from rust/grpcuds-ffi/pkgconfig/grpcuds.pc.in
# for a chosen install prefix, so C/C++ consumers can `pkg-config --cflags
# --libs grpcuds`.
#
#   ./scripts/gen-pkgconfig.sh --prefix /usr/local
#   ./scripts/gen-pkgconfig.sh --prefix /opt/grpcuds --bundled
#   ./scripts/gen-pkgconfig.sh --prefix /usr/local --libdir lib64 -o out/grpcuds.pc
#
# Layout the generated .pc assumes under <prefix>:
#   <prefix>/<libdir>/libgrpcuds_ffi.a   (and libgrpcuds_ffi.so)
#   <prefix>/include/grpcuds.h
#   <prefix>/include/grpcudspp/*.h
#
# --bundled selects the static-nghttp2 variant: the runtime was built with the
# grpcuds `bundled` Cargo feature, which folds the nghttp2 objects INTO
# libgrpcuds_ffi.{a,so} itself — no system libnghttp2 is required or linked.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
template="${repo_root}/rust/grpcuds-ffi/pkgconfig/grpcuds.pc.in"

prefix=""
libdir="lib"
includedir=""
outfile=""
bundled=0

usage() {
    awk 'NR>2 && /^#/ {sub(/^# ?/,""); print; next} NR>2 {exit}' "${BASH_SOURCE[0]}"
    exit "${1:-0}"
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --prefix)     prefix="$2"; shift 2 ;;
        --libdir)     libdir="$2"; shift 2 ;;
        --includedir) includedir="$2"; shift 2 ;;
        -o|--output)  outfile="$2"; shift 2 ;;
        --bundled)    bundled=1; shift ;;
        -h|--help)    usage 0 ;;
        *) echo "unknown arg: $1" >&2; usage 1 ;;
    esac
done

[[ -n "$prefix" ]] || { echo "error: --prefix is required" >&2; usage 1; }
[[ -f "$template" ]] || { echo "error: template not found: $template" >&2; exit 1; }

# Version is the single source of truth in the workspace manifest.
version="$(sed -n 's/^version *= *"\(.*\)"/\1/p' "${repo_root}/rust/Cargo.toml" | head -1)"
[[ -n "$version" ]] || version="0.1.0"

# Normalise to absolute paths so the .pc works regardless of pkg-config's cwd.
case "$libdir" in /*) libdir_abs="$libdir" ;; *) libdir_abs="\${exec_prefix}/$libdir" ;; esac
if [[ -n "$includedir" ]]; then
    case "$includedir" in /*) includedir_abs="$includedir" ;; *) includedir_abs="\${prefix}/$includedir" ;; esac
else
    includedir_abs="\${prefix}/include"
fi

if [[ "$bundled" -eq 1 ]]; then
    # cargo folds the bundled static nghttp2 into libgrpcuds_ffi.{a,so}
    # itself, so the .pc must not name a libnghttp2 that is never shipped.
    nghttp2_requires=""
    nghttp2_libs=""
else
    nghttp2_requires=" libnghttp2"
    nghttp2_libs=""
fi

rendered="$(sed \
    -e "s|@PREFIX@|${prefix}|g" \
    -e "s|@LIBDIR@|${libdir_abs}|g" \
    -e "s|@INCLUDEDIR@|${includedir_abs}|g" \
    -e "s|@VERSION@|${version}|g" \
    -e "s|@NGHTTP2_REQUIRES@|${nghttp2_requires}|g" \
    -e "s|@NGHTTP2_LIBS@|${nghttp2_libs}|g" \
    "$template")"

if [[ -n "$outfile" ]]; then
    mkdir -p "$(dirname "$outfile")"
    printf '%s\n' "$rendered" > "$outfile"
    echo "wrote $outfile"
else
    printf '%s\n' "$rendered"
fi
