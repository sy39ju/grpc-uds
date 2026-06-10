// SPDX-License-Identifier: MIT OR Apache-2.0
//
// BLE — STOCK grpc++ client (protobuf-full) over UDS. Self-checking against a
// grpcuds server (or the grpc++ server). Exit 0 ok / 1 mismatch / 2 usage.
// With a second arg "hold" it stays connected (footprint sampling).
#include <grpcpp/grpcpp.h>

#include <unistd.h>

#include <cstdio>
#include <memory>
#include <string>

#include "ble.grpc.pb.h"
#include "mock_values.h"

int main(int argc, char** argv) {
    if (argc < 2) {
        std::fprintf(stderr, "usage: %s <sock> [hold]\n", argv[0]);
        return 2;
    }
    bool hold = argc > 2 && std::string(argv[2]) == "hold";
    auto channel = grpc::CreateChannel("unix:" + std::string(argv[1]),
                                       grpc::InsecureChannelCredentials());
    auto stub = ble::BleService::NewStub(channel);

    {
        grpc::ClientContext ctx;
        ble::InitRequest req;
        ble::InitReply rep;
        if (!stub->Init(&ctx, req, &rep).ok() || !rep.ok()) return 1;
    }
    {
        grpc::ClientContext ctx;
        ble::ScanResultStreamRequest req;
        auto reader = stub->ScanResultStream(&ctx, req);
        ble::ScanResult r;
        int n = 0;
        while (reader->Read(&r)) {
            if (r.mac() != mock::kBleMac || r.rssi() != mock::ble_rssi(n)) return 1;
            ++n;
        }
        if (!reader->Finish().ok() || n != mock::kBleScanCount) return 1;
    }
    {
        grpc::ClientContext ctx;
        ble::RemoveScanFilterRequest req;
        req.set_filter_id(99);
        ble::RemoveScanFilterReply rep;
        auto s = stub->RemoveScanFilter(&ctx, req, &rep);
        if (s.error_code() != grpc::StatusCode::NOT_FOUND) return 1;
    }

    std::printf("READY\n");
    std::fflush(stdout);
    if (hold) sleep(5);
    return 0;
}
