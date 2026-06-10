// SPDX-License-Identifier: MIT OR Apache-2.0
//
// BLE scanner client — the generated typed stub (`ble::BleScanner::NewStub`),
// the same shape as stock gRPC C++. Drives the whole contract: Init ->
// StartLeScan -> stream adverts -> StopLeScan -> GATT battery read. Works
// against this server or any stock gRPC server speaking ble.proto. Exits 0
// when the round-trip checks out, 1 otherwise.
#include <cstdio>
#include <cstring>

#include <grpcudspp/health.h>

#include "ble.grpc.pb.h"

int main(int argc, char** argv) {
    const char* sock = argc > 1 ? argv[1] : "/tmp/grpcuds-ble.sock";
    // Library events (connect failures, deadline expiry, reconnects) to
    // stderr — silent by default without this.
    grpcuds::EnableStderrLogging(grpcuds::LOG_INFO);
    grpcuds::Client client(sock);
    if (!client) {
        std::fprintf(stderr, "connect %s failed (is the server running?)\n", sock);
        return 1;
    }
    auto stub = ble::BleScanner::NewStub(client);

    // Health first — the standard grpc.health.v1 probe any stock tool can
    // do. The wire helpers in <grpcudspp/health.h> encode/decode the two
    // tiny protocol messages.
    {
        std::vector<uint8_t> req8 =
            grpcuds::health::EncodeCheckRequest("ble.BleScanner");
        std::vector<uint8_t> out8;
        grpcuds::Status hs = client.UnaryRaw("/grpc.health.v1.Health/Check",
                                             req8.data(), req8.size(), &out8);
        int status = 0;
        if (!hs.ok() ||
            !grpcuds::health::DecodeResponse(out8.data(), out8.size(), &status) ||
            status != grpcuds::health::SERVING) {
            std::fprintf(stderr, "health check failed (status=%d)\n", status);
            return 1;
        }
        std::printf("health(ble.BleScanner) -> SERVING\n");

        // Unknown services must fail NOT_FOUND, per the protocol.
        req8 = grpcuds::health::EncodeCheckRequest("no.such.Service");
        hs = client.UnaryRaw("/grpc.health.v1.Health/Check", req8.data(),
                             req8.size(), &out8);
        if (hs.error_code() != grpcuds::NOT_FOUND) return 1;
    }

    // Adapter up.
    ::ble_InitReply init = ble_InitReply_init_zero;
    grpcuds::Status s = stub->Init(ble_InitRequest_init_zero, &init);
    if (!s.ok() || !init.ok) {
        std::fprintf(stderr, "Init failed: %s\n", s.error_message().c_str());
        return 1;
    }
    std::printf("Init -> adapter %s\n", init.adapter);

    // Scan for adverts at -70 dBm or better.
    ::ble_StartLeScanRequest sreq = ble_StartLeScanRequest_init_zero;
    sreq.min_rssi = -70;
    ::ble_StartLeScanReply srep = ble_StartLeScanReply_init_zero;
    s = stub->StartLeScan(sreq, &srep);
    if (!s.ok() || !srep.ok) return 1;

    uint32_t adverts = 0;
    char first_mac[18] = {0};
    {
        auto reader = stub->ScanResultStream(ble_ScanResultStreamRequest_init_zero);
        ::ble_ScanResult r = ble_ScanResult_init_zero;
        while (reader.Read(&r)) {
            std::printf("scan %s rssi=%d%s%s\n", r.mac, r.rssi, r.name[0] ? " name=" : "",
                        r.name);
            if (r.rssi < -70) {
                std::fprintf(stderr, "min_rssi filter violated (%d)\n", r.rssi);
                return 1;
            }
            if (!first_mac[0]) std::snprintf(first_mac, sizeof(first_mac), "%s", r.mac);
            ++adverts;
            r = ble_ScanResult_init_zero;
        }
        if (reader.status().error_code() != grpcuds::OK) {
            std::fprintf(stderr, "scan stream failed: %s\n",
                         reader.status().error_message().c_str());
            return 1;
        }
    }
    if (adverts == 0) return 1;

    ::ble_StopLeScanReply stop = ble_StopLeScanReply_init_zero;
    s = stub->StopLeScan(ble_StopLeScanRequest_init_zero, &stop);
    if (!s.ok() || stop.adverts_seen != adverts) {
        std::fprintf(stderr, "StopLeScan: seen=%u, streamed=%u\n", stop.adverts_seen, adverts);
        return 1;
    }
    std::printf("StopLeScan -> %u adverts\n", stop.adverts_seen);

    // GATT: battery level of the strongest device.
    ::ble_ReadCharacteristicRequest creq = ble_ReadCharacteristicRequest_init_zero;
    std::snprintf(creq.mac, sizeof(creq.mac), "%s", first_mac);
    std::snprintf(creq.uuid, sizeof(creq.uuid), "2a19");
    ::ble_ReadCharacteristicReply crep = ble_ReadCharacteristicReply_init_zero;
    s = stub->ReadCharacteristic(creq, &crep);
    if (!s.ok() || crep.value.size != 1) return 1;
    std::printf("battery(%s) -> %u%%\n", creq.mac, crep.value.bytes[0]);

    // And the error path: an unknown device is a clean NOT_FOUND, not a hang.
    std::snprintf(creq.mac, sizeof(creq.mac), "00:00:00:00:00:00");
    s = stub->ReadCharacteristic(creq, &crep);
    if (s.error_code() != grpcuds::NOT_FOUND) return 1;

    std::printf("example: OK\n");
    return 0;
}
