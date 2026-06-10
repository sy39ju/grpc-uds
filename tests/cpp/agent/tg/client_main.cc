// SPDX-License-Identifier: MIT OR Apache-2.0
//
// AI agent — grpcuds client binary, self-checking (peer: rust tonic server).
#include <grpcudspp/client.h>

#include <cstdio>
#include <string>

#include "agent_client_check.h"

int main(int argc, char** argv) {
    if (argc < 2) {
        std::fprintf(stderr, "usage: %s <sock>\n", argv[0]);
        return 2;
    }
    grpcuds::Client client(argv[1]);
    if (!client) return 1;
    int rc = agent_client_check(client);
    if (rc == 0) std::printf("agent-tg-client: OK\n");
    return rc;
}
