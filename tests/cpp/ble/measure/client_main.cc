// SPDX-License-Identifier: MIT OR Apache-2.0
//
// BLE grpcuds C++ client for footprint measurement: connect, do one round of
// calls, print READY, then hold the connection idle so an external sampler can
// read /proc/<pid>/smaps_rollup. Not part of the matrix.
#include <grpcudspp/client.h>

#include <unistd.h>

#include <cstdio>

#include "ble.grpc.pb.h"

int main(int argc, char** argv) {
    if (argc < 2) return 2;
    grpcuds::Client client(argv[1]);
    if (!client) return 1;
    auto stub = ble::BleService::NewStub(client);

    ::ble_InitRequest ireq = ble_InitRequest_init_zero;
    ::ble_InitReply irep = ble_InitReply_init_zero;
    stub->Init(ireq, &irep);
    ::ble_ScanResultStreamRequest sreq = ble_ScanResultStreamRequest_init_zero;
    auto reader = stub->ScanResultStream(sreq);
    ::ble_ScanResult r = ble_ScanResult_init_zero;
    while (reader.Read(&r)) r = ble_ScanResult_init_zero;

    std::printf("READY\n");
    std::fflush(stdout);
    sleep(5);
    return 0;
}
