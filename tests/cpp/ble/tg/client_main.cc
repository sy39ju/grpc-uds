// SPDX-License-Identifier: MIT OR Apache-2.0
//
// BLE — grpcuds client binary. Connects to a Rust tonic server peer (the `tg`
// row) at argv[1], drives the RPCs, self-checks. Exit 0 on success, 1 on
// mismatch, 2 on usage error.
#include <grpcudspp/client.h>

#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>

#include "ble.grpc.pb.h"
#include "mock_values.h"

#define CHECK(c)                                                            \
    do {                                                                    \
        if (!(c)) {                                                         \
            std::fprintf(stderr, "CHECK failed: %s (%s:%d)\n", #c, __FILE__, \
                         __LINE__);                                         \
            std::exit(1);                                                   \
        }                                                                   \
    } while (0)

int main(int argc, char** argv) {
    if (argc < 2) {
        std::fprintf(stderr, "usage: %s <sock>\n", argv[0]);
        return 2;
    }
    grpcuds::Client client(argv[1]);
    CHECK(static_cast<bool>(client));
    auto stub = ble::BleService::NewStub(client);

    ::ble_InitRequest ireq = ble_InitRequest_init_zero;
    ::ble_InitReply irep = ble_InitReply_init_zero;
    grpcuds::Status s = stub->Init(ireq, &irep);
    CHECK(s.ok());
    CHECK(irep.ok);

    ::ble_ScanResultStreamRequest sreq = ble_ScanResultStreamRequest_init_zero;
    auto reader = stub->ScanResultStream(sreq);
    int n = 0;
    ::ble_ScanResult r = ble_ScanResult_init_zero;
    while (reader.Read(&r)) {
        CHECK(std::strcmp(r.mac, mock::kBleMac) == 0);
        CHECK(r.rssi == mock::ble_rssi(n));
        ++n;
        r = ble_ScanResult_init_zero;
    }
    CHECK(reader.status().error_code() == grpcuds::OK);
    CHECK(n == mock::kBleScanCount);

    ::ble_RemoveScanFilterRequest rreq = ble_RemoveScanFilterRequest_init_zero;
    rreq.filter_id = 99;
    ::ble_RemoveScanFilterReply rrep = ble_RemoveScanFilterReply_init_zero;
    grpcuds::Status es = stub->RemoveScanFilter(rreq, &rrep);
    CHECK(es.error_code() == grpcuds::NOT_FOUND);

    std::printf("ble-tg-client: OK\n");
    return 0;
}
