# SPDX-License-Identifier: MIT OR Apache-2.0
#
# grpcudspp — C++ ServerBuilder-pattern wrapper over the grpcuds C ABI.
#
# This is a header-only INTERFACE library plus a tests target.
#
# The C runtime (grpcuds-ffi) must be built separately by Cargo before
# running CMake. Default expectation:
#
#     cd ../rust && cargo build --release
#     cd ../cpp  && cmake -S . -B build && cmake --build build
#     ctest --test-dir build

cmake_minimum_required(VERSION 3.16)
project(grpcudspp CXX)

# ---- binary hardening (on by default; -DGRPCUDS_HARDENING=OFF to disable) ----
# Distro host toolchains often default these; CROSS toolchains for embedded
# targets often do NOT — making them explicit guarantees PIE + stack
# protector + full RELRO + noexecstack on the device too.
option(GRPCUDS_HARDENING "PIE, stack protector, full RELRO, noexecstack" ON)
if(GRPCUDS_HARDENING)
    # CXX-only checks: this project() enables no C (everything here is .cc),
    # and flag checks for a language that is not enabled are a hard ERROR
    # on newer CMake (3.31+) when configured standalone.
    include(CheckPIESupported)
    check_pie_supported(OUTPUT_VARIABLE GRPCUDS_PIE_OUTPUT LANGUAGES CXX)
    if(NOT CMAKE_CXX_LINK_PIE_SUPPORTED)
        message(WARNING "GRPCUDS_HARDENING: this toolchain cannot link PIE; "
                        "binaries will not be position-independent. "
                        "${GRPCUDS_PIE_OUTPUT}")
    endif()
    set(CMAKE_POSITION_INDEPENDENT_CODE ON)
    include(CheckCXXCompilerFlag)
    check_cxx_compiler_flag(-fstack-protector-strong GRPCUDS_HAS_SP_STRONG)
    if(GRPCUDS_HAS_SP_STRONG)
        add_compile_options(-fstack-protector-strong)
    endif()
    add_link_options(-Wl,-z,relro -Wl,-z,now -Wl,-z,noexecstack)
endif()

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)

# ---- Header-only target ----------------------------------------------------
add_library(grpcudspp INTERFACE)
target_include_directories(grpcudspp INTERFACE
    ${CMAKE_CURRENT_SOURCE_DIR}/include
    ${CMAKE_CURRENT_SOURCE_DIR}/../rust/grpcuds-ffi/include
)

# ---- C ABI runtime (libgrpcuds_ffi) ---------------------------------------
# Prefer the release static lib; fall back to debug. Search .a first
# so the example links statically (no LD_LIBRARY_PATH needed at run time).
# Honor an explicit -DGRPCUDS_FFI=/abs/path override (e.g. a client-featured
# build); otherwise prefer release, fall back to debug.
if(NOT GRPCUDS_FFI)
    set(GRPCUDS_TARGET_DIR ${CMAKE_CURRENT_SOURCE_DIR}/../rust/target)
    set(_grpcuds_a ${GRPCUDS_TARGET_DIR}/release/libgrpcuds_ffi.a)
    if(NOT EXISTS ${_grpcuds_a})
        set(_grpcuds_a ${GRPCUDS_TARGET_DIR}/debug/libgrpcuds_ffi.a)
    endif()
    if(NOT EXISTS ${_grpcuds_a})
        message(FATAL_ERROR "libgrpcuds_ffi.a not found — run `cargo build --release` first")
    endif()
    set(GRPCUDS_FFI ${_grpcuds_a})
endif()
message(STATUS "Using grpcuds_ffi library: ${GRPCUDS_FFI}")

# nghttp2 (the C runtime dynamically links it)
find_library(NGHTTP2_LIB NAMES nghttp2 REQUIRED)

# ---- which C ABI halves does libgrpcuds_ffi export? -----------------------
# So a server-only or client-only lib (./build.sh --side ...) still
# configures cleanly. Override with -DGRPCUDS_SIDE=server|client|both.
if(NOT GRPCUDS_SIDE)
    execute_process(COMMAND nm "${GRPCUDS_FFI}" OUTPUT_VARIABLE _gu_nm ERROR_QUIET)
    if(_gu_nm MATCHES "grpcuds_server_bind_uds" AND _gu_nm MATCHES "grpcuds_client_connect")
        set(GRPCUDS_SIDE both)
    elseif(_gu_nm MATCHES "grpcuds_client_connect")
        set(GRPCUDS_SIDE client)
    else()
        set(GRPCUDS_SIDE server)
    endif()
endif()
set(GRPCUDS_HAVE_SERVER OFF)
set(GRPCUDS_HAVE_CLIENT OFF)
if(GRPCUDS_SIDE STREQUAL both OR GRPCUDS_SIDE STREQUAL server)
    set(GRPCUDS_HAVE_SERVER ON)
endif()
if(GRPCUDS_SIDE STREQUAL both OR GRPCUDS_SIDE STREQUAL client)
    set(GRPCUDS_HAVE_CLIENT ON)
endif()
message(STATUS "grpcuds C ABI side: ${GRPCUDS_SIDE}")

# ---- Tests ----------------------------------------------------------------
enable_testing()
add_subdirectory(tests)
