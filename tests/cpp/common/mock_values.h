// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Deterministic fixtures the C++ examples produce/expect, mirroring the Rust
// `*_domain::expect` so a grpcuds C++ side and a Rust tonic peer agree on the
// wire.
#ifndef GRPCUDS_EXAMPLES_MOCK_VALUES_H_
#define GRPCUDS_EXAMPLES_MOCK_VALUES_H_

#include <cstdint>
#include <sstream>
#include <string>
#include <vector>

namespace mock {

// The echo "inference" both Agent servers run: token i = words[i % n] (or "…").
inline std::vector<std::string> agent_tokens(const std::string& prompt, int max) {
    std::vector<std::string> words;
    std::istringstream iss(prompt);
    for (std::string w; iss >> w;) words.push_back(w);
    std::vector<std::string> out;
    out.reserve(max);
    for (int i = 0; i < max; ++i) {
        out.push_back(words.empty() ? "\xe2\x80\xa6" : words[i % words.size()]);
    }
    return out;
}


// ---- BLE: three scan results ----
inline constexpr const char* kBleMac = "AA:BB:CC:DD:EE:FF";
inline constexpr int kBleScanCount = 3;
inline int ble_rssi(int i) { return -40 - i; }
inline constexpr uint8_t kBleAdv[3] = {0x02, 0x01, 0x06};

// ---- AI agent (Agent model runtime only — no Assistant/oneof in C++) ----
inline constexpr int kAgentModelCount = 2;
inline constexpr const char* kAgentModelA = "echo-1";
inline constexpr const char* kAgentModelB = "echo-1-mini";
inline constexpr int kAgentEmbedDims = 8;

// ---- X.509 (deterministic mock — no crypto) ----
inline constexpr const char* kX509MockKey = "MOCKKEY\n";

}  // namespace mock

#endif  // GRPCUDS_EXAMPLES_MOCK_VALUES_H_
