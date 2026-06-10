// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Shared self-check the x509 gg/tg clients run against a deterministic mock
// server (grpcuds C++ in gg, Rust tonic in tg). Returns 0 / 1.
#ifndef GRPCUDS_EXAMPLES_X509_CLIENT_CHECK_H_
#define GRPCUDS_EXAMPLES_X509_CLIENT_CHECK_H_

#include <grpcudspp/client.h>

#include <cstdio>
#include <cstring>

#include "x509.grpc.pb.h"

inline int x509_client_check(grpcuds::Client& client) {
    auto stub = x509::X509::NewStub(client);
    // GenerateSelfSigned (unary) → canned MOCKCERT.
    ::x509_GenerateSelfSignedRequest greq = x509_GenerateSelfSignedRequest_init_zero;
    std::snprintf(greq.common_name, sizeof(greq.common_name), "mock.test");
    greq.validity_days = 10;
    ::x509_KeyPairPem kp = x509_KeyPairPem_init_zero;
    if (!stub->GenerateSelfSigned(greq, &kp).ok() ||
        std::strstr(kp.cert_pem, "cn=mock.test") == nullptr) {
        std::fprintf(stderr, "generate mismatch: '%s'\n", kp.cert_pem);
        return 1;
    }

    // CheckExpiry (server streaming) → one status per input pem.
    ::x509_CheckExpiryRequest creq = x509_CheckExpiryRequest_init_zero;
    creq.pems_count = 2;
    std::snprintf(creq.pems[0], sizeof(creq.pems[0]), "a");
    std::snprintf(creq.pems[1], sizeof(creq.pems[1]), "b");
    creq.now_unix = 1000000;
    auto reader = stub->CheckExpiry(creq);
    int n = 0;
    ::x509_ExpiryStatus st = x509_ExpiryStatus_init_zero;
    while (reader.Read(&st)) {
        char want[16];
        std::snprintf(want, sizeof(want), "cert-%d", n);
        if (std::strcmp(st.subject, want) != 0 || st.seconds_remaining != 1000 || st.expired) {
            std::fprintf(stderr, "expiry %d mismatch: subj='%s' rem=%lld\n", n, st.subject,
                         static_cast<long long>(st.seconds_remaining));
            return 1;
        }
        ++n;
        st = x509_ExpiryStatus_init_zero;
    }
    if (reader.status().error_code() != grpcuds::OK || n != 2) {
        std::fprintf(stderr, "expiry count=%d\n", n);
        return 1;
    }
    return 0;
}

#endif  // GRPCUDS_EXAMPLES_X509_CLIENT_CHECK_H_
