// SPDX-License-Identifier: MIT OR Apache-2.0
//
// grpcuds::health — the standard gRPC health checking service
// (`grpc.health.v1.Health`) for C++ servers, header-only.
//
// Stock tooling (grpc_health_probe, grpcurl, tonic-health / grpc++ health
// clients) can ask "is this daemon serving?" over the normal socket:
//
//     grpcuds::health::HealthService health;     // "" starts SERVING
//     builder.RegisterService(&health);
//     health.SetStatus("ble.BleScanner", grpcuds::health::SERVING);
//     // ... later, e.g. when the radio dies:
//     health.SetStatus("ble.BleScanner", grpcuds::health::NOT_SERVING);
//
// Check is unary (unknown services fail NOT_FOUND, per the protocol);
// Watch is server-streaming (immediate status — SERVICE_UNKNOWN for
// unregistered names — then every change). SetStatus is thread-safe;
// watcher writes ride the outbound mailbox.
//
// The two protocol messages are one string field and one varint enum, so
// they are encoded/decoded right here — no nanopb run, no generated code.
// (The Rust twin, `grpcuds::health` behind the `health` feature, pins the
// same bytes in its unit tests; tonic-health conformance runs in CI.)

#ifndef GRPCUDSPP_HEALTH_H_
#define GRPCUDSPP_HEALTH_H_

#include <cstdint>
#include <memory>
#include <mutex>
#include <string>
#include <vector>

#include "grpcudspp/server_context.h"
#include "grpcudspp/server_writer.h"
#include "grpcudspp/service.h"
#include "grpcudspp/status.h"

namespace grpcuds {
namespace health {

/// `grpc.health.v1.HealthCheckResponse.ServingStatus`.
enum ServingStatus {
    UNKNOWN = 0,
    SERVING = 1,
    NOT_SERVING = 2,
    SERVICE_UNKNOWN = 3,  // Watch-only: name not registered
};

// ---- wire helpers (field 1 only, proto3) -------------------------------------
// HealthCheckRequest  = { string service = 1; }   -> 0x0A <varint len> bytes
// HealthCheckResponse = { ServingStatus status = 1; } -> 0x08 <varint>
// proto3 default (empty string / UNKNOWN) encodes as zero bytes.

inline std::vector<uint8_t> EncodeCheckRequest(const std::string& service) {
    std::vector<uint8_t> out;
    if (service.empty()) return out;
    out.push_back(0x0A);
    size_t len = service.size();
    while (len >= 0x80) {
        out.push_back(static_cast<uint8_t>(len) | 0x80);
        len >>= 7;
    }
    out.push_back(static_cast<uint8_t>(len));
    out.insert(out.end(), service.begin(), service.end());
    return out;
}

inline bool DecodeCheckRequest(const uint8_t* d, size_t n, std::string* service) {
    service->clear();
    size_t i = 0;
    while (i < n) {
        if (d[i] != 0x0A) return false;  // only field 1 exists
        ++i;
        uint64_t len = 0;
        int shift = 0;
        while (i < n && (d[i] & 0x80)) {
            if (shift >= 64) return false;  // malformed: a >64-bit varint
            len |= static_cast<uint64_t>(d[i] & 0x7F) << shift;
            shift += 7;
            ++i;
        }
        if (i >= n || shift >= 64) return false;
        len |= static_cast<uint64_t>(d[i]) << shift;
        ++i;
        if (len > n - i) return false;
        service->assign(reinterpret_cast<const char*>(d + i), len);
        i += len;
    }
    return true;
}

inline std::vector<uint8_t> EncodeResponse(ServingStatus status) {
    std::vector<uint8_t> out;
    if (status == UNKNOWN) return out;  // proto3 default
    out.push_back(0x08);
    out.push_back(static_cast<uint8_t>(status));  // all values < 0x80
    return out;
}

inline bool DecodeResponse(const uint8_t* d, size_t n, int* status) {
    *status = 0;  // proto3 default when absent
    size_t i = 0;
    while (i < n) {
        if (d[i] != 0x08) return false;
        ++i;
        uint64_t v = 0;
        int shift = 0;
        while (i < n && (d[i] & 0x80)) {
            if (shift >= 64) return false;  // malformed: a >64-bit varint
            v |= static_cast<uint64_t>(d[i] & 0x7F) << shift;
            shift += 7;
            ++i;
        }
        if (i >= n || shift >= 64) return false;
        v |= static_cast<uint64_t>(d[i]) << shift;
        ++i;
        *status = static_cast<int>(v);
    }
    return true;
}

// ---- the service --------------------------------------------------------------

class HealthService : public Service {
 public:
    HealthService() { statuses_.emplace_back("", SERVING); }

