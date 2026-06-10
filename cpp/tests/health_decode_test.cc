// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Unit test for the grpc.health.v1 wire decoders in <grpcudspp/health.h>.
// These parse PEER-CONTROLLED bytes (a Health/Check or Health/Watch request
// body), so malformed input must be rejected, never read out of bounds or
// shift past 64 bits (UB). Pure header test — no server, no socket.

#include <grpcudspp/health.h>

#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <string>
#include <vector>

#define CHECK(cond)                                                      \
    do {                                                                 \
        if (!(cond)) {                                                   \
            std::fprintf(stderr, "CHECK failed: %s (%s:%d)\n", #cond,    \
                         __FILE__, __LINE__);                            \
            std::exit(1);                                                \
        }                                                                \
    } while (0)

using grpcuds::health::DecodeCheckRequest;
using grpcuds::health::DecodeResponse;
using grpcuds::health::EncodeCheckRequest;
using grpcuds::health::EncodeResponse;
using grpcuds::health::SERVING;

int main() {
    // ---- well-formed round trips ------------------------------------------
    {
        std::vector<uint8_t> enc = EncodeCheckRequest("ble.BleScanner");
        std::string svc;
        CHECK(DecodeCheckRequest(enc.data(), enc.size(), &svc));
        CHECK(svc == "ble.BleScanner");

        // Empty service (proto3 default) encodes to zero bytes, decodes empty.
        std::vector<uint8_t> e0 = EncodeCheckRequest("");
        CHECK(e0.empty());
        std::string s0 = "stale";
        CHECK(DecodeCheckRequest(e0.data(), e0.size(), &s0));
        CHECK(s0.empty());

        std::vector<uint8_t> rs = EncodeResponse(SERVING);
        int status = -1;
        CHECK(DecodeResponse(rs.data(), rs.size(), &status));
        CHECK(status == SERVING);
    }

    // ---- malformed: wrong field tag ---------------------------------------
    {
        const uint8_t bad_tag[] = {0x12, 0x01, 'x'};  // field 2, not 1
        std::string svc;
        CHECK(!DecodeCheckRequest(bad_tag, sizeof(bad_tag), &svc));
        int st;
        CHECK(!DecodeResponse(bad_tag, sizeof(bad_tag), &st));
    }

    // ---- malformed: length runs past the buffer ---------------------------
    {
        const uint8_t overlong[] = {0x0A, 0x7F, 'a', 'b'};  // claims 127 bytes
        std::string svc;
        CHECK(!DecodeCheckRequest(overlong, sizeof(overlong), &svc));
    }

    // ---- malformed: truncated varint (continuation bit, no terminator) ----
    {
        const uint8_t truncated[] = {0x0A, 0x80};  // 0x80 = "more bytes follow"
        std::string svc;
        CHECK(!DecodeCheckRequest(truncated, sizeof(truncated), &svc));
    }

    // ---- malformed: oversized varint (>64-bit shift would be UB) ----------
    // 11 continuation bytes drive the shift to 70+; the decoder must reject
    // before `<< shift` with shift >= 64.
    {
        std::vector<uint8_t> huge;
        huge.push_back(0x0A);
        for (int i = 0; i < 11; ++i) huge.push_back(0x80);
        huge.push_back(0x01);
        std::string svc;
        CHECK(!DecodeCheckRequest(huge.data(), huge.size(), &svc));

        std::vector<uint8_t> huge_r;
        huge_r.push_back(0x08);
        for (int i = 0; i < 11; ++i) huge_r.push_back(0x80);
        huge_r.push_back(0x01);
        int st;
        CHECK(!DecodeResponse(huge_r.data(), huge_r.size(), &st));
    }

    std::printf("health_decode_test: OK\n");
    return 0;
}
