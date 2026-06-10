// SPDX-License-Identifier: MIT OR Apache-2.0
//
// grpcuds from plain C — a BLE scanner server, the same contract the C++
// example (../ble) implements, using the GENERATED C service stubs
// (protoc-gen-grpcudspp --grpcudspp_opt=c). One simulated radio, two .c
// files, no C++ anywhere.
//
//   1. fill a ble_BleScanner_service table of handler function pointers,
//   2. ble_BleScanner_register() it (the generated trampolines do the
//      nanopb decode/encode),
//   3. drive the fds from your own poll(2) loop.
//
// Streaming note: scan results are produced on a SEPARATE thread (a stand-in
// for a real radio whose adverts arrive off the I/O thread). The
// ScanResultStream handler spawns that producer and returns immediately
// (deferred); the producer calls ble_BleScanner_ScanResultStream_send/_finish
// from its own thread. Those are thread-safe — grpcuds_call_write/_finish
// route off-I/O-thread writes through the outbound mailbox in the C ABI, drained
// on the I/O thread (grpcuds_mailbox_drain). The I/O thread registers itself
// (grpcuds_mailbox_register_io_thread) and watches the mailbox wakeup fd. This
// is the same shape as the C++ example (../ble), with the mailbox now in the
// shared C ABI. See docs/THREADING.md.
//
// Stock gRPC clients (grpcurl, tonic, grpc++) can call this server.

#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include <grpcuds.h>

#include "ble.grpcuds.h"

// ---- the simulated radio (mirrors ../ble/server_main.cc) -------------------

typedef struct {
    const char* mac;
    const char* name;
    int32_t rssi;        // base signal strength, dBm
    uint8_t adv[3];      // flags + 16-bit service UUID (battery)
    uint8_t battery_pct; // GATT 0x2a19 value
} Device;

static const Device kDevices[] = {
    {"D4:3A:2C:11:22:33", "thermo-1", -48, {0x06, 0x0F, 0x18}, 87},
    {"F0:99:B6:44:55:66", "tag-7", -63, {0x06, 0x0F, 0x18}, 52},
    {"C8:5C:21:77:88:99", "", -81, {0x04, 0x0F, 0x18}, 14}, // no local name
};
#define N_DEVICES ((int)(sizeof(kDevices) / sizeof(kDevices[0])))
#define N_SWEEPS 4 // adverts per device for the demo scan

// Per-server state, handed to every handler via svc.user_data.
typedef struct {
    int32_t min_rssi;            // StartLeScan filter (0 = none)
    atomic_uint adverts_seen;    // counted by the producer thread, read by StopLeScan
} BleState;

// ---- logging: the library is silent until given a sink --------------------

static void log_sink(int level, const char* msg, int64_t arg, void* ud) {
    (void)ud;
    fprintf(stderr, "grpcuds[%c] %s (arg=%lld)\n", "EID"[level], msg,
            (long long)arg);
}

// ---- handlers: typed in, typed out (the generated trampolines en/decode) --

static int on_init(grpcuds_call_ref ref, const ble_InitRequest* req,
                   ble_InitReply* resp, void* ud) {
    (void)ref;
    (void)req;
    (void)ud;
    resp->ok = true;
    snprintf(resp->adapter, sizeof(resp->adapter), "hci0-sim");
    return GRPCUDS_OK;
}

static int on_start_scan(grpcuds_call_ref ref, const ble_StartLeScanRequest* req,
                         ble_StartLeScanReply* resp, void* ud) {
    (void)ref;
    BleState* st = (BleState*)ud;
    st->min_rssi = req->min_rssi;
    atomic_store(&st->adverts_seen, 0);
    resp->ok = true;
    return GRPCUDS_OK;
}

// What the producer thread needs: the call handle, a snapshot of the scan
// filter (so it never reads I/O-thread state), and where to count adverts.
typedef struct {
    grpcuds_call_ref ref;
    int32_t min_rssi;
    BleState* st;
} ScanJob;

