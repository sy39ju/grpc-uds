# SPDX-License-Identifier: MIT OR Apache-2.0
#
# grpcuds_codegen(<domain>) — run nanopb + protoc-gen-grpcudspp on
# proto/<domain>.proto, once. Sets two parent-scope variables:
#   <domain>_GEN_DIR      include dir holding <domain>.pb.h / <domain>.grpc.pb.h
#   <domain>_GEN_SOURCES  .c/.cc to compile (nanopb msgs + grpcudspp stubs +
#                         the nanopb runtime)
#
# Requires (set by the top-level CMakeLists): PROTO_ROOT, NANOPB_DIR,
# NANOPB_GENERATOR, PROTOC, GRPCUDSPP_PLUGIN.
function(grpcuds_codegen DOMAIN)
    set(GEN_DIR ${CMAKE_CURRENT_BINARY_DIR}/generated)
    file(MAKE_DIRECTORY ${GEN_DIR})

    set(PROTO   ${PROTO_ROOT}/${DOMAIN}.proto)
    set(OPTS    ${PROTO_ROOT}/${DOMAIN}.options)
    set(PB_H    ${GEN_DIR}/${DOMAIN}.pb.h)
    set(PB_C    ${GEN_DIR}/${DOMAIN}.pb.c)
    set(GRPC_H  ${GEN_DIR}/${DOMAIN}.grpc.pb.h)
    set(GRPC_CC ${GEN_DIR}/${DOMAIN}.grpc.pb.cc)

    add_custom_command(
        OUTPUT ${PB_H} ${PB_C}
        COMMAND ${NANOPB_GENERATOR} -D ${GEN_DIR} -I ${PROTO_ROOT} -f ${OPTS} ${PROTO}
        DEPENDS ${PROTO} ${OPTS}
        COMMENT "nanopb: ${DOMAIN}.pb.{h,c}"
        VERBATIM)

    add_custom_command(
        OUTPUT ${GRPC_H} ${GRPC_CC}
        COMMAND ${PROTOC}
                --plugin=protoc-gen-grpcudspp=${GRPCUDSPP_PLUGIN}
                --grpcudspp_out=${GEN_DIR}
                --proto_path=${PROTO_ROOT}
                ${PROTO}
        DEPENDS ${PROTO} ${GRPCUDSPP_PLUGIN}
        COMMENT "grpcudspp: ${DOMAIN}.grpc.pb.{h,cc}"
        VERBATIM)

    # One target OWNS the codegen. The outputs are sources of several
    # executables; without this, the Makefile generator duplicates the custom
    # command into every consuming target and parallel builds run the
    # generator concurrently — a compile can then read a half-written file
    # (seen in CI as nanopb's "#error Regenerate this file ...").
    add_custom_target(${DOMAIN}_grpcuds_gen
        DEPENDS ${PB_H} ${PB_C} ${GRPC_H} ${GRPC_CC})

    set(${DOMAIN}_GEN_DIR ${GEN_DIR} PARENT_SCOPE)
    # nanopb codec — message structs + descriptors, needed by client AND server.
    set(${DOMAIN}_GEN_NANOPB
        ${PB_C}
        ${NANOPB_DIR}/pb_common.c ${NANOPB_DIR}/pb_decode.c ${NANOPB_DIR}/pb_encode.c
        PARENT_SCOPE)
    # grpcudspp service stubs — reference the SERVER C ABI, so server side only.
    set(${DOMAIN}_GEN_SERVICE ${GRPC_CC} PARENT_SCOPE)
endfunction()

