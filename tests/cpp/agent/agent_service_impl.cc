// SPDX-License-Identifier: MIT OR Apache-2.0
#include "agent_service_impl.h"

#include <cmath>
#include <cstdio>
#include <cstring>

#include "mock_values.h"

grpcuds::Status AgentServiceImpl::ListModels(grpcuds::ServerContext*,
                                             const ::agent_ListModelsRequest*,
                                             ::agent_ModelList* response) {
    response->models_count = 2;
    std::snprintf(response->models[0].name, sizeof(response->models[0].name), "%s",
                  mock::kAgentModelA);
    response->models[0].context_len = 4096;
    std::snprintf(response->models[1].name, sizeof(response->models[1].name), "%s",
                  mock::kAgentModelB);
    response->models[1].context_len = 1024;
    return grpcuds::Status::Ok();
}

grpcuds::Status AgentServiceImpl::Generate(grpcuds::ServerContext*,
                                           const ::agent_GenerateRequest* request,
                                           grpcuds::ServerWriter<::agent_Token>* writer) {
    if (std::strcmp(request->model, mock::kAgentModelA) != 0 &&
        std::strcmp(request->model, mock::kAgentModelB) != 0) {
        return grpcuds::Status(grpcuds::NOT_FOUND, "unknown model");
    }
    int max = request->max_tokens == 0 ? 32 : static_cast<int>(request->max_tokens);
    auto toks = mock::agent_tokens(request->prompt, max);
    for (int i = 0; i < static_cast<int>(toks.size()); ++i) {
        ::agent_Token t = agent_Token_init_zero;
        t.index = static_cast<uint32_t>(i);
        std::snprintf(t.text, sizeof(t.text), "%s", toks[i].c_str());
        writer->Write(t);
    }
    writer->Finish(grpcuds::Status::Ok());
    return grpcuds::Status::Ok();
}

void AgentServiceImpl::Embed(grpcuds::ServerContext*, const ::agent_EmbedRequest* request,
                             grpcuds::UnaryResponder<::agent_Embedding> responder) {
    // Long-running-job shape: return now, finish from a worker thread. The
    // responder is a cheap thread-safe handle (mailbox-backed).
    std::string text(request->text);
    workers_.emplace_back([text, responder]() mutable {
        ::agent_Embedding response = agent_Embedding_init_zero;
        float buckets[8] = {0};
        for (char ch : text) {
            buckets[static_cast<unsigned char>(ch) % 8] += 1.0f;
        }
        float norm = 0;
        for (float v : buckets) norm += v * v;
        norm = std::sqrt(norm);
        if (norm < 1.0f) norm = 1.0f;
        response.values_count = 8;
        for (int i = 0; i < 8; ++i) response.values[i] = buckets[i] / norm;
        responder.Respond(response);
    });
}
