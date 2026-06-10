// SPDX-License-Identifier: MIT OR Apache-2.0
//
// AI agent — STOCK grpc++ client (protobuf-full) over UDS, self-checking
// against a grpcuds (or grpc++) Agent server. Exit 0/1/2; "hold" stays idle.
#include <grpcpp/grpcpp.h>

#include <unistd.h>

#include <cstdio>
#include <memory>
#include <string>

#include "agent.grpc.pb.h"
#include "mock_values.h"

int main(int argc, char** argv) {
    if (argc < 2) {
        std::fprintf(stderr, "usage: %s <sock> [hold]\n", argv[0]);
        return 2;
    }
    bool hold = argc > 2 && std::string(argv[2]) == "hold";
    auto channel = grpc::CreateChannel("unix:" + std::string(argv[1]),
                                       grpc::InsecureChannelCredentials());
    auto stub = agent::Agent::NewStub(channel);

    {
        grpc::ClientContext ctx;
        agent::ListModelsRequest req;
        agent::ModelList rep;
        if (!stub->ListModels(&ctx, req, &rep).ok() ||
            rep.models_size() != mock::kAgentModelCount) {
            return 1;
        }
    }
    {
        grpc::ClientContext ctx;
        agent::GenerateRequest req;
        req.set_model(mock::kAgentModelA);
        req.set_prompt("the quick brown fox");
        req.set_max_tokens(6);
        auto reader = stub->Generate(&ctx, req);
        auto expected = mock::agent_tokens("the quick brown fox", 6);
        agent::Token t;
        int n = 0;
        while (reader->Read(&t)) {
            if (n >= static_cast<int>(expected.size()) || t.text() != expected[n]) return 1;
            ++n;
        }
        if (!reader->Finish().ok() || n != static_cast<int>(expected.size())) return 1;
    }
    {
        grpc::ClientContext ctx;
        agent::EmbedRequest req;
        req.set_text("hello world");
        agent::Embedding rep;
        if (!stub->Embed(&ctx, req, &rep).ok() ||
            rep.values_size() != mock::kAgentEmbedDims) {
            return 1;
        }
    }

    std::printf("READY\n");
    std::fflush(stdout);
    if (hold) sleep(5);
    return 0;
}
