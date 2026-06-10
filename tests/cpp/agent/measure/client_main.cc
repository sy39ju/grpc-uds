// SPDX-License-Identifier: MIT OR Apache-2.0
//
// AI-agent grpcuds C++ client for footprint measurement (see ble/measure).
#include <grpcudspp/client.h>

#include <unistd.h>

#include <cstdio>

#include "agent_client_check.h"

int main(int argc, char** argv) {
    if (argc < 2) return 2;
    grpcuds::Client client(argv[1]);
    if (!client) return 1;
    agent_client_check(client);
    std::printf("READY\n");
    std::fflush(stdout);
    sleep(5);
    return 0;
}
