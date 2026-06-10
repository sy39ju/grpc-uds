// SPDX-License-Identifier: MIT OR Apache-2.0
//
// BLE client on the GENERATED C stubs (--grpcudspp_opt=c) — pairs against
// the grpcuds C++ server (ble-gt-server) and, when grpc++ is installed,
// against the STOCK grpc++ server: the conformance proof that generated-C
// clients speak real gRPC. Mirrors ble-tg-client's checks; fixture values
// follow tests/cpp/common/mock_values.h (a C++ header, so they are pinned
// here as plain constants). Self-checking: exit 0 on success.

#include <stdio.h>
#include <string.h>

#include "ble.grpcuds.h"

#define CHECK(c)                                                          \
    do {                                                                  \
        if (!(c)) {                                                       \
            fprintf(stderr, "CHECK failed: %s (%s:%d)\n", #c, __FILE__,   \
                    __LINE__);                                            \
            return 1;                                                     \
        }                                                                 \
    } while (0)

// mock_values.h equivalents (kBleMac / ble_rssi / kBleScanCount).
static const char* kMac = "AA:BB:CC:DD:EE:FF";
#define SCAN_COUNT 3

int main(int argc, char** argv) {
    if (argc < 2) {
        fprintf(stderr, "usage: %s <sock>\n", argv[0]);
        return 2;
    }
    grpcuds_client* c = grpcuds_client_connect_wait(argv[1], 3000);
    CHECK(c != NULL);

    ble_InitRequest ireq = ble_InitRequest_init_zero;
    ble_InitReply irep = ble_InitReply_init_zero;
    CHECK(ble_BleService_Init(c, &ireq, &irep) == GRPCUDS_OK);
    CHECK(irep.ok);

    ble_ScanResultStreamRequest sreq = ble_ScanResultStreamRequest_init_zero;
    grpcuds_stream* st = ble_BleService_ScanResultStream_start(c, &sreq);
    CHECK(st != NULL);
    int n = 0;
    ble_ScanResult r;
    int rc;
    while ((rc = ble_BleService_ScanResultStream_next(st, &r)) == 1) {
        CHECK(strcmp(r.mac, kMac) == 0);
        CHECK(r.rssi == -40 - n);
        ++n;
    }
    CHECK(rc == 0);
    CHECK(grpcuds_stream_status(st) == GRPCUDS_OK);
    CHECK(n == SCAN_COUNT);
    grpcuds_stream_free(st);

    ble_RemoveScanFilterRequest rreq = ble_RemoveScanFilterRequest_init_zero;
    rreq.filter_id = 99;
    ble_RemoveScanFilterReply rrep = ble_RemoveScanFilterReply_init_zero;
    CHECK(ble_BleService_RemoveScanFilter(c, &rreq, &rrep) == GRPCUDS_NOT_FOUND);

    grpcuds_client_free(c);
    printf("ble-cgen-client: OK\n");
    return 0;
}
