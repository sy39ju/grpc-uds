// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Shared self-check the agent gg/tg clients run against a server (grpcuds C++
// in gg, Rust tonic in tg). Returns 0 on success, 1 on mismatch.
#ifndef GRPCUDS_EXAMPLES_AGENT_CLIENT_CHECK_H_
#define GRPCUDS_EXAMPLES_AGENT_CLIENT_CHECK_H_

#include <grpcudspp/client.h>

#include <cstdio>
#include <cstring>
#include <string>

#include "agent_cpp.grpc.pb.h"
#include "mock_values.h"

inline int agent_client_check(grpcuds::Client& client) {
    auto stub = agent::Agent::NewStub(client);
    // ListModels (unary).
    ::agent_ListModelsRequest lreq = agent_ListModelsRequest_init_zero;
    ::agent_ModelList models = agent_ModelList_init_zero;
    if (!stub->ListModels(lreq, &models).ok() ||
        models.models_count != mock::kAgentModelCount) {
        std::fprintf(stderr, "ListModels failed (count=%u)\n", models.models_count);
        return 1;
    }

    // Generate (server streaming) — echo the prompt, capped.
    ::agent_GenerateRequest greq = agent_GenerateRequest_init_zero;
    std::snprintf(greq.model, sizeof(greq.model), "%s", mock::kAgentModelA);
    std::snprintf(greq.prompt, sizeof(greq.prompt), "the quick brown fox");
    greq.max_tokens = 6;
    auto reader = stub->Generate(greq);
    auto expected = mock::agent_tokens("the quick brown fox", 6);
    int n = 0;
    ::agent_Token t = agent_Token_init_zero;
    while (reader.Read(&t)) {
        if (n >= static_cast<int>(expected.size()) || expected[n] != t.text) {
            std::fprintf(stderr, "token %d mismatch: '%s'\n", n, t.text);
            return 1;
        }
        ++n;
        t = agent_Token_init_zero;
    }
    if (reader.status().error_code() != grpcuds::OK ||
        n != static_cast<int>(expected.size())) {
        std::fprintf(stderr, "generate count=%d\n", n);
        return 1;
    }

    // Embed (unary) — 8-dim.
    ::agent_EmbedRequest ereq = agent_EmbedRequest_init_zero;
    std::snprintf(ereq.text, sizeof(ereq.text), "hello world");
    ::agent_Embedding emb = agent_Embedding_init_zero;
    if (!stub->Embed(ereq, &emb).ok() ||
        emb.values_count != mock::kAgentEmbedDims) {
        std::fprintf(stderr, "embed dims=%u\n", emb.values_count);
        return 1;
    }
    return 0;
}

#endif  // GRPCUDS_EXAMPLES_AGENT_CLIENT_CHECK_H_
