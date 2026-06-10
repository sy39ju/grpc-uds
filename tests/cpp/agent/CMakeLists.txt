# SPDX-License-Identifier: MIT OR Apache-2.0
# C++ agent uses agent_cpp.proto (the Agent service only — no Assistant oneof).
grpcuds_codegen(agent_cpp)

grpcuds_example(agent-gg BOTH agent_cpp gg/main.cc agent_service_impl.cc)
if(TARGET agent-gg)
    add_test(NAME agent-gg COMMAND agent-gg)
endif()

grpcuds_example(agent-gt-server SERVER agent_cpp gt/server_main.cc agent_service_impl.cc)

grpcuds_example(agent-tg-client CLIENT agent_cpp tg/client_main.cc)

# footprint measurement only (holds the connection idle, not in the matrix).
grpcuds_example(agent-measure-client CLIENT agent_cpp measure/client_main.cc)

# stock grpc++ peer cells (full agent.proto — protobuf handles the oneof).
if(GRPCPP_AVAILABLE)
    grpcpp_codegen(agent agent)
    grpcpp_example(agent-grpcpp-server agent grpcpp/server_main.cc)
    grpcpp_example(agent-grpcpp-client agent grpcpp/client_main.cc)
    if(TARGET agent-gt-server)
        add_test(NAME agent-grpcuds-server-grpcpp-client
                 COMMAND bash ${PROJECT_SOURCE_DIR}/cmake/interop_pair.sh
                         $<TARGET_FILE:agent-gt-server> $<TARGET_FILE:agent-grpcpp-client>)
    endif()
    if(TARGET agent-tg-client)
        add_test(NAME agent-grpcpp-server-grpcuds-client
                 COMMAND bash ${PROJECT_SOURCE_DIR}/cmake/interop_pair.sh
                         $<TARGET_FILE:agent-grpcpp-server> $<TARGET_FILE:agent-tg-client>)
    endif()
endif()
