// SPDX-License-Identifier: MIT OR Apache-2.0
//
// X.509 — grpcuds server binary (peer: rust tonic-client-x509).
#include <unistd.h>

#include <string>

#include "poll_loop.h"
#include "x509_service_impl.h"

int main(int argc, char** argv) {
    std::string path = argc > 1 ? argv[1] : "/tmp/grpcuds-x509-gt.sock";
    ::unlink(path.c_str());
    X509ServiceImpl svc;
    return grpcuds_ex::run_poll_loop(path, &svc, []() {});
}
