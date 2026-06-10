// SPDX-License-Identifier: MIT OR Apache-2.0
//
// X.509 — grpcuds server + grpcuds client, one self-checking process.
#include <grpcudspp/grpcudspp.h>

#include <unistd.h>

#include <cstdio>
#include <string>

#include "x509_client_check.h"
#include "x509_service_impl.h"

int main() {
    std::string path = "/tmp/grpcuds-x509-gg-" + std::to_string(getpid()) + ".sock";
    ::unlink(path.c_str());

    grpcuds::ServerBuilder builder;
    builder.AddListeningPort("unix:" + path);
    X509ServiceImpl svc;
    builder.RegisterService(&svc);
    auto server = builder.BuildAndStart();
    if (!server) return 1;
    grpcuds::ServerThread server_thread(std::move(server));

    grpcuds::Client client(path);
    if (!client) return 1;
    int rc = x509_client_check(client);
    if (rc == 0) std::printf("x509-gg: OK\n");
    return rc;
}