// The "radio": runs on its own thread, pushing adverts through the mailbox.
// ble_BleScanner_ScanResultStream_send/_finish wrap grpcuds_call_write/_finish,
// which are thread-safe — off the I/O thread they enqueue + poke the wakeup fd.
static void* scan_producer(void* arg) {
    ScanJob* job = (ScanJob*)arg;
    for (int sweep = 0; sweep < N_SWEEPS; ++sweep) {
        for (int i = 0; i < N_DEVICES; ++i) {
            const Device* d = &kDevices[i];
            int32_t rssi = d->rssi - ((sweep * 7 + i * 3) % 5);
            if (job->min_rssi != 0 && rssi < job->min_rssi) continue;

            ble_ScanResult r = ble_ScanResult_init_zero;
            snprintf(r.mac, sizeof(r.mac), "%s", d->mac);
            snprintf(r.name, sizeof(r.name), "%s", d->name);
            r.rssi = rssi;
            r.adv_data.size = sizeof(d->adv);
            memcpy(r.adv_data.bytes, d->adv, sizeof(d->adv));
            if (ble_BleScanner_ScanResultStream_send(job->ref, &r) != 0) {
                goto done; // stream gone (client cancelled / server closing)
            }
            atomic_fetch_add(&job->st->adverts_seen, 1);
        }
    }
    ble_BleScanner_ScanResultStream_finish(job->ref, GRPCUDS_OK);
done:
    free(job);
    return NULL;
}

// Server-streaming: hand the stream to the producer thread and return at once
// (deferred — returning GRPCUDS_OK without finishing keeps the call open).
static int on_scan_stream(grpcuds_call_ref ref,
                          const ble_ScanResultStreamRequest* req, void* ud) {
    (void)req;
    BleState* st = (BleState*)ud;
    ScanJob* job = (ScanJob*)malloc(sizeof(*job));
    if (!job) return GRPCUDS_RESOURCE_EXHAUSTED;
    job->ref = ref;
    job->min_rssi = st->min_rssi;
    job->st = st;

    pthread_t tid;
    if (pthread_create(&tid, NULL, scan_producer, job) != 0) {
        free(job);
        return GRPCUDS_INTERNAL;
    }
    pthread_detach(tid);
    return GRPCUDS_OK; // the producer finishes the stream off-thread
}

static int on_stop_scan(grpcuds_call_ref ref, const ble_StopLeScanRequest* req,
                        ble_StopLeScanReply* resp, void* ud) {
    (void)ref;
    (void)req;
    BleState* st = (BleState*)ud;
    resp->adverts_seen = atomic_load(&st->adverts_seen);
    return GRPCUDS_OK;
}

static int on_read_char(grpcuds_call_ref ref,
                        const ble_ReadCharacteristicRequest* req,
                        ble_ReadCharacteristicReply* resp, void* ud) {
    (void)ref;
    (void)ud;
    if (strcmp(req->uuid, "2a19") != 0) { // battery level only
        return GRPCUDS_UNIMPLEMENTED;
    }
    for (int i = 0; i < N_DEVICES; ++i) {
        if (strcmp(req->mac, kDevices[i].mac) == 0) {
            resp->value.size = 1;
            resp->value.bytes[0] = kDevices[i].battery_pct;
            return GRPCUDS_OK;
        }
    }
    return GRPCUDS_NOT_FOUND;
}

// ---- the event loop ---------------------------------------------------------

static volatile sig_atomic_t g_stop = 0;
static void on_signal(int sig) {
    (void)sig;
    g_stop = 1;
}

#define MAX_CONNS 16

