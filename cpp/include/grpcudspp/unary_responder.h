// SPDX-License-Identifier: MIT OR Apache-2.0
//
// grpcuds::UnaryResponder<T> — deferred completion for unary RPCs, the
// grpcuds analogue of grpc++'s callback-API `ServerUnaryReactor`.
//
// The generated unary trampoline constructs one and invokes the deferred
// virtual overload of your method. Its DEFAULT implementation calls the
// synchronous handler and completes immediately, so existing services are
// unchanged. To keep the call open past the handler's return (long-running
// work), override the deferred overload instead and complete from any
// thread later:
//
//     void Embed(grpcuds::ServerContext*, const ::agent_EmbedRequest* req,
//                grpcuds::UnaryResponder<::agent_Embedding> responder) override {
//         StartJob(*req, [responder]() mutable {        // any thread, later
//             ::agent_Embedding reply = agent_Embedding_init_zero;
//             // ... fill reply ...
//             responder.Respond(reply);                 // or responder.Fail(s)
//         });
//     }
//
// Respond() encodes the single response via nanopb, writes it, and finishes
// OK; Fail() finishes with a non-OK status and no message. Both are
// thread-safe (the C ABI routes off-I/O-thread writes through the outbound
// mailbox; see docs/THREADING.md) and are
// single-use: the first completion wins, later calls fail. Copies share the
// underlying call — copying is cheap and intended (capture by value).
//
// A handler that neither completes nor stores the responder leaves the call
// open forever — same contract as a streaming handler that never Finish()es.

#ifndef GRPCUDSPP_UNARY_RESPONDER_H_
#define GRPCUDSPP_UNARY_RESPONDER_H_

#include "grpcudspp/server_writer.h"

namespace grpcuds {

template <typename T>
class UnaryResponder {
 public:
    UnaryResponder(void* call, int32_t call_id) : raw_(call, call_id) {}

    explicit UnaryResponder(const ServerContext& ctx) : raw_(ctx) {}

    // Plugin-generated specializations encode `response` (via nanopb),
    // write it, and finish OK. The base template is intentionally
    // undefined — using it without a generated specialization is a link
    // error, the same protection ServerWriter<T>::Write has.
    bool Respond(const T& response);

    // Finish with a non-OK status and no response message.
    bool Fail(const Status& s) { return raw_.Finish(s); }

 private:
    RawWriter raw_;
};

}  // namespace grpcuds

#endif  // GRPCUDSPP_UNARY_RESPONDER_H_
