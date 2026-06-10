// SPDX-License-Identifier: MIT OR Apache-2.0
//
// BLE — grpcuds server + grpcuds client, one process. Self-checking ctest:
// exits 0 on success, 1 on mismatch.
#include <grpcudspp/client.h>
#include <grpcudspp/grpcudspp.h>

#include <unistd.h>

#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>

#include "ble.grpc.pb.h"
#include "ble_service_impl.h"
#include "mock_values.h"

#define CHECK(c)                                                            \
    do {                                                                    \
        if (!(c)) {                                                         \
            std::fprintf(stderr, "CHECK failed: %s (%s:%d)\n", #c, __FILE__, \
                         __LINE__);                                         \
            std::exit(1);                                                   \
        }                                                                   \
    } while (0)

int main() {
    std::string path = "/tmp/grpcuds-ble-gg-" + std::to_string(getpid()) + ".sock";
    ::unlink(path.c_str());

    grpcuds::ServerBuilder builder;
    builder.AddListeningPort("unix:" + path);
    BleServiceImpl svc;
    builder.RegisterService(&svc);
    auto server = builder.BuildAndStart();
    CHECK(server != nullptr);
    grpcuds::ServerThread server_thread(std::move(server));

    grpcuds::Client client(path);
    CHECK(static_cast<bool>(client));
    auto stub = ble::BleService::NewStub(client);

    // Init (unary).
    ::ble_InitRequest ireq = ble_InitRequest_init_zero;
    ::ble_InitReply irep = ble_InitReply_init_zero;
    grpcuds::Status s = stub->Init(ireq, &irep);
    CHECK(s.ok());
    CHECK(irep.ok);

    // ScanResultStream (server streaming).
    ::ble_ScanResultStreamRequest sreq = ble_ScanResultStreamRequest_init_zero;
    auto reader = stub->ScanResultStream(sreq);
    int n = 0;
    ::ble_ScanResult r = ble_ScanResult_init_zero;
    while (reader.Read(&r)) {
        CHECK(std::strcmp(r.mac, mock::kBleMac) == 0);
        CHECK(r.rssi == mock::ble_rssi(n));
        CHECK(r.adv_data.size == 3);
        ++n;
        r = ble_ScanResult_init_zero;
    }
    CHECK(reader.status().error_code() == grpcuds::OK);
    CHECK(n == mock::kBleScanCount);

    // RemoveScanFilter error path → NOT_FOUND.
    ::ble_RemoveScanFilterRequest rreq = ble_RemoveScanFilterRequest_init_zero;
    rreq.filter_id = 99;
    ::ble_RemoveScanFilterReply rrep = ble_RemoveScanFilterReply_init_zero;
    grpcuds::Status es = stub->RemoveScanFilter(rreq, &rrep);
    CHECK(es.error_code() == grpcuds::NOT_FOUND);

    std::printf("ble-gg: OK\n");
    return 0;
}
