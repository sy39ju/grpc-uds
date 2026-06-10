// SPDX-License-Identifier: MIT OR Apache-2.0
//
// grpcuds::ServerContext — per-call context handed to a handler.
//
// Mirrors `grpc::ServerContext` shape. The opaque (call, call_id) pair
// from the C ABI is what the runtime uses to identify a call; this class
// keeps them together so a handler can hand the context off (e.g. to a
// BLE event closure) and later write from there.

#ifndef GRPCUDSPP_SERVER_CONTEXT_H_
#define GRPCUDSPP_SERVER_CONTEXT_H_

#include <stdint.h>

#include <grpcuds.h>

namespace grpcuds {

class ServerContext {
 public:
    ServerContext(void* call, int32_t call_id)
        : call_(call), call_id_(call_id) {}

    // Opaque handle pair. Pass these to grpcuds::ServerWriter / RawWriter
    // or directly to the C ABI grpcuds_call_* functions.
    void* call() const { return call_; }
    int32_t call_id() const { return call_id_; }

    // Remaining milliseconds of this call's `grpc-timeout` budget — stock
    // gRPC's "context deadline". >= 0 when the client sent a deadline
    // (0 = already due), -1 when it sent none. Use it to skip work that
    // cannot finish in time.
    int64_t TimeRemainingMs() const {
        return grpcuds_call_time_remaining_ms(call_, call_id_);
    }

    // Install a cancel-cleanup callback for this call. The callback fires
    // when the peer cancels mid-stream (RST_STREAM) — typical use is to
    // tear down a backing producer (BLE scan, GATT subscription).
    //
    // Lifetime contract for `user_data`: the pointer MUST stay valid
    // until either the callback fires OR the call closes gracefully (in
    // which case the hook is forgotten and your graceful-close path must
    // free it). Heap-allocate the state and free from the callback.
    // See grpcuds_call_set_cancel_hook in grpcuds.h for a full example.
    bool SetCancelHook(void (*callback)(void* user_data), void* user_data) {
        return grpcuds_call_set_cancel_hook(call_, call_id_, callback,
                                            user_data) == 0;
    }

 private:
    void* call_;
    int32_t call_id_;
};

}  // namespace grpcuds

#endif  // GRPCUDSPP_SERVER_CONTEXT_H_