    /// Set (or register) `service`'s status and notify its watchers.
    /// `""` is the server overall. Thread-safe.
    void SetStatus(const std::string& service, ServingStatus status) {
        std::vector<uint8_t> msg = EncodeResponse(status);
        std::vector<std::pair<void*, int32_t>> targets;
        {
            std::lock_guard<std::mutex> lock(mu_);
            FindOrAddStatus(service)->second = status;
            for (auto& w : watchers_) {
                if (w->live && w->service == service) {
                    targets.emplace_back(w->call, w->call_id);
                }
            }
        }
        // Write OUTSIDE mu_: on the I/O thread a write re-enters the core,
        // which may close streams and fire OnCancel synchronously — and
        // OnCancel takes mu_ (std::mutex is non-recursive). Off the I/O
        // thread the writes ride the outbound mailbox, whose tombstone
        // registry drops items for connections freed after this snapshot.
        for (auto& t : targets) {
            RawWriter(t.first, t.second).Write(msg.data(), msg.size());
        }
    }

    void BindToServer(grpcuds_server* server) override {
        grpcuds_server_register_method(server, "/grpc.health.v1.Health/Check",
                                       &CheckTr, this);
        grpcuds_server_register_method(server, "/grpc.health.v1.Health/Watch",
                                       &WatchTr, this);
    }

 private:
    // One Watch subscription. `live` flips off when the client cancels;
    // the slot itself stays owned by the service (stable address for the
    // cancel hook) and is reaped lazily when the next watcher registers.
    struct Watcher {
        HealthService* svc;
        void* call;
        int32_t call_id;
        std::string service;
        bool live = true;
    };

    // Flat linear scans on purpose: a daemon registers a handful of
    // services, and std::map's node machinery costs tens of KB in an
    // unoptimized embedded build for nothing at this scale.
    std::pair<std::string, ServingStatus>* FindStatus(const std::string& service) {
        for (auto& e : statuses_) {
            if (e.first == service) return &e;
        }
        return nullptr;
    }
    std::pair<std::string, ServingStatus>* FindOrAddStatus(const std::string& service) {
        if (auto* e = FindStatus(service)) return e;
        statuses_.emplace_back(service, UNKNOWN);
        return &statuses_.back();
    }

    static int CheckTr(void* call, int32_t call_id, const uint8_t* req,
                       size_t req_len, void* ud) {
        auto* self = static_cast<HealthService*>(ud);
        RawWriter w(call, call_id);
        std::string service;
        if (!DecodeCheckRequest(req, req_len, &service)) {
            w.Finish(Status(INVALID_ARGUMENT, "malformed HealthCheckRequest"));
            return GRPCUDS_OK;
        }
        ServingStatus status;
        {
            std::lock_guard<std::mutex> lock(self->mu_);
            auto* e = self->FindStatus(service);
            if (e == nullptr) {
                // The protocol: unknown service names fail with NOT_FOUND.
                w.Finish(Status(NOT_FOUND, "unknown service"));
                return GRPCUDS_OK;
            }
            status = e->second;
        }
        std::vector<uint8_t> msg = EncodeResponse(status);
        w.Write(msg.data(), msg.size());
        w.Finish(Status::Ok());
        return GRPCUDS_OK;
    }

    static int WatchTr(void* call, int32_t call_id, const uint8_t* req,
                       size_t req_len, void* ud) {
        auto* self = static_cast<HealthService*>(ud);
        std::string service;
        if (!DecodeCheckRequest(req, req_len, &service)) {
            RawWriter(call, call_id)
                .Finish(Status(INVALID_ARGUMENT, "malformed HealthCheckRequest"));
            return GRPCUDS_OK;
        }
        auto watcher = std::make_unique<Watcher>();
        watcher->svc = self;
        watcher->call = call;
        watcher->call_id = call_id;
        watcher->service = service;
        Watcher* raw = watcher.get();
        ServingStatus current;
        {
            std::lock_guard<std::mutex> lock(self->mu_);
            auto* e = self->FindStatus(service);
            current = e == nullptr ? SERVICE_UNKNOWN : e->second;
            // Reap watchers cancelled since the last subscription.
            auto& ws = self->watchers_;
            for (size_t i = ws.size(); i > 0; --i) {
                if (!ws[i - 1]->live) ws.erase(ws.begin() + (i - 1));
            }
            ws.push_back(std::move(watcher));
        }
        ServerContext(call, call_id).SetCancelHook(&OnCancel, raw);
        // The initial write happens OUTSIDE mu_ for the same reason as in
        // SetStatus: it re-enters the core, which may fire OnCancel for
        // another watcher's stream, and OnCancel takes mu_. Registering
        // first is safe — handlers run on the I/O thread, so nothing can
        // cancel this stream before SetCancelHook above, and a concurrent
        // SetStatus snapshot rides the mailbox, draining after this
        // direct write (initial status stays first on the wire).
        std::vector<uint8_t> msg = EncodeResponse(current);
        RawWriter(call, call_id).Write(msg.data(), msg.size());
        return GRPCUDS_OK;  // stream stays open; SetStatus feeds it
    }

    static void OnCancel(void* user_data) {
        auto* w = static_cast<Watcher*>(user_data);
        std::lock_guard<std::mutex> lock(w->svc->mu_);
        w->live = false;
    }

    std::mutex mu_;
    std::vector<std::pair<std::string, ServingStatus>> statuses_;
    std::vector<std::unique_ptr<Watcher>> watchers_;
};

}  // namespace health
}  // namespace grpcuds

#endif  // GRPCUDSPP_HEALTH_H_
