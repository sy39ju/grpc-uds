# SPDX-License-Identifier: MIT OR Apache-2.0
#
# CMake toolchain file for armv7 (hard-float) Linux cross builds.
#
# Used by BOTH cross flows in docs/BUILDING.md:
#   * the `bundled` nghttp2 build inside `cargo build --target armv7-...`
#     (the cmake crate picks it up via the CMAKE_TOOLCHAIN_FILE_<target> env)
#   * cross-building a C++ server (e.g. tests/cpp) directly with
#     `cmake -DCMAKE_TOOLCHAIN_FILE=...`
#
# Environment-driven so one file serves different setups:
#   CROSS_PREFIX  compiler prefix or absolute prefix path
#                 (default: arm-linux-gnueabihf-)
#   TOOLCHAIN_DIR directory holding the cross tools when they are NOT on
#                 PATH (e.g. /opt/gcc-arm-.../bin); also prepended to PATH
#   SYSROOT       target sysroot (optional but recommended; also what
#                 grpcuds-sys' build.rs reads for bindgen)

#
# NOTE (Debian/Ubuntu cross-package sysroots): Debian's cross `libc.so` is a
# linker SCRIPT with absolute paths (/usr/arm-linux-gnueabihf/lib/...). Under
# --sysroot (which CMAKE_SYSROOT passes to the linker) those resolve INSIDE
# the sysroot — "cannot find ... inside $SYSROOT" means mirror the path once:
#     mkdir -p "$SYSROOT/usr/arm-linux-gnueabihf"
#     ln -s ../lib "$SYSROOT/usr/arm-linux-gnueabihf/lib"

set(CMAKE_SYSTEM_NAME Linux)
set(CMAKE_SYSTEM_PROCESSOR armv7)

if(DEFINED ENV{CROSS_PREFIX})
    set(_cross "$ENV{CROSS_PREFIX}")
else()
    set(_cross "arm-linux-gnueabihf-")
endif()

# Toolchain not on PATH? Point TOOLCHAIN_DIR at the directory holding the
# tools (e.g. /opt/gcc-arm-.../bin): the compilers resolve absolutely and the
# dir is prepended to PATH so binutils invoked during the build resolve too.
# (CROSS_PREFIX may alternatively be an absolute prefix path itself.)
if(DEFINED ENV{TOOLCHAIN_DIR})
    if(NOT IS_ABSOLUTE "${_cross}")
        set(_cross "$ENV{TOOLCHAIN_DIR}/${_cross}")
    endif()
    set(ENV{PATH} "$ENV{TOOLCHAIN_DIR}:$ENV{PATH}")
endif()

set(CMAKE_C_COMPILER   "${_cross}gcc")
set(CMAKE_CXX_COMPILER "${_cross}g++")

if(DEFINED ENV{SYSROOT})
    set(CMAKE_SYSROOT "$ENV{SYSROOT}")
    # Let pkg-config (glib, BT stacks, ...) resolve inside the sysroot too.
    set(ENV{PKG_CONFIG_SYSROOT_DIR} "$ENV{SYSROOT}")
    set(ENV{PKG_CONFIG_LIBDIR}
        "$ENV{SYSROOT}/usr/lib/pkgconfig:$ENV{SYSROOT}/usr/share/pkgconfig:$ENV{SYSROOT}/usr/lib/arm-linux-gnueabihf/pkgconfig")
endif()

# Search libraries/headers only in the sysroot; host tools (protoc, the
# grpcudspp plugin, nanopb_generator) still come from the build machine.
set(CMAKE_FIND_ROOT_PATH_MODE_PROGRAM NEVER)
set(CMAKE_FIND_ROOT_PATH_MODE_LIBRARY ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_INCLUDE ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_PACKAGE ONLY)
