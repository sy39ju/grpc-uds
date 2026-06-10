// SPDX-License-Identifier: MIT OR Apache-2.0
//
// AI agent — STOCK grpc++ server (protobuf-full) over UDS, the `Agent` model
// runtime (no Assistant loop). protobuf-full handles agent.proto's oneof, so
// this uses the full proto directly. Emits the deterministic mock values.
#include <grpcpp/grpcpp.h>

#include <cmath>
#include <cstdio>
#include <string>

#include "agent.grpc.pb.h"
#include "mock_values.h"

using grpc::Server;
using grpc::ServerBuilder;
using grpc::ServerContext;
using grpc::ServerWriter;
using grpc::Status;

class AgentGrpcppService final : public agent::Agent::Service {
    Status ListModels(ServerContext*, const agent::ListModelsRequest*,
                      agent::ModelList* out) override {
        auto* a = out->add_models();
        a->set_name(mock::kAgentModelA);
        a->set_context_len(4096);
        auto* b = out->add_models();
        b->set_name(mock::kAgentModelB);
        b->set_context_len(1024);
        return Status::OK;
    }
    Status Generate(ServerContext*, const agent::GenerateRequest* req,
                    ServerWriter<agent::Token>* w) override {
        if (req->model() != mock::kAgentModelA && req->model() != mock::kAgentModelB) {
            return Status(grpc::StatusCode::NOT_FOUND, "unknown model");
        }
        int max = req->max_tokens() == 0 ? 32 : static_cast<int>(req->max_tokens());
        auto toks = mock::agent_tokens(req->prompt(), max);
        for (int i = 0; i < static_cast<int>(toks.size()); ++i) {
            agent::Token t;
            t.set_index(static_cast<uint32_t>(i));
            t.set_text(toks[i]);
            w->Write(t);
        }
        return Status::OK;
    }
    Status Embed(ServerContext*, const agent::EmbedRequest* req,
                 agent::Embedding* out) override {
        float buckets[8] = {0};
        for (char c : req->text()) buckets[static_cast<unsigned char>(c) % 8] += 1.0f;
        float norm = 0;
        for (float v : buckets) norm += v * v;
        norm = std::sqrt(norm);
        if (norm < 1.0f) norm = 1.0f;
        for (float v : buckets) out->add_values(v / norm);
        return Status::OK;
    }
};

int main(int argc, char** argv) {
    std::string path = argc > 1 ? argv[1] : "/tmp/agent-grpcpp.sock";
    ::unlink(path.c_str());
    AgentGrpcppService svc;
    ServerBuilder builder;
    builder.AddListeningPort("unix:" + path, grpc::InsecureServerCredentials());
    builder.RegisterService(&svc);
    std::unique_ptr<Server> server = builder.BuildAndStart();
    if (!server) return 1;
    std::printf("READY\n");
    std::fflush(stdout);
    server->Wait();
    return 0;
}
