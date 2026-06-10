# SPDX-License-Identifier: MIT OR Apache-2.0
#
# The plain-C example — a complete grpcuds server + client in two .c files,
# no C++ anywhere. Auto-detects whether it sits inside the unpacked SDK
# (<dist>/example-c) or inside the git repo, like the C++ example.
#
#   cmake -S . -B build && cmake --build build
#   ./run_demo.sh build                  # BLE scan round-trip -> "example: OK"
#
# (In the repo, run ./build.sh first for the .a.)
cmake_minimum_required(VERSION 3.16)
project(grpcuds-ble-c C)

set(CMAKE_C_STANDARD 11)
set(CMAKE_C_STANDARD_REQUIRED ON)

if(NOT CMAKE_BUILD_TYPE)
    set(CMAKE_BUILD_TYPE MinSizeRel)
endif()

# ---- binary hardening (on by default; -DGRPCUDS_HARDENING=OFF to disable) ----
option(GRPCUDS_HARDENING "PIE, stack protector, full RELRO, noexecstack" ON)
if(GRPCUDS_HARDENING)
    include(CheckPIESupported)
    check_pie_supported(OUTPUT_VARIABLE GRPCUDS_PIE_OUTPUT LANGUAGES C)
    if(NOT CMAKE_C_LINK_PIE_SUPPORTED)
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
    # ---- SDK layout: <dist>/example-c (sdk/, target/, host/ are siblings) -
    set(_inc    "${CMAKE_CURRENT_SOURCE_DIR}/../sdk/include")
    set(_ffi    "${CMAKE_CURRENT_SOURCE_DIR}/../target/lib/libgrpcuds_ffi.a")
    set(_nanopb "${CMAKE_CURRENT_SOURCE_DIR}/../sdk/nanopb")
    set(_plugin "${CMAKE_CURRENT_SOURCE_DIR}/../host/bin/protoc-gen-grpcudspp")
    set(_proto  "${CMAKE_CURRENT_SOURCE_DIR}/../sdk/proto")
else()
    # ---- repo layout: <repo>/example/c -------------------------------------
    set(_inc    "${CMAKE_CURRENT_SOURCE_DIR}/../../rust/grpcuds-ffi/include")
    set(_ffi    "${CMAKE_CURRENT_SOURCE_DIR}/../../rust/target/release/libgrpcuds_ffi.a")
    set(_nanopb "${CMAKE_CURRENT_SOURCE_DIR}/../nanopb")
    set(_plugin "${CMAKE_CURRENT_SOURCE_DIR}/../../rust/target/release/protoc-gen-grpcudspp")
    set(_proto  "${CMAKE_CURRENT_SOURCE_DIR}/../ble/proto")
endif()
set(GRPCUDS_INCLUDE_DIRS "${_inc}"    CACHE STRING   "grpcuds header dirs")
set(GRPCUDS_FFI          "${_ffi}"    CACHE FILEPATH "libgrpcuds_ffi.a (built with the client feature)")
set(NANOPB_DIR           "${_nanopb}" CACHE PATH     "nanopb runtime + generator")
set(GRPCUDSPP_PLUGIN     "${_plugin}" CACHE FILEPATH "host protoc-gen-grpcudspp (C mode)")
# The same BLE contract the C++ example uses (shipped in sdk/proto), so the
# two examples show the identical service in C and in C++.
set(PROTO_DIR "${_proto}" CACHE PATH "directory holding ble.proto + ble.options")

if(NOT EXISTS "${GRPCUDS_FFI}")
    message(FATAL_ERROR "GRPCUDS_FFI not found at ${GRPCUDS_FFI} — in the repo "
        "run ./build.sh first; in the SDK keep example-c/ inside the unpacked "
        "tree (or pass -DGRPCUDS_FFI=...).")
endif()
if(NOT EXISTS "${NANOPB_DIR}/pb_decode.h")
    message(FATAL_ERROR "nanopb not found at ${NANOPB_DIR} — "
        "git submodule update --init example/nanopb (or pass -DNANOPB_DIR=...)")
endif()
if(NOT EXISTS "${GRPCUDSPP_PLUGIN}")
    message(FATAL_ERROR "protoc-gen-grpcudspp not found at ${GRPCUDSPP_PLUGIN} — "
        "in the repo run ./build.sh plugin; in the SDK keep example-c/ inside "
        "the unpacked tree (or pass -DGRPCUDSPP_PLUGIN=...).")
endif()

find_package(Python3 REQUIRED COMPONENTS Interpreter)
find_program(PROTOC protoc REQUIRED)
find_library(NGHTTP2_LIB NAMES nghttp2 REQUIRED)

# Which C ABI halves does the lib export? (Same probe as the C++ example.)
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

# ---- codegen: ble.proto -> nanopb messages + plain-C service stubs ----------
# nanopb makes the message structs; protoc-gen-grpcudspp (C mode) makes the
# service table + typed client wrappers (ble.grpcuds.{h,c}).
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
    OUTPUT ${GEN}/ble.grpcuds.h ${GEN}/ble.grpcuds.c
    COMMAND ${PROTOC} --plugin=protoc-gen-grpcudspp=${GRPCUDSPP_PLUGIN}
            --grpcudspp_out=${GEN} --grpcudspp_opt=c
            --proto_path=${PROTO_DIR} ${PROTO_DIR}/ble.proto
    DEPENDS ${PROTO_DIR}/ble.proto ${GRPCUDSPP_PLUGIN}
    COMMENT "grpcudspp (C mode): ble.grpcuds.{h,c}" VERBATIM)
add_custom_target(ble_c_gen DEPENDS
    ${GEN}/ble.pb.h ${GEN}/ble.pb.c ${GEN}/ble.grpcuds.h ${GEN}/ble.grpcuds.c)

set(NANOPB_RUNTIME
    ${NANOPB_DIR}/pb_common.c ${NANOPB_DIR}/pb_decode.c ${NANOPB_DIR}/pb_encode.c)

# ---- targets ------------------------------------------------------------------
# The generated client wrappers are static inline in ble.grpcuds.h, so the
# client compiles the header only; the server also compiles ble.grpcuds.c
# (the trampolines + registration).
set(_targets)
if(NOT GRPCUDS_SIDE STREQUAL client)
    add_executable(ble-c-server server.c ${GEN}/ble.pb.c ${GEN}/ble.grpcuds.c
                   ${NANOPB_RUNTIME})
    list(APPEND _targets ble-c-server)
endif()
if(NOT GRPCUDS_SIDE STREQUAL server)
    add_executable(ble-c-client client.c ${GEN}/ble.pb.c ${NANOPB_RUNTIME})
    list(APPEND _targets ble-c-client)
endif()

foreach(t IN LISTS _targets)
    add_dependencies(${t} ble_c_gen)
    target_include_directories(${t} PRIVATE ${GEN} ${GRPCUDS_INCLUDE_DIRS} ${NANOPB_DIR})
    target_link_libraries(${t} PRIVATE ${GRPCUDS_FFI} ${NGHTTP2_LIB} pthread dl m)
endforeach()

enable_testing()
if(TARGET ble-c-server AND TARGET ble-c-client)
    add_test(NAME ble-c-example
             COMMAND bash ${CMAKE_CURRENT_SOURCE_DIR}/run_demo.sh
                     $<TARGET_FILE:ble-c-server> $<TARGET_FILE:ble-c-client>)
endif()
