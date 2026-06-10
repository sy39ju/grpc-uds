# SPDX-License-Identifier: MIT OR Apache-2.0
#
# C-mode codegen end-to-end: ble.proto -> ble.grpcuds.{h,c} (plain C via
# `--grpcudspp_opt=c`), compiled as C11 into one self-checking binary that
# runs server (poll loop) + forked C client against the generated stubs.
if(NOT (GRPCUDS_HAVE_SERVER AND GRPCUDS_HAVE_CLIENT))
    return()
endif()

set(GEN_DIR ${CMAKE_CURRENT_BINARY_DIR}/generated)
file(MAKE_DIRECTORY ${GEN_DIR})

add_custom_command(
    OUTPUT ${GEN_DIR}/ble.pb.h ${GEN_DIR}/ble.pb.c
    COMMAND ${NANOPB_GENERATOR} -D ${GEN_DIR} -I ${PROTO_ROOT}
            -f ${PROTO_ROOT}/ble.options ${PROTO_ROOT}/ble.proto
    DEPENDS ${PROTO_ROOT}/ble.proto ${PROTO_ROOT}/ble.options
    COMMENT "nanopb (cgen): ble.pb.{h,c}" VERBATIM)
add_custom_command(
    OUTPUT ${GEN_DIR}/ble.grpcuds.h ${GEN_DIR}/ble.grpcuds.c
    COMMAND ${PROTOC} --plugin=protoc-gen-grpcudspp=${GRPCUDSPP_PLUGIN}
            --grpcudspp_out=${GEN_DIR} --grpcudspp_opt=c
            --proto_path=${PROTO_ROOT} ${PROTO_ROOT}/ble.proto
    DEPENDS ${PROTO_ROOT}/ble.proto ${GRPCUDSPP_PLUGIN}
    COMMENT "grpcudspp (C mode): ble.grpcuds.{h,c}" VERBATIM)
add_custom_target(cgen_grpcuds_gen DEPENDS
    ${GEN_DIR}/ble.pb.h ${GEN_DIR}/ble.pb.c
    ${GEN_DIR}/ble.grpcuds.h ${GEN_DIR}/ble.grpcuds.c)

set(NANOPB_RT
    ${NANOPB_DIR}/pb_common.c ${NANOPB_DIR}/pb_decode.c ${NANOPB_DIR}/pb_encode.c)

add_executable(ble-cgen-c main.c ${GEN_DIR}/ble.pb.c ${GEN_DIR}/ble.grpcuds.c
    ${NANOPB_RT})
add_dependencies(ble-cgen-c cgen_grpcuds_gen)
target_include_directories(ble-cgen-c PRIVATE
    ${GEN_DIR} ${FFI_INCLUDE} ${NANOPB_DIR})
target_link_libraries(ble-cgen-c PRIVATE ${GRPCUDS_FFI} ${NGHTTP2_LIB} pthread dl m)

add_test(NAME ble-cgen-c COMMAND ble-cgen-c)

# ---- generated-C client vs real servers (the conformance pairs) -------------
add_executable(ble-cgen-client client_main.c ${GEN_DIR}/ble.pb.c ${NANOPB_RT})
add_dependencies(ble-cgen-client cgen_grpcuds_gen)
target_include_directories(ble-cgen-client PRIVATE
    ${GEN_DIR} ${FFI_INCLUDE} ${NANOPB_DIR})
target_link_libraries(ble-cgen-client PRIVATE ${GRPCUDS_FFI} ${NGHTTP2_LIB} pthread dl m)

if(TARGET ble-gt-server)
    add_test(NAME ble-cpp-server-cgen-client
             COMMAND bash ${PROJECT_SOURCE_DIR}/cmake/interop_pair.sh
                     $<TARGET_FILE:ble-gt-server> $<TARGET_FILE:ble-cgen-client>)
endif()
if(GRPCPP_AVAILABLE AND TARGET ble-grpcpp-server)
    # Generated-C client against STOCK grpc++ — wire conformance.
    add_test(NAME ble-grpcpp-server-cgen-client
             COMMAND bash ${PROJECT_SOURCE_DIR}/cmake/interop_pair.sh
                     $<TARGET_FILE:ble-grpcpp-server> $<TARGET_FILE:ble-cgen-client>)
endif()

# ---- emitter edge shapes under -Werror ---------------------------------------
# edge.proto: no package, two services, nested types, req == resp, same RPC
# name twice. Built as strict C11 with a small scratch override — the
# generated code must be warning-clean.
add_custom_command(
    OUTPUT ${GEN_DIR}/edge.pb.h ${GEN_DIR}/edge.pb.c
    COMMAND ${NANOPB_GENERATOR} -D ${GEN_DIR} -I ${CMAKE_CURRENT_SOURCE_DIR}
            ${CMAKE_CURRENT_SOURCE_DIR}/edge.proto
    DEPENDS ${CMAKE_CURRENT_SOURCE_DIR}/edge.proto
    COMMENT "nanopb (cgen): edge.pb.{h,c}" VERBATIM)
add_custom_command(
    OUTPUT ${GEN_DIR}/edge.grpcuds.h ${GEN_DIR}/edge.grpcuds.c
    COMMAND ${PROTOC} --plugin=protoc-gen-grpcudspp=${GRPCUDSPP_PLUGIN}
            --grpcudspp_out=${GEN_DIR} --grpcudspp_opt=c
            --proto_path=${CMAKE_CURRENT_SOURCE_DIR}
            ${CMAKE_CURRENT_SOURCE_DIR}/edge.proto
    DEPENDS ${CMAKE_CURRENT_SOURCE_DIR}/edge.proto ${GRPCUDSPP_PLUGIN}
    COMMENT "grpcudspp (C mode): edge.grpcuds.{h,c}" VERBATIM)
add_custom_target(cgen_edge_gen DEPENDS
    ${GEN_DIR}/edge.pb.h ${GEN_DIR}/edge.pb.c
    ${GEN_DIR}/edge.grpcuds.h ${GEN_DIR}/edge.grpcuds.c)

add_executable(edge-cgen-compile edge_compile.c ${GEN_DIR}/edge.pb.c
    ${GEN_DIR}/edge.grpcuds.c ${NANOPB_RT})
add_dependencies(edge-cgen-compile cgen_edge_gen)
target_include_directories(edge-cgen-compile PRIVATE
    ${GEN_DIR} ${FFI_INCLUDE} ${NANOPB_DIR})
target_compile_options(edge-cgen-compile PRIVATE -Wall -Wextra -Werror)
target_compile_definitions(edge-cgen-compile PRIVATE GRPCUDSC_MAX_MESSAGE_SIZE=64)
target_link_libraries(edge-cgen-compile PRIVATE ${GRPCUDS_FFI} ${NGHTTP2_LIB} pthread dl m)

add_test(NAME edge-cgen-compile COMMAND edge-cgen-compile)
