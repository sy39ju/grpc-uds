// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Deterministic mock X.509 service (no crypto), mirroring the Rust
// `x509_domain::TonicX509` so the cross-language assertions agree.
#ifndef GRPCUDS_EXAMPLES_X509_SERVICE_IMPL_H_
#define GRPCUDS_EXAMPLES_X509_SERVICE_IMPL_H_

#include "x509.grpc.pb.h"

class X509ServiceImpl final : public x509::X509::Service {
 public:
    grpcuds::Status GenerateSelfSigned(grpcuds::ServerContext*,
                                       const ::x509_GenerateSelfSignedRequest* request,
                                       ::x509_KeyPairPem* response) override;
    grpcuds::Status Inspect(grpcuds::ServerContext*, const ::x509_PemCert* request,
                            ::x509_CertInfo* response) override;
    grpcuds::Status Fingerprint(grpcuds::ServerContext*, const ::x509_FingerprintRequest*,
                                ::x509_FingerprintReply* response) override;
    grpcuds::Status CheckExpiry(grpcuds::ServerContext*, const ::x509_CheckExpiryRequest* request,
                                grpcuds::ServerWriter<::x509_ExpiryStatus>* writer) override;
};

#endif  // GRPCUDS_EXAMPLES_X509_SERVICE_IMPL_H_
