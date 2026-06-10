// SPDX-License-Identifier: MIT OR Apache-2.0
//
// grpcuds::Status — gRPC C++-style status wrapper.
//
// Migration note: the API mirrors `grpc::Status` — construct from a
// StatusCode (optionally with a message); `ok()` / `error_code()` /
// `error_message()` available. The message, when non-empty, is shipped as a
// `grpc-message` trailer (percent-encoded by the runtime) alongside the
// numeric `grpc-status`.

#ifndef GRPCUDSPP_STATUS_H_
#define GRPCUDSPP_STATUS_H_

#include <string>
#include <utility>

#include <grpcuds.h>

namespace grpcuds {

// Mirror of `grpc::StatusCode` (and grpcuds_status from the C header).
enum StatusCode : int {
    OK                  = GRPCUDS_OK,
    CANCELLED           = GRPCUDS_CANCELLED,
    UNKNOWN             = GRPCUDS_UNKNOWN,
    INVALID_ARGUMENT    = GRPCUDS_INVALID_ARGUMENT,
    DEADLINE_EXCEEDED   = GRPCUDS_DEADLINE_EXCEEDED,
    NOT_FOUND           = GRPCUDS_NOT_FOUND,
    ALREADY_EXISTS      = GRPCUDS_ALREADY_EXISTS,
    PERMISSION_DENIED   = GRPCUDS_PERMISSION_DENIED,
    RESOURCE_EXHAUSTED  = GRPCUDS_RESOURCE_EXHAUSTED,
    FAILED_PRECONDITION = GRPCUDS_FAILED_PRECONDITION,
    ABORTED             = GRPCUDS_ABORTED,
    OUT_OF_RANGE        = GRPCUDS_OUT_OF_RANGE,
    UNIMPLEMENTED       = GRPCUDS_UNIMPLEMENTED,
    INTERNAL            = GRPCUDS_INTERNAL,
    UNAVAILABLE         = GRPCUDS_UNAVAILABLE,
    DATA_LOSS           = GRPCUDS_DATA_LOSS,
    UNAUTHENTICATED     = GRPCUDS_UNAUTHENTICATED,
};

class Status {
 public:
    Status() : code_(OK) {}
    explicit Status(StatusCode code) : code_(code) {}
    Status(StatusCode code, std::string message)
        : code_(code), message_(std::move(message)) {}

    static Status Ok() { return Status(OK); }

    bool ok() const { return code_ == OK; }
    StatusCode error_code() const { return code_; }
    const std::string& error_message() const { return message_; }

 private:
    StatusCode code_;
    std::string message_;
};

}  // namespace grpcuds

#endif  // GRPCUDSPP_STATUS_H_
