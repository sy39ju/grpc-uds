// SPDX-License-Identifier: MIT OR Apache-2.0
//
// BLE scanner server — the canonical grpcuds shape in one file:
//
//   - unary handlers fill the reply and return (Init / StartLeScan /
//     StopLeScan / ReadCharacteristic);
//   - the streaming handler returns IMMEDIATELY and hands the writer to a
//     producer thread (the simulated radio), which pushes one ScanResult per
//     advertisement and Finish()es when the scan stops. ServerWriter is a
//     cheap mailbox handle — Write/Finish are thread-safe from any thread.
//
// The "radio" is a fixed device table swept on a timer; replace produce()
// with your platform's scan callback and nothing else changes.
#include <grpcudspp/grpcudspp.h>
#include <grpcudspp/health.h>

#include <unistd.h>

#include <atomic>
#include <chrono>
#include <csignal>
#include <cstdio>
#include <cstring>
#include <string>
#include <thread>

#include "ble.grpc.pb.h"

namespace {

// ---- the simulated radio -----------------------------------------------------
struct Device {
    const char* mac;
    const char* name;
    int32_t rssi;          // base signal strength, dBm
    uint8_t adv[3];        // flags + 16-bit service UUID (battery)
    uint8_t battery_pct;   // GATT 0x2a19 value
};

constexpr Device kDevices[] = {
    {"D4:3A:2C:11:22:33", "thermo-1", -48, {0x06, 0x0F, 0x18}, 87},
    {"F0:99:B6:44:55:66", "tag-7", -63, {0x06, 0x0F, 0x18}, 52},
    {"C8:5C:21:77:88:99", "", -81, {0x04, 0x0F, 0x18}, 14},  // no local name
};
constexpr int kSweeps = 4;  // adverts per device for the demo scan

}  // namespace

class BleScannerImpl final : public ble::BleScanner::Service {
 public:
    grpcuds::Status Init(grpcuds::ServerContext*, const ::ble_InitRequest*,
                         ::ble_InitReply* reply) override {
        reply->ok = true;
        std::snprintf(reply->adapter, sizeof(reply->adapter), "hci0-sim");
        return grpcuds::Status::Ok();
    }

    grpcuds::Status StartLeScan(grpcuds::ServerContext*, const ::ble_StartLeScanRequest* request,
                                ::ble_StartLeScanReply* reply) override {
        min_rssi_ = request->min_rssi;
        adverts_seen_ = 0;
        scanning_ = true;
        reply->ok = true;
        return grpcuds::Status::Ok();
    }

    grpcuds::Status ScanResultStream(grpcuds::ServerContext*, const ::ble_ScanResultStreamRequest*,
                                     grpcuds::ServerWriter<::ble_ScanResult>* writer) override {
        // `producer_live_` (cleared by the radio thread itself on exit) is
        // the "stream already open" signal — `scanning_` can't be it, since
        // StartLeScan sets that before the stream ever opens.
        if (producer_live_) {
            return grpcuds::Status(grpcuds::ALREADY_EXISTS, "a scan stream is already open");
        }
        // Reap the previous scan's thread: a finished thread stays joinable
        // until joined, and without this a SECOND client's scan would
        // bounce forever. (At worst it is still exiting — µs of wait.)
        if (producer_.joinable()) producer_.join();
        // Hand the stream to the radio thread and return — the handler must
        // not block (it runs on the I/O thread).
        producer_live_ = true;
        producer_ = std::thread(&BleScannerImpl::produce, this, *writer);
        return grpcuds::Status::Ok();
    }

    grpcuds::Status StopLeScan(grpcuds::ServerContext*, const ::ble_StopLeScanRequest*,
                               ::ble_StopLeScanReply* reply) override {
        scanning_ = false;
        reply->adverts_seen = adverts_seen_;
        return grpcuds::Status::Ok();
    }