int main(int argc, char** argv) {
    const char* sock = argc > 1 ? argv[1] : "/tmp/grpcuds-ble-c.sock";
    unlink(sock);
    signal(SIGINT, on_signal);
    signal(SIGTERM, on_signal);
    grpcuds_set_log_callback(log_sink, GRPCUDS_LOG_DEBUG, NULL);

    grpcuds_server* server = grpcuds_server_new();
    if (!server || grpcuds_server_bind_uds(server, sock) != 0) {
        fprintf(stderr, "bind %s failed\n", sock);
        return 1;
    }

    BleState state = {0, 0};
    ble_BleScanner_service svc;
    memset(&svc, 0, sizeof(svc)); // unimplemented RPCs stay NULL
    svc.user_data = &state;
    svc.Init = on_init;
    svc.StartLeScan = on_start_scan;
    svc.ScanResultStream = on_scan_stream;
    svc.StopLeScan = on_stop_scan;
    svc.ReadCharacteristic = on_read_char;
    if (ble_BleScanner_register(server, &svc) != 0) {
        fprintf(stderr, "register failed\n");
        return 1;
    }

    // Standard health checking: stock probers (grpc_health_probe, grpcurl,
    // tonic-health) can ask "is this daemon serving?" over the same socket.
    grpcuds_health_register(server);
    grpcuds_health_set_status("ble.BleScanner", GRPCUDS_HEALTH_SERVING);

    // This thread runs the poll loop and drains the mailbox: mark it the I/O
    // thread, and watch the mailbox wakeup fd so a producer's write wakes poll().
    grpcuds_mailbox_register_io_thread();
    int wakeup_fd = grpcuds_mailbox_wakeup_fd();

    printf("READY\n");
    printf("ble-c server on unix:%s (Ctrl-C to stop)\n", sock);
    fflush(stdout);

    grpcuds_conn* conns[MAX_CONNS] = {0};
    while (!g_stop) {
        struct pollfd fds[2 + MAX_CONNS];
        grpcuds_conn* by_slot[2 + MAX_CONNS] = {0};
        // fds[0] = listener, fds[1] = mailbox wakeup, fds[2..] = connections.
        fds[0].fd = grpcuds_server_listener_fd(server);
        fds[0].events = POLLIN;
        fds[1].fd = wakeup_fd;
        fds[1].events = POLLIN;
        nfds_t n = 2;
        for (int i = 0; i < MAX_CONNS; ++i) {
            if (!conns[i]) continue;
            fds[n].fd = grpcuds_conn_fd(conns[i]);
            fds[n].events = POLLIN;
            if (grpcuds_conn_wants_write(conns[i])) fds[n].events |= POLLOUT;
            by_slot[n] = conns[i];
            ++n;
        }

        int timeout_ms = 1000;
        for (int i = 0; i < MAX_CONNS; ++i) {
            if (!conns[i]) continue;
            int64_t d = grpcuds_conn_next_deadline_ms(conns[i]);
            if (d >= 0 && d < timeout_ms) timeout_ms = (int)d;
        }

        if (poll(fds, n, timeout_ms) < 0) continue; // EINTR -> re-check g_stop

        // Flush any writes a producer thread queued while we were in poll().
        // (Cheap when empty; the wakeup fd just makes poll() return promptly.)
        grpcuds_mailbox_drain();

        if (fds[0].revents & POLLIN) {
            grpcuds_conn* c;
            while ((c = grpcuds_server_accept(server)) != NULL) {
                int slot = -1;
                for (int i = 0; i < MAX_CONNS; ++i) {
                    if (!conns[i]) {
                        slot = i;
                        break;
                    }
                }
                if (slot < 0) {
                    grpcuds_conn_free(c); // full house — drop the conn
                } else {
                    conns[slot] = c;
                }
            }
        }
        for (nfds_t s = 2; s < n; ++s) {
            grpcuds_conn* c = by_slot[s];
            if (!c) continue;
            if (fds[s].revents == 0 && grpcuds_conn_next_deadline_ms(c) != 0) {
                continue;
            }
            if (grpcuds_conn_tick(c) != 0) { // 1 = closed, <0 = -errno
                for (int i = 0; i < MAX_CONNS; ++i) {
                    if (conns[i] == c) conns[i] = NULL;
                }
                grpcuds_conn_free(c);
            }
        }
    }

    // Final drain so any writes queued just before shutdown reach the wire.
    grpcuds_mailbox_drain();
    for (int i = 0; i < MAX_CONNS; ++i) {
        if (conns[i]) grpcuds_conn_tick(conns[i]);
    }
    for (int i = 0; i < MAX_CONNS; ++i) {
        if (conns[i]) grpcuds_conn_free(conns[i]);
    }
    grpcuds_server_free(server);
    printf("ble-c server: clean shutdown\n");
    return 0;
}
