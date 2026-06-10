// SPDX-License-Identifier: MIT OR Apache-2.0
//
// BLE — STOCK grpc++ server (protobuf-full) over UDS. The same-language stock
// peer for the C++ matrix: a grpcuds C++ client dials this exactly as it dials
// a grpcuds server. Emits the deterministic mock values so the assertions agree.
#include <grpcpp/grpcpp.h>

#include <csignal>
#include <cstdio>
#include <string>

#include "ble.grpc.pb.h"
#include "mock_values.h"

using grpc::Server;
using grpc::ServerBuilder;
using grpc::ServerContext;
using grpc::ServerWriter;
using grpc::Status;

class BleGrpcppService final : public ble::BleService::Service {
    Status Init(ServerContext*, const ble::InitRequest*, ble::InitReply* r) override {
        r->set_ok(true);
        return Status::OK;
    }
    Status StartLeScan(ServerContext*, const ble::StartLeScanRequest*,
                       ble::StartLeScanReply* r) override {
        r->set_ok(true);
        return Status::OK;
    }
    Status StopLeScan(ServerContext*, const ble::StopLeScanRequest*,
                      ble::StopLeScanReply* r) override {
        r->set_ok(true);
        return Status::OK;
    }
    Status ScanResultStream(ServerContext*, const ble::ScanResultStreamRequest*,
                            ServerWriter<ble::ScanResult>* w) override {
        for (int i = 0; i < mock::kBleScanCount; ++i) {
            ble::ScanResult r;
            r.set_mac(mock::kBleMac);
            r.set_rssi(mock::ble_rssi(i));
            r.set_adv_data(std::string(reinterpret_cast<const char*>(mock::kBleAdv), 3));
            w->Write(r);
        }
        return Status::OK;
    }
    Status AdapterStateChangeStream(ServerContext*, const ble::AdapterStateChangeStreamRequest*,
                                    ServerWriter<ble::AdapterStateChange>*) override {
        return Status::OK;
    }
    Status AddScanFilter(ServerContext*, const ble::AddScanFilterRequest*,
                         ble::AddScanFilterReply* r) override {
        r->set_filter_id(7);
        return Status::OK;
    }
    Status RemoveScanFilter(ServerContext*, const ble::RemoveScanFilterRequest* req,
                            ble::RemoveScanFilterReply* r) override {
        if (req->filter_id() != 7) {
            return Status(grpc::StatusCode::NOT_FOUND, "unknown filter id");
        }
        r->set_ok(true);
        return Status::OK;
    }
};

int main(int argc, char** argv) {
    std::string path = argc > 1 ? argv[1] : "/tmp/ble-grpcpp.sock";
    ::unlink(path.c_str());
    BleGrpcppService svc;
    ServerBuilder builder;
    builder.AddListeningPort("unix:" + path, grpc::InsecureServerCredentials());
    builder.RegisterService(&svc);
    std::unique_ptr<Server> server = builder.BuildAndStart();
    if (!server) {
        std::fprintf(stderr, "grpc++ BuildAndStart failed\n");
        return 1;
    }
    std::printf("READY\n");
    std::fflush(stdout);
    server->Wait();
    return 0;
}
