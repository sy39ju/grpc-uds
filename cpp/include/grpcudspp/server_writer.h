// SPDX-License-Identifier: MIT OR Apache-2.0
//
// grpcuds::ServerWriter<T> + grpcuds::RawWriter.
//
// `ServerWriter<T>` mirrors `grpc::ServerWriter<T>` — the templated
// `Write(const T&)` is *declared* here; the per-T definition is emitted
// by the protoc plugin, which calls nanopb to encode T into bytes
// before forwarding to grpcuds_call_write.
//
// `RawWriter` is the bytes-in / bytes-out variant for hand-rolled
// handlers (and the echo test).

#ifndef GRPCUDSPP_SERVER_WRITER_H_
#define GRPCUDSPP_SERVER_WRITER_H_

#include <stddef.h>
#include <stdint.h>

#include <grpcuds.h>

#include "grpcudspp/server_context.h"
#include "grpcudspp/status.h"

namespace grpcuds {

/// Backpressure policy for the per-call outbound queue. Matches the C ABI
/// `grpcuds_backpressure_policy` enum value-for-value.
enum class OverflowPolicy : int {
    Reject     = GRPCUDS_BACKPRESSURE_REJECT,
    DropOldest = GRPCUDS_BACKPRESSURE_DROP_OLDEST,
};

/// Backpressure configuration. Use the static factories so the
/// capacity-zero-with-a-policy misuse isn't representable.
///
/// Sample:
///
///     writer->SetBackpressure(grpcuds::Backpressure::Unbounded());
///     writer->SetBackpressure(
///         grpcuds::Backpressure::Bounded(4, grpcuds::OverflowPolicy::DropOldest));
class Backpressure {
 public:
    static Backpressure Unbounded() {
        return Backpressure(true, 0, OverflowPolicy::Reject);
    }
    static Backpressure Bounded(size_t capacity, OverflowPolicy policy) {
        return Backpressure(false, capacity, policy);
    }

    bool is_unbounded() const { return unbounded_; }
    size_t capacity() const { return capacity_; }
    OverflowPolicy policy() const { return policy_; }

 private:
    Backpressure(bool u, size_t c, OverflowPolicy p)
        : unbounded_(u), capacity_(c), policy_(p) {}
    bool unbounded_;
    size_t capacity_;
    OverflowPolicy policy_;
};

class RawWriter {
 public:
    RawWriter(void* call, int32_t call_id)
        : call_(call), call_id_(call_id) {}

    explicit RawWriter(const ServerContext& ctx)
        : RawWriter(ctx.call(), ctx.call_id()) {}

    // Enqueue one gRPC message payload. The 5-byte length prefix is added
    // by the runtime — do not include it in `data`.
    //
    // Thread-safe: grpcuds_call_write routes off-I/O-thread writes through the
    // outbound mailbox itself (on the I/O thread it touches the core directly,
    // honoring backpressure; off it, it copies + pokes the wakeup fd, and the
    // core call happens later via Server::DrainOutbound). See docs/THREADING.md.
    bool Write(const uint8_t* data, size_t len) {
        return grpcuds_call_write(call_, call_id_, data, len) == 0;
    }

    // Ship the trailing HEADERS and close the server side of the stream.
    // Subsequent Write/Finish calls fail. Thread-safe like Write().
    bool Finish(const Status& s) {
        const std::string& msg = s.error_message();
        return grpcuds_call_finish_msg(
                   call_, call_id_, static_cast<int>(s.error_code()),
                   reinterpret_cast<const uint8_t*>(msg.data()),
                   msg.size()) == 0;
    }

    // Configure outbound queue backpressure. Returns true on success.
    //
    // NOT thread-safe: unlike Write/Finish there is no mailbox path here —
    // this always calls the core directly. Call it only on the I/O thread,
    // typically inside the handler before handing the stream to a producer
    // thread. Calling it off the I/O thread races the session.
    bool SetBackpressure(Backpressure bp) {
        if (bp.is_unbounded()) {
            return grpcuds_call_set_backpressure_unbounded(call_, call_id_) == 0;
        }
        return grpcuds_call_set_backpressure_bounded(
                   call_, call_id_, bp.capacity(),
                   static_cast<int>(bp.policy())) == 0;
    }

 private:
    void* call_;
    int32_t call_id_;
};

template <typename T>
class ServerWriter {
 public:
    ServerWriter(void* call, int32_t call_id)
        : raw_(call, call_id) {}

    explicit ServerWriter(const ServerContext& ctx) : raw_(ctx) {}

    // Plugin-generated specializations encode `message` (via nanopb) and
    // forward to RawWriter::Write. The base template is intentionally
    // undefined — using it without a generated specialization is a link
    // error, which is the same protection gRPC's plugin gives.
    bool Write(const T& message);

    bool Finish(const Status& s) { return raw_.Finish(s); }

    // See RawWriter::SetBackpressure.
    bool SetBackpressure(Backpressure bp) { return raw_.SetBackpressure(bp); }

 private:
    RawWriter raw_;
};

}  // namespace grpcuds

#endif  // GRPCUDSPP_SERVER_WRITER_H_
