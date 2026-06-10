// SPDX-License-Identifier: MIT OR Apache-2.0
//
// AI agent — grpcuds server binary (peer: rust tonic-client-agent).
#include <unistd.h>

#include <string>

#include "agent_service_impl.h"
#include "poll_loop.h"

int main(int argc, char** argv) {
    std::string path = argc > 1 ? argv[1] : "/tmp/grpcuds-agent-gt.sock";
    ::unlink(path.c_str());
    AgentServiceImpl svc;
    return grpcuds_ex::run_poll_loop(path, &svc, []() {});
}
