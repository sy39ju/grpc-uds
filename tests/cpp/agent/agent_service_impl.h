// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Deterministic AI-agent service (the `Agent` model runtime only — the C++
// examples avoid the `Assistant` loop and its AgentEvent oneof). Mirrors the
// Rust `agent_domain` echo runtime.
#ifndef GRPCUDS_EXAMPLES_AGENT_SERVICE_IMPL_H_
#define GRPCUDS_EXAMPLES_AGENT_SERVICE_IMPL_H_

#include <string>
#include <thread>
#include <vector>

#include "agent_cpp.grpc.pb.h"

class AgentServiceImpl final : public agent::Agent::Service {
 public:
    ~AgentServiceImpl() override {
        for (auto& t : workers_) t.join();
    }

    grpcuds::Status ListModels(grpcuds::ServerContext*, const ::agent_ListModelsRequest*,
                               ::agent_ModelList* response) override;
    grpcuds::Status Generate(grpcuds::ServerContext*, const ::agent_GenerateRequest* request,
                             grpcuds::ServerWriter<::agent_Token>* writer) override;
    // Deferred unary on purpose: the handler returns immediately and a
    // worker thread completes the call via the UnaryResponder — this is the
    // matrix's coverage of the deferred-unary codegen against every client
    // (grpcuds C++/Rust, tonic, stock grpc++).
    void Embed(grpcuds::ServerContext*, const ::agent_EmbedRequest* request,
               grpcuds::UnaryResponder<::agent_Embedding> responder) override;

 private:
    std::vector<std::thread> workers_;
};

#endif  // GRPCUDS_EXAMPLES_AGENT_SERVICE_IMPL_H_
