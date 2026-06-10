# SPDX-License-Identifier: MIT OR Apache-2.0
grpcuds_codegen(ble)

# gg — grpcuds server + grpcuds client, one self-checking process (needs BOTH
# C ABI halves).
grpcuds_example(ble-gg BOTH ble gg/main.cc ble_service_impl.cc)
if(TARGET ble-gg)
    add_test(NAME ble-gg COMMAND ble-gg)
endif()

# gt — grpcuds server binary (peer: rust tonic-client-ble).
grpcuds_example(ble-gt-server SERVER ble gt/server_main.cc ble_service_impl.cc)

# tg — grpcuds client binary, self-checking (peer: rust tonic-server-ble).
grpcuds_example(ble-tg-client CLIENT ble tg/client_main.cc)

# footprint measurement only (holds the connection idle, not in the matrix).
grpcuds_example(ble-measure-client CLIENT ble measure/client_main.cc)

# stock grpc++ peer cells (built only when grpc++ is present). The interop
# ctests only register when the grpcuds side they pair with was built.
if(GRPCPP_AVAILABLE)
    grpcpp_codegen(ble ble)
    grpcpp_example(ble-grpcpp-server ble grpcpp/server_main.cc)
    grpcpp_example(ble-grpcpp-client ble grpcpp/client_main.cc)
    if(TARGET ble-gt-server)
        add_test(NAME ble-grpcuds-server-grpcpp-client
                 COMMAND bash ${PROJECT_SOURCE_DIR}/cmake/interop_pair.sh
                         $<TARGET_FILE:ble-gt-server> $<TARGET_FILE:ble-grpcpp-client>)
    endif()
    if(TARGET ble-tg-client)
        add_test(NAME ble-grpcpp-server-grpcuds-client
                 COMMAND bash ${PROJECT_SOURCE_DIR}/cmake/interop_pair.sh
                         $<TARGET_FILE:ble-grpcpp-server> $<TARGET_FILE:ble-tg-client>)
    endif()
endif()
