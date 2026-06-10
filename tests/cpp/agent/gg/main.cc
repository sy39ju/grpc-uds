// SPDX-License-Identifier: MIT OR Apache-2.0
//
// AI agent — grpcuds server + grpcuds client, one self-checking process.
#include <grpcudspp/grpcudspp.h>

#include <unistd.h>

#include <cstdio>
#include <string>

#include "agent_client_check.h"
#include "agent_service_impl.h"

int main() {
    std::string path = "/tmp/grpcuds-agent-gg-" + std::to_string(getpid()) + ".sock";
    ::unlink(path.c_str());

    grpcuds::ServerBuilder builder;
    builder.AddListeningPort("unix:" + path);
    AgentServiceImpl svc;
    builder.RegisterService(&svc);
    auto server = builder.BuildAndStart();
    if (!server) return 1;
    grpcuds::ServerThread server_thread(std::move(server));

    grpcuds::Client client(path);
    if (!client) return 1;
    int rc = agent_client_check(client);
    if (rc == 0) std::printf("agent-gg: OK\n");
    return rc;
}