    grpcuds::Status ReadCharacteristic(grpcuds::ServerContext*,
                                       const ::ble_ReadCharacteristicRequest* request,
                                       ::ble_ReadCharacteristicReply* reply) override {
        if (std::strcmp(request->uuid, "2a19") != 0) {  // battery level
            return grpcuds::Status(grpcuds::UNIMPLEMENTED, "only uuid 2a19 is simulated");
        }
        for (const Device& d : kDevices) {
            if (std::strcmp(request->mac, d.mac) == 0) {
                reply->value.size = 1;
                reply->value.bytes[0] = d.battery_pct;
                return grpcuds::Status::Ok();
            }
        }
        return grpcuds::Status(grpcuds::NOT_FOUND, "no such device");
    }

    // Stop the radio and reap the producer — call after the I/O loop is down.
    void Shutdown() {
        scanning_ = false;
        if (producer_.joinable()) producer_.join();
    }

 private:
    void produce(grpcuds::ServerWriter<::ble_ScanResult> writer) {
        // Deterministic rssi jitter stands in for the air interface.
        for (int sweep = 0; scanning_ && sweep < kSweeps; ++sweep) {
            int i = 0;
            for (const Device& d : kDevices) {
                int32_t rssi = d.rssi - ((sweep * 7 + i++ * 3) % 5);
                if (min_rssi_ != 0 && rssi < min_rssi_) continue;
                ::ble_ScanResult r = ble_ScanResult_init_zero;
                std::snprintf(r.mac, sizeof(r.mac), "%s", d.mac);
                std::snprintf(r.name, sizeof(r.name), "%s", d.name);
                r.rssi = rssi;
                r.adv_data.size = sizeof(d.adv);
                std::memcpy(r.adv_data.bytes, d.adv, sizeof(d.adv));
                if (!writer.Write(r)) {  // client went away
                    scanning_ = false;
                    producer_live_ = false;
                    return;
                }
                ++adverts_seen_;
            }
            std::this_thread::sleep_for(std::chrono::milliseconds(30));
        }
        writer.Finish(grpcuds::Status::Ok());
        producer_live_ = false;
    }

    std::atomic<bool> scanning_{false};
    std::atomic<bool> producer_live_{false};
    std::atomic<int32_t> min_rssi_{0};
    std::atomic<uint32_t> adverts_seen_{0};
    std::thread producer_;
};

namespace {
volatile sig_atomic_t g_stop = 0;
void on_signal(int) { g_stop = 1; }
}  // namespace

int main(int argc, char** argv) {
    const std::string sock = argc > 1 ? argv[1] : "/tmp/grpcuds-ble.sock";
    ::unlink(sock.c_str());
    std::signal(SIGINT, on_signal);
    std::signal(SIGTERM, on_signal);

    // The library is silent by default; route its events (accept, peer
    // EOF, deadline expiry, ...) to stderr. DEBUG so a daemon under
    // development shows its connection lifecycle; use LOG_INFO or your
    // own SetLogCallback sink in production.
    grpcuds::EnableStderrLogging(grpcuds::LOG_DEBUG);

    BleScannerImpl service;
    // Standard health checking (grpc.health.v1) — stock probers
    // (grpc_health_probe, grpcurl, tonic-health) work out of the box.
    grpcuds::health::HealthService health;
    health.SetStatus("ble.BleScanner", grpcuds::health::SERVING);
    grpcuds::ServerBuilder builder;
    builder.AddListeningPort("unix:" + sock);
    builder.RegisterService(&service);
    builder.RegisterService(&health);
    auto server = builder.BuildAndStart();
    if (!server) {
        std::fprintf(stderr, "BuildAndStart failed\n");
        return 1;
    }
    {
        grpcuds::ServerThread io(std::move(server));
        std::printf("READY\n");  // sentinel for run_demo.sh
        std::printf("ble-server on unix:%s (Ctrl-C to stop)\n", sock.c_str());
        std::fflush(stdout);
        while (!g_stop) pause();
    }  // I/O loop down first...
    service.Shutdown();  // ...then reap the radio thread.
    return 0;
}
