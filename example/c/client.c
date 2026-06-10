// SPDX-License-Identifier: MIT OR Apache-2.0
//
// grpcuds from plain C — the BLE scanner client, the same contract the C++
// example (../ble) drives, using the GENERATED typed wrappers
// (protoc-gen-grpcudspp --grpcudspp_opt=c). Sequence: Init -> StartLeScan
// -> stream adverts -> StopLeScan -> GATT battery read. Exits 0 on success.
// It first runs a standard grpc.health.v1 Check probe (the server registers
// the health service via grpcuds_health_register).

#include <stdio.h>
#include <string.h>

#include <grpcuds.h>

#include "ble.grpcuds.h"

static void log_sink(int level, const char* msg, int64_t arg, void* ud) {
    (void)ud;
    fprintf(stderr, "grpcuds[%c] %s (arg=%lld)\n", "EID"[level], msg,
            (long long)arg);
}

#define CHECK(cond)                                                        \
    do {                                                                   \
        if (!(cond)) {                                                     \
            fprintf(stderr, "CHECK failed: %s (%s:%d)\n", #cond, __FILE__, \
                    __LINE__);                                             \
            return 1;                                                      \
        }                                                                  \
    } while (0)

int main(int argc, char** argv) {
    const char* sock = argc > 1 ? argv[1] : "/tmp/grpcuds-ble-c.sock";
    grpcuds_set_log_callback(log_sink, GRPCUDS_LOG_INFO, NULL);

    grpcuds_client* c = grpcuds_client_connect_wait(sock, 3000);
    if (!c) {
        fprintf(stderr, "connect %s failed (is the server running?)\n", sock);
        return 1;
    }

    // Standard health checking — the grpc.health.v1 Check any stock prober does.
    // Overall ("") is SERVING; an unknown service fails NOT_FOUND per the spec.
    {
        grpcuds_response* h = grpcuds_client_unary(
            c, "/grpc.health.v1.Health/Check", NULL, 0);
        CHECK(h && grpcuds_response_status(h) == GRPCUDS_OK);
        size_t n = 0;
        const uint8_t* body = grpcuds_response_body(h, &n);
        // HealthCheckResponse{status=SERVING} = {0x08, 0x01}.
        CHECK(n == 2 && body[0] == 0x08 && body[1] == GRPCUDS_HEALTH_SERVING);
        printf("health(overall) -> SERVING\n");
        grpcuds_response_free(h);

        // Check{service="ghost"} = 0x0A <len> "ghost" -> NOT_FOUND.
        unsigned char req[8] = {0x0A, 0x05, 'g', 'h', 'o', 's', 't'};
        h = grpcuds_client_unary(c, "/grpc.health.v1.Health/Check", req, 7);
        CHECK(h && grpcuds_response_status(h) == GRPCUDS_NOT_FOUND);
        printf("health(ghost) -> NOT_FOUND\n");
        grpcuds_response_free(h);

        // Watch{service="ble.BleScanner"} streams the current status, then every
        // change. Read the immediate SERVING, then cancel (frees the stream).
        const char* svc = "ble.BleScanner";
        size_t sl = strlen(svc);
        unsigned char wreq[2 + 32];
        wreq[0] = 0x0A;
        wreq[1] = (unsigned char)sl;
        memcpy(wreq + 2, svc, sl);
        grpcuds_stream* ws = grpcuds_client_server_streaming(
            c, "/grpc.health.v1.Health/Watch", wreq, 2 + sl);
        CHECK(ws);
        size_t wn = 0;
        const uint8_t* wmsg = grpcuds_stream_next(ws, &wn);
        CHECK(wmsg && wn == 2 && wmsg[0] == 0x08 && wmsg[1] == GRPCUDS_HEALTH_SERVING);
        printf("health watch(%s) -> SERVING\n", svc);
        grpcuds_stream_free(ws); // cancels the stream -> server cancel hook fires
    }

    // Init the adapter.
    ble_InitRequest ireq = ble_InitRequest_init_zero;
    ble_InitReply irep = ble_InitReply_init_zero;
    CHECK(ble_BleScanner_Init(c, &ireq, &irep) == GRPCUDS_OK);
    CHECK(irep.ok);
    printf("Init -> adapter %s\n", irep.adapter);

    // Start a scan that drops adverts weaker than -70 dBm.
    ble_StartLeScanRequest sreq = ble_StartLeScanRequest_init_zero;
    sreq.min_rssi = -70;
    ble_StartLeScanReply srep = ble_StartLeScanReply_init_zero;
    CHECK(ble_BleScanner_StartLeScan(c, &sreq, &srep) == GRPCUDS_OK);
    CHECK(srep.ok);

    // Stream the adverts until the server closes the stream.
    ble_ScanResultStreamRequest streamreq = ble_ScanResultStreamRequest_init_zero;
    grpcuds_stream* st = ble_BleScanner_ScanResultStream_start(c, &streamreq);
    CHECK(st != NULL);
    int count = 0;
    ble_ScanResult r;
    int rc;
    while ((rc = ble_BleScanner_ScanResultStream_next(st, &r)) == 1) {
        printf("scan %s rssi=%d name=%s\n", r.mac, (int)r.rssi,
               r.name[0] ? r.name : "(none)");
        ++count;
    }
    CHECK(rc == 0); // 0 = clean end of stream (not a decode error)
    CHECK(grpcuds_stream_status(st) == GRPCUDS_OK);
    grpcuds_stream_free(st);

    // Stop, and confirm the count matches what we streamed.
    ble_StopLeScanRequest streq = ble_StopLeScanRequest_init_zero;
    ble_StopLeScanReply strep = ble_StopLeScanReply_init_zero;
    CHECK(ble_BleScanner_StopLeScan(c, &streq, &strep) == GRPCUDS_OK);
    printf("StopLeScan -> %u adverts\n", strep.adverts_seen);
    CHECK(strep.adverts_seen == (uint32_t)count);

    // GATT: read the battery level (0x2a19) of the first device.
    ble_ReadCharacteristicRequest greq = ble_ReadCharacteristicRequest_init_zero;
    snprintf(greq.mac, sizeof(greq.mac), "D4:3A:2C:11:22:33");
    snprintf(greq.uuid, sizeof(greq.uuid), "2a19");
    ble_ReadCharacteristicReply grep = ble_ReadCharacteristicReply_init_zero;
    CHECK(ble_BleScanner_ReadCharacteristic(c, &greq, &grep) == GRPCUDS_OK);
    CHECK(grep.value.size == 1);
    printf("battery(%s) -> %u%%\n", greq.mac, grep.value.bytes[0]);

    // An unsupported characteristic returns UNIMPLEMENTED, not a crash.
    snprintf(greq.uuid, sizeof(greq.uuid), "0000");
    CHECK(ble_BleScanner_ReadCharacteristic(c, &greq, &grep) ==
          GRPCUDS_UNIMPLEMENTED);

    printf("example: OK\n");
    grpcuds_client_free(c);
    return 0;
}
