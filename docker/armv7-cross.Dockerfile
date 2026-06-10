# SPDX-License-Identifier: MIT OR Apache-2.0
#
# armv7 cross-build environment for grpcuds — the documented SYSROOT flow
# (docs/BUILDING.md, rust/bindings/armv7/README.md) made reproducible:
#
#   docker build -t grpcuds-armv7 -f docker/armv7-cross.Dockerfile docker/
#   docker run --rm -v "$PWD":/src:ro -v /tmp/armv7-out:/out grpcuds-armv7 \
#       /src/docker/armv7-build.sh
#
# Contents:
#   * arm-linux-gnueabihf gcc + armhf libc          (cross compile/link)
#   * /sysroot — armhf libc headers/libs + libnghttp2 from ubuntu-ports,
#     laid out the way grpcuds-sys/build.rs expects ($SYSROOT/usr/include/
#     nghttp2/nghttp2.h, $SYSROOT/usr/lib/.../libnghttp2.so)  → DYNAMIC link,
#     the project invariant
#   * rust stable + armv7 target, clang/libclang    (cargo + bindgen)
#   * qemu-user                                     (run the armv7 binaries)
FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl xz-utils \
        gcc-arm-linux-gnueabihf g++-arm-linux-gnueabihf libc6-dev-armhf-cross \
        clang libclang-dev \
        protobuf-compiler \
        cmake make python3 python3-protobuf \
        qemu-user file \
    && rm -rf /var/lib/apt/lists/*

# armhf packages live on ports.ubuntu.com; pin the default sources to amd64
# so `apt update` doesn't look for armhf indexes on archive.ubuntu.com.
RUN sed -i 's/^Components:/Architectures: amd64\nComponents:/' \
        /etc/apt/sources.list.d/ubuntu.sources \
    && printf '%s\n' \
        'Types: deb' \
        'URIs: http://ports.ubuntu.com/ubuntu-ports' \
        'Suites: noble noble-updates noble-security' \
        'Components: main universe' \
        'Architectures: armhf' \
        'Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg' \
        > /etc/apt/sources.list.d/armhf-ports.sources \
    && dpkg --add-architecture armhf \
    && apt-get update

# /sysroot: armhf libc (from the cross package) + libnghttp2 (from ports).
RUN mkdir -p /sysroot/usr /tmp/debs \
    && cp -a /usr/arm-linux-gnueabihf/include /sysroot/usr/include \
    && cp -a /usr/arm-linux-gnueabihf/lib /sysroot/usr/lib \
    && ln -s usr/lib /sysroot/lib \
    # Debian's cross libc.so is a linker script with ABSOLUTE paths
    # (/usr/arm-linux-gnueabihf/lib/...); under --sysroot the linker resolves
    # those inside the sysroot — mirror the path so they exist there.
    && mkdir -p /sysroot/usr/arm-linux-gnueabihf \
    && ln -s ../lib /sysroot/usr/arm-linux-gnueabihf/lib \
    && cd /tmp/debs \
    && apt-get download libnghttp2-14:armhf libnghttp2-dev:armhf \
    && for d in *.deb; do dpkg -x "$d" /sysroot; done \
    && test -f /sysroot/usr/include/nghttp2/nghttp2.h \
    && ls /sysroot/usr/lib/arm-linux-gnueabihf/libnghttp2.so \
    && rm -rf /tmp/debs && rm -rf /var/lib/apt/lists/*

# Rust (stable + the armv7 target, matching rust/rust-toolchain.toml).
ENV RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo \
    PATH=/opt/cargo/bin:/usr/local/bin:/usr/bin:/bin
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --profile minimal --default-toolchain stable \
            --component rustfmt --component clippy \
            --target armv7-unknown-linux-gnueabihf \
    && chmod -R a+rX /opt/rustup /opt/cargo

ENV SYSROOT=/sysroot \
    QEMU_LD_PREFIX=/sysroot
WORKDIR /work
