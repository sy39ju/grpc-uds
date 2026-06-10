// SPDX-License-Identifier: MIT OR Apache-2.0
//
// grpcuds::Service — base class for all services.
//
// The protoc plugin emits a `MyService::Service` subclass whose
// `BindToServer` registers the method trampolines + RPC-method virtual
// declarations. Users derive from that to implement their handlers,
// gRPC-C++ style.
//
// For hand-rolled services (tests, ad-hoc tooling),
// derive directly from `grpcuds::Service` and call
// `grpcuds_server_register_method` inside `BindToServer`.

#ifndef GRPCUDSPP_SERVICE_H_
#define GRPCUDSPP_SERVICE_H_

#include <grpcuds.h>

namespace grpcuds {

class Service {
 public:
    Service() = default;
    virtual ~Service() = default;

    // Non-copyable / non-movable: a service is referenced via raw
    // pointer from registered C trampolines.
    Service(const Service&) = delete;
    Service& operator=(const Service&) = delete;

    // Register every RPC method this service exposes onto `server` via
    // grpcuds_server_register_method. ServerBuilder::BuildAndStart calls
    // this for each registered service.
    virtual void BindToServer(grpcuds_server* server) = 0;
};

}  // namespace grpcuds

#endif  // GRPCUDSPP_SERVICE_H_
