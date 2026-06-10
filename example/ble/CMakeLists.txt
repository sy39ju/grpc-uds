# SPDX-License-Identifier: MIT OR Apache-2.0
#
# The grpcuds example — a complete grpcuds⇄grpcuds BLE service in two files
# (server_main.cc / client_main.cc). Auto-detects whether it sits inside the
# unpacked SDK (<dist>/example) or inside the git repo, and finds the
# headers / libgrpcuds_ffi.a / protoc plugin / nanopb accordingly. Every
# location is overridable with -D.
#
#   cmake -S . -B build && cmake --build build
#   ./run_demo.sh build                        # scan round-trip -> "example: OK"
#
# (In the repo, run ./build.sh first for the .a + plugin.)
cmake_minimum_required(VERSION 3.16)
project(grpcuds-ble-example CXX C)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

# Embedded-right default: optimize for size unless the caller chose.
if(NOT CMAKE_BUILD_TYPE)
    set(CMAKE_BUILD_TYPE MinSizeRel)
endif()

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

# Dead-strip unreferenced code — the realistic embedded link. Without it a
# client carries the server-only mailbox / health service from the static lib.
add_compile_options(-ffunction-sections -fdata-sections)
add_link_options(-Wl,--gc-sections)

if(EXISTS "${CMAKE_CURRENT_SOURCE_DIR}/../sdk/include/grpcuds.h")
    # ---- SDK layout: <dist>/example (sdk/, target/, host/ are siblings) ----
    set(_inc      "${CMAKE_CURRENT_SOURCE_DIR}/../sdk/include")
    set(_ffi      "${CMAKE_CURRENT_SOURCE_DIR}/../target/lib/libgrpcuds_ffi.a")
    set(_plugin   "${CMAKE_CURRENT_SOURCE_DIR}/../host/bin/protoc-gen-grpcudspp")
    set(_nanopb   "${CMAKE_CURRENT_SOURCE_DIR}/../sdk/nanopb")
    set(_proto    "${CMAKE_CURRENT_SOURCE_DIR}/../sdk/proto")
    set(GRPCUDS_INCLUDE_DIRS "${_inc}" CACHE STRING "grpcuds header dirs")
else()
    # ---- repo layout: <repo>/example/ble ----------------------------------
    set(_ffi      "${CMAKE_CURRENT_SOURCE_DIR}/../../rust/target/release/libgrpcuds_ffi.a")
    set(_plugin   "${CMAKE_CURRENT_SOURCE_DIR}/../../rust/target/release/protoc-gen-grpcudspp")
    set(_nanopb   "${CMAKE_CURRENT_SOURCE_DIR}/../nanopb")
    set(_proto    "${CMAKE_CURRENT_SOURCE_DIR}/proto")
    set(GRPCUDS_INCLUDE_DIRS
        "${CMAKE_CURRENT_SOURCE_DIR}/../../rust/grpcuds-ffi/include"
        "${CMAKE_CURRENT_SOURCE_DIR}/../../cpp/include"
        CACHE STRING "grpcuds header dirs")
endif()
set(GRPCUDS_FFI      "${_ffi}"    CACHE FILEPATH "libgrpcuds_ffi.a (built with the client feature)")
set(GRPCUDSPP_PLUGIN "${_plugin}" CACHE FILEPATH "host protoc-gen-grpcudspp")
set(NANOPB_DIR       "${_nanopb}" CACHE PATH     "nanopb runtime + generator")
set(PROTO_DIR        "${_proto}"  CACHE PATH     "directory holding ble.proto + ble.options")

foreach(_req IN ITEMS GRPCUDS_FFI GRPCUDSPP_PLUGIN)
    if(NOT EXISTS "${${_req}}")
        message(FATAL_ERROR "${_req} not found at ${${_req}} — in the repo run "
            "./build.sh first; in the SDK keep example/ inside the unpacked tree "
            "(or pass -D${_req}=...).")
    endif()
endforeach()
if(NOT EXISTS "${NANOPB_DIR}/pb_decode.h")
    message(FATAL_ERROR "nanopb not found at ${NANOPB_DIR} — "
        "git submodule update --init example/nanopb (or pass -DNANOPB_DIR=...)")
endif()

find_program(PROTOC protoc REQUIRED)
find_package(Python3 REQUIRED COMPONENTS Interpreter)
find_library(NGHTTP2_LIB NAMES nghttp2 REQUIRED)

# Which C ABI halves does the lib export? (Same probe as tests/cpp.)
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

# ---- codegen: ble.proto -> nanopb messages + grpcudspp service stubs ---------
set(GEN ${CMAKE_CURRENT_BINARY_DIR}/generated)
file(MAKE_DIRECTORY ${GEN})
add_custom_command(
    OUTPUT ${GEN}/ble.pb.h ${GEN}/ble.pb.c
    COMMAND ${Python3_EXECUTABLE} ${NANOPB_DIR}/generator/nanopb_generator.py
            -D ${GEN} -I ${PROTO_DIR}
            -f ${PROTO_DIR}/ble.options
            ${PROTO_DIR}/ble.proto
    DEPENDS ${PROTO_DIR}/ble.proto ${PROTO_DIR}/ble.options
    COMMENT "nanopb: ble.pb.{h,c}" VERBATIM)
add_custom_command(
    OUTPUT ${GEN}/ble.grpc.pb.h ${GEN}/ble.grpc.pb.cc
    COMMAND ${PROTOC} --plugin=protoc-gen-grpcudspp=${GRPCUDSPP_PLUGIN}
            --grpcudspp_out=${GEN} --proto_path=${PROTO_DIR}
            ${PROTO_DIR}/ble.proto
    DEPENDS ${PROTO_DIR}/ble.proto ${GRPCUDSPP_PLUGIN}
    COMMENT "grpcudspp: ble.grpc.pb.{h,cc}" VERBATIM)
add_custom_target(ble_example_gen DEPENDS
    ${GEN}/ble.pb.h ${GEN}/ble.pb.c ${GEN}/ble.grpc.pb.h ${GEN}/ble.grpc.pb.cc)

set(NANOPB_RUNTIME
    ${NANOPB_DIR}/pb_common.c ${NANOPB_DIR}/pb_decode.c ${NANOPB_DIR}/pb_encode.c)

# ---- targets ------------------------------------------------------------------
set(_targets)
if(NOT GRPCUDS_SIDE STREQUAL client)
    add_executable(ble-server server_main.cc ${GEN}/ble.pb.c ${GEN}/ble.grpc.pb.cc
                   ${NANOPB_RUNTIME})
    list(APPEND _targets ble-server)
endif()
if(NOT GRPCUDS_SIDE STREQUAL server)
    add_executable(ble-client client_main.cc ${GEN}/ble.pb.c ${NANOPB_RUNTIME})
    target_compile_definitions(ble-client PRIVATE GRPCUDSPP_HAVE_NANOPB)
    list(APPEND _targets ble-client)
endif()

foreach(t IN LISTS _targets)
    add_dependencies(${t} ble_example_gen)
    target_include_directories(${t} PRIVATE ${GEN} ${GRPCUDS_INCLUDE_DIRS} ${NANOPB_DIR})
    target_link_libraries(${t} PRIVATE ${GRPCUDS_FFI} ${NGHTTP2_LIB} pthread dl m)
endforeach()

enable_testing()
if(TARGET ble-server AND TARGET ble-client)
    add_test(NAME ble-example
             COMMAND bash ${CMAKE_CURRENT_SOURCE_DIR}/run_demo.sh
                     $<TARGET_FILE:ble-server> $<TARGET_FILE:ble-client>)
endif()
