// SPDX-License-Identifier: MIT OR Apache-2.0
#include "x509_service_impl.h"

#include <cstdio>
#include <cstring>

namespace {
constexpr int64_t kMockNow = 1700000000;
}

grpcuds::Status X509ServiceImpl::GenerateSelfSigned(
    grpcuds::ServerContext*, const ::x509_GenerateSelfSignedRequest* request,
    ::x509_KeyPairPem* response) {
    if (request->common_name[0] == '\0') {
        return grpcuds::Status(grpcuds::INVALID_ARGUMENT, "common_name is required");
    }
    uint32_t days = request->validity_days == 0 ? 365 : request->validity_days;
    std::snprintf(response->cert_pem, sizeof(response->cert_pem), "MOCKCERT cn=%s days=%u\n",
                  request->common_name, days);
    std::snprintf(response->key_pem, sizeof(response->key_pem), "MOCKKEY\n");
    return grpcuds::Status::Ok();
}

grpcuds::Status X509ServiceImpl::Inspect(grpcuds::ServerContext*, const ::x509_PemCert* request,
                                         ::x509_CertInfo* response) {
    // Echo the CN encoded by GenerateSelfSigned ("...cn=<cn> ...").
    const char* cn = std::strstr(request->pem, "cn=");
    char name[64] = {0};
    if (cn) {
        cn += 3;
        std::snprintf(name, sizeof(name), "%s", cn);
        for (char* p = name; *p; ++p) {
            if (*p == ' ' || *p == '\n') {
                *p = '\0';
                break;
            }
        }
    }
    std::snprintf(response->subject, sizeof(response->subject), "CN=%s", name);
    std::snprintf(response->issuer, sizeof(response->issuer), "CN=%s", name);
    return grpcuds::Status::Ok();
}

grpcuds::Status X509ServiceImpl::Fingerprint(grpcuds::ServerContext*,
                                             const ::x509_FingerprintRequest*,
                                             ::x509_FingerprintReply* response) {
    for (int i = 0; i < 64; ++i) response->hex[i] = '0';
    response->hex[64] = '\0';
    return grpcuds::Status::Ok();
}

grpcuds::Status X509ServiceImpl::CheckExpiry(grpcuds::ServerContext*,
                                             const ::x509_CheckExpiryRequest* request,
                                             grpcuds::ServerWriter<::x509_ExpiryStatus>* writer) {
    int64_t now = request->now_unix != 0 ? request->now_unix : kMockNow;
    for (pb_size_t i = 0; i < request->pems_count; ++i) {
        ::x509_ExpiryStatus st = x509_ExpiryStatus_init_zero;
        st.index = static_cast<uint32_t>(i);
        std::snprintf(st.subject, sizeof(st.subject), "cert-%u", static_cast<unsigned>(i));
        st.not_after_unix = now + 1000;
        st.seconds_remaining = 1000;
        st.expired = false;
        writer->Write(st);
    }
    writer->Finish(grpcuds::Status::Ok());
    return grpcuds::Status::Ok();
}