# grpcuds_example(<target> <SERVER|CLIENT|BOTH> <domain> <sources...>) — declare
# an example binary with the shared include dirs / link libs / codegen sources.
# Skipped when the linked lib lacks the needed C ABI half (GRPCUDS_HAVE_SERVER /
# GRPCUDS_HAVE_CLIENT). CLIENT/BOTH targets get GRPCUDSPP_HAVE_NANOPB (the typed
# client.h API).
function(grpcuds_example TARGET KIND DOMAIN)
    if(KIND STREQUAL "SERVER" AND NOT GRPCUDS_HAVE_SERVER)
        return()
    elseif(KIND STREQUAL "CLIENT" AND NOT GRPCUDS_HAVE_CLIENT)
        return()
    elseif(KIND STREQUAL "BOTH" AND NOT (GRPCUDS_HAVE_SERVER AND GRPCUDS_HAVE_CLIENT))
        return()
    endif()
    # Clients need only the nanopb codec; the grpcudspp service stubs are
    # server-side (they reference the server C ABI), so omit them for CLIENT.
    set(_gen ${${DOMAIN}_GEN_NANOPB})
    if(NOT KIND STREQUAL "CLIENT")
        list(APPEND _gen ${${DOMAIN}_GEN_SERVICE})
    endif()
    add_executable(${TARGET} ${ARGN} ${_gen})
    # Serialize behind the single codegen target (see grpcuds_codegen).
    add_dependencies(${TARGET} ${DOMAIN}_grpcuds_gen)
    target_include_directories(${TARGET} PRIVATE
        ${${DOMAIN}_GEN_DIR}
        ${GRPCUDSPP_INCLUDE}
        ${FFI_INCLUDE}
        ${NANOPB_DIR}
        ${PROJECT_SOURCE_DIR}/common
        ${CMAKE_CURRENT_SOURCE_DIR})
    if(NOT KIND STREQUAL "SERVER")
        target_compile_definitions(${TARGET} PRIVATE GRPCUDSPP_HAVE_NANOPB)
    endif()
    target_link_libraries(${TARGET} PRIVATE ${GRPCUDS_FFI} ${NGHTTP2_LIB} pthread dl m)
endfunction()

# ---- stock grpc++ peers (only when find_package(gRPC) succeeded) ------------

# grpcpp_codegen(<domain> <proto-basename>) — protobuf-full C++ + grpc_cpp_plugin
# stubs into a SEPARATE dir so they don't collide with the nanopb <domain>.pb.h.
function(grpcpp_codegen DOMAIN PROTO_BASENAME)
    set(GEN ${CMAKE_CURRENT_BINARY_DIR}/generated_grpcpp)
    file(MAKE_DIRECTORY ${GEN})
    set(PROTO ${PROTO_ROOT}/${PROTO_BASENAME}.proto)
    set(PB     ${GEN}/${PROTO_BASENAME}.pb.cc)
    set(GRPC   ${GEN}/${PROTO_BASENAME}.grpc.pb.cc)
    add_custom_command(
        OUTPUT ${PB} ${GEN}/${PROTO_BASENAME}.pb.h
        COMMAND ${PROTOC} --cpp_out=${GEN} -I ${PROTO_ROOT} ${PROTO}
        DEPENDS ${PROTO}
        COMMENT "protobuf: ${PROTO_BASENAME}.pb.{h,cc}"
        VERBATIM)
    add_custom_command(
        OUTPUT ${GRPC} ${GEN}/${PROTO_BASENAME}.grpc.pb.h
        COMMAND ${PROTOC} --grpc_out=${GEN}
                --plugin=protoc-gen-grpc=${GRPC_CPP_PLUGIN} -I ${PROTO_ROOT} ${PROTO}
        DEPENDS ${PROTO} ${GRPC_CPP_PLUGIN}
        COMMENT "grpc++: ${PROTO_BASENAME}.grpc.pb.{h,cc}"
        VERBATIM)
    # One owner for the grpc++ codegen too (server + client both consume it).
    add_custom_target(${DOMAIN}_grpcpp_gen
        DEPENDS ${PB} ${GEN}/${PROTO_BASENAME}.pb.h
                ${GRPC} ${GEN}/${PROTO_BASENAME}.grpc.pb.h)
    set(${DOMAIN}_GRPCPP_DIR ${GEN} PARENT_SCOPE)
    set(${DOMAIN}_GRPCPP_SOURCES ${PB} ${GRPC} PARENT_SCOPE)
endfunction()

# grpcpp_example(<target> <domain> <sources...>) — a stock grpc++ binary.
function(grpcpp_example TARGET DOMAIN)
    add_executable(${TARGET} ${ARGN} ${${DOMAIN}_GRPCPP_SOURCES})
    add_dependencies(${TARGET} ${DOMAIN}_grpcpp_gen)
    target_include_directories(${TARGET} PRIVATE
        ${${DOMAIN}_GRPCPP_DIR}
        ${PROJECT_SOURCE_DIR}/common
        ${CMAKE_CURRENT_SOURCE_DIR})
    target_link_libraries(${TARGET} PRIVATE gRPC::grpc++ protobuf::libprotobuf)
endfunction()
