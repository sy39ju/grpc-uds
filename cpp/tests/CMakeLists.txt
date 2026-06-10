# SPDX-License-Identifier: MIT OR Apache-2.0

# Helper: a grpcudspp test executable + ctest, with the common link libs.
function(grpcudspp_test NAME)
    add_executable(${NAME} ${NAME}.cc)
    target_link_libraries(${NAME} PRIVATE grpcudspp ${GRPCUDS_FFI} ${NGHTTP2_LIB} pthread dl m)
    add_test(NAME ${NAME} COMMAND ${NAME})
endfunction()

# The outbound mailbox now lives in the Rust C ABI (grpcuds-ffi-impl); its
# unit/race tests are in rust/grpcuds-ffi-impl/src/mailbox.rs.

# Server-side tests — built when the lib exports the server C ABI.
if(GRPCUDS_HAVE_SERVER)
    grpcudspp_test(echo_test)
    grpcudspp_test(outbound_thread_test)
    grpcudspp_test(producer_io_split_test)
    grpcudspp_test(server_thread_test)
    grpcudspp_test(grpc_message_test)
    grpcudspp_test(health_decode_test)
endif()

# Client wrapper test — runs a server AND a client in one process, so it needs
# BOTH halves (server-only or client-only lib builds skip it cleanly).
if(GRPCUDS_HAVE_SERVER AND GRPCUDS_HAVE_CLIENT)
    grpcudspp_test(client_test)
endif()
