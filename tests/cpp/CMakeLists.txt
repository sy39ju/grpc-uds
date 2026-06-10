# SPDX-License-Identifier: MIT OR Apache-2.0
#
# grpcuds C++ example matrix: 3 domains × 3 transport combos.
#   <domain>-gg   grpcuds server + grpcuds client (one process, self-checking ctest)
#   <domain>-gt-server   grpcuds server binary (peer = rust tonic-client-<domain>)
#   <domain>-tg-client   grpcuds client binary, self-checking (peer = rust tonic-server-<domain>)
#
# Prereqs (built by Cargo before cmake):
#   cd ../../rust && cargo build --release -p grpcuds-ffi --features client
#   cd ../../rust && cargo build --release -p protoc-gen-grpcudspp
#   git submodule update --init example/nanopb   (auto-detected; or -DNANOPB_DIR=...)
cmake_minimum_required(VERSION 3.16)
project(grpcuds-examples CXX C)

# ---- binary hardening (on by default; -DGRPCUDS_HARDENING=OFF to disable) ----
# Distro host toolchains often default these; CROSS toolchains for embedded
# targets often do NOT — making them explicit guarantees PIE + stack
# protector + full RELRO + noexecstack on the device too.
option(GRPCUDS_HARDENING "PIE, stack protector, full RELRO, noexecstack" ON)
if(GRPCUDS_HARDENING)
    include(CheckPIESupported)
    check_pie_supported(OUTPUT_VARIABLE GRPCUDS_PIE_OUTPUT LANGUAGES C CXX)
    if(NOT CMAKE_C_LINK_PIE_SUPPORTED OR NOT CMAKE_CXX_LINK_PIE_SUPPORTED)
        message(WARNING "GRPCUDS_HARDENING: this toolchain cannot link PIE; "
                        "binaries will not be position-independent. "
                        "${GRPCUDS_PIE_OUTPUT}")
    endif()
    set(CMAKE_POSITION_INDEPENDENT_CODE ON)
    include(CheckCCompilerFlag)
    check_c_compiler_flag(-fstack-protector-strong GRPCUDS_HAS_SP_STRONG)
    if(GRPCUDS_HAS_SP_STRONG)
        add_compile_options(-fstack-protector-strong)
    endif()
    add_link_options(-Wl,-z,relro -Wl,-z,now -Wl,-z,noexecstack)
endif()

# Dead-strip unreferenced code so the footprint numbers reflect a realistic
# embedded link (a client does not carry the server-only mailbox / health).
add_compile_options(-ffunction-sections -fdata-sections)
add_link_options(-Wl,--gc-sections)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)

set(PROTO_ROOT        ${CMAKE_CURRENT_SOURCE_DIR}/proto)
set(GRPCUDSPP_INCLUDE ${CMAKE_CURRENT_SOURCE_DIR}/../../cpp/include)
set(FFI_INCLUDE       ${CMAKE_CURRENT_SOURCE_DIR}/../../rust/grpcuds-ffi/include)

# ---- nanopb ----------------------------------------------------------------
# Defaults to the bundled submodule at example/nanopb; override with
# -DNANOPB_DIR=/abs/path or the NANOPB_DIR env var.
if(NOT NANOPB_DIR)
    set(NANOPB_DIR "$ENV{NANOPB_DIR}")
endif()
if(NOT NANOPB_DIR AND EXISTS "${CMAKE_CURRENT_SOURCE_DIR}/../../example/nanopb/pb_decode.h")
    set(NANOPB_DIR "${CMAKE_CURRENT_SOURCE_DIR}/../../example/nanopb")
endif()
if(NOT NANOPB_DIR OR NOT EXISTS "${NANOPB_DIR}/pb_decode.h")
    message(FATAL_ERROR
        "nanopb not found. Init the bundled submodule:\n"
        "    git submodule update --init example/nanopb\n"
        "or pass -DNANOPB_DIR=/abs/path to a nanopb checkout (pb.h, pb_decode.c, generator/).")
endif()
message(STATUS "NANOPB_DIR: ${NANOPB_DIR}")
if(EXISTS "${NANOPB_DIR}/generator/nanopb_generator.py")
    find_package(Python3 REQUIRED COMPONENTS Interpreter)
    set(NANOPB_GENERATOR ${Python3_EXECUTABLE} "${NANOPB_DIR}/generator/nanopb_generator.py")
else()
    find_program(NANOPB_GENERATOR_EXE nanopb_generator HINTS $ENV{HOME}/.local/bin REQUIRED)
    set(NANOPB_GENERATOR ${NANOPB_GENERATOR_EXE})
endif()

# ---- protoc + plugin + ffi + nghttp2 --------------------------------------
find_program(PROTOC protoc REQUIRED)
set(GRPCUDSPP_PLUGIN ${CMAKE_CURRENT_SOURCE_DIR}/../../rust/target/release/protoc-gen-grpcudspp
    CACHE FILEPATH "Path to the host protoc-gen-grpcudspp binary")
if(NOT EXISTS ${GRPCUDSPP_PLUGIN})
    message(FATAL_ERROR "protoc-gen-grpcudspp not found at ${GRPCUDSPP_PLUGIN} — "
        "cd rust && cargo build --release -p protoc-gen-grpcudspp")
endif()
# The grpcuds-client cells need the client ABI → build with --features client.
set(GRPCUDS_FFI ${CMAKE_CURRENT_SOURCE_DIR}/../../rust/target/release/libgrpcuds_ffi.a
    CACHE FILEPATH "Path to libgrpcuds_ffi.a (built --features client)")
if(NOT EXISTS ${GRPCUDS_FFI})
    message(FATAL_ERROR "libgrpcuds_ffi.a not found at ${GRPCUDS_FFI} — "
        "cd rust && cargo build --release -p grpcuds-ffi --features client")
endif()
find_library(NGHTTP2_LIB NAMES nghttp2 REQUIRED)

# ---- which C ABI halves does the linked libgrpcuds_ffi.a export? -----------
# Build only the matching cells, so a server-only or client-only lib (from
# `./build.sh --side server|client`) still configures + builds cleanly.
# Override with -DGRPCUDS_SIDE=server|client|both.
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
message(STATUS "grpcuds C ABI side: ${GRPCUDS_SIDE} (server=${GRPCUDS_HAVE_SERVER} client=${GRPCUDS_HAVE_CLIENT})")

# ---- stock grpc++ (optional) ----------------------------------------------
# The stock-gRPC peer cells build only when grpc++ + protobuf-full + the C++
# plugin are present; otherwise they're skipped and the matrix still builds.
find_package(gRPC CONFIG QUIET)
find_package(Protobuf QUIET)
find_program(GRPC_CPP_PLUGIN grpc_cpp_plugin)
if(gRPC_FOUND AND Protobuf_FOUND AND GRPC_CPP_PLUGIN)
    set(GRPCPP_AVAILABLE ON)
    message(STATUS "stock grpc++ ${gRPC_VERSION} found — building grpc++ peer cells")
else()
    set(GRPCPP_AVAILABLE OFF)
    message(STATUS "stock grpc++ not found — skipping grpc++ peer cells")
endif()

include(${CMAKE_CURRENT_SOURCE_DIR}/cmake/grpcuds_codegen.cmake)

enable_testing()
add_subdirectory(ble)
add_subdirectory(agent)
add_subdirectory(x509)
add_subdirectory(cgen)
