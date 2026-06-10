// SPDX-License-Identifier: MIT OR Apache-2.0
//
// End-to-end for the plain-C generated stubs (`--grpcudspp_opt=c`):
// the BleService table + register on the server side, the typed call
// wrappers on the client side — one self-checking C11 binary (server poll
// loop in the parent, blocking client in a fork). Covers:
//   - unary round trip THROUGH the deferred path (handler responds via
//     <Rpc>_respond and returns GRPCUDS_HANDLER_DEFERRED),
//   - sync unary with both an OK and an error-status return,
//   - server-streaming via <Rpc>_send/_finish + client _start/_next,
//   - a NULL table entry surfacing as UNIMPLEMENTED to the client.

#include <poll.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

#include "ble.grpcuds.h"

#define CHECK(cond)                                                       \
    do {                                                                  \
        if (!(cond)) {                                                    \
            fprintf(stderr, "CHECK failed: %s (%s:%d)\n", #cond,          \
                    __FILE__, __LINE__);                                  \
            return 1;                                                     \
        }                                                                 \
    } while (0)

// ---- server handlers (the generated service table) -------------------------

static int init_handler(grpcuds_call_ref ref, const ble_InitRequest* req,
                        ble_InitReply* resp, void* ud) {
    (void)req;
    (void)ud;
    // Exercise the DEFERRED contract end-to-end: complete via the generated
    // helper instead of the trampoline ("later", same I/O thread).
    ble_InitReply reply = ble_InitReply_init_zero;
    reply.ok = true;
    (void)resp;
    if (ble_BleService_Init_respond(ref, &reply) != 0) return GRPCUDS_INTERNAL;
    return GRPCUDS_HANDLER_DEFERRED;
}

static int add_filter_handler(grpcuds_call_ref ref,
                              const ble_AddScanFilterRequest* req,
                              ble_AddScanFilterReply* resp, void* ud) {
    (void)ref;
    (void)ud;
    if (req->filter.mac_prefix[0] == '\0') {
        return GRPCUDS_NOT_FOUND; // sync error-status path
    }
    resp->filter_id = 7; // sync OK path: trampoline encodes + finishes
    return GRPCUDS_OK;
}

static int scan_stream_handler(grpcuds_call_ref ref,
                               const ble_ScanResultStreamRequest* req,
                               void* ud) {
    (void)req;
    (void)ud;
    for (int i = 0; i < 3; ++i) {
        ble_ScanResult r = ble_ScanResult_init_zero;
        snprintf(r.mac, sizeof(r.mac), "AA:BB:CC:DD:EE:0%d", i);
        r.rssi = -40 - i;
        if (ble_BleService_ScanResultStream_send(ref, &r) != 0) {
            return GRPCUDS_INTERNAL;
        }
    }
    ble_BleService_ScanResultStream_finish(ref, GRPCUDS_OK);
    return GRPCUDS_OK;
}

// ---- client (forked child) ---------------------------------------------------

static int run_client(const char* sock) {
    grpcuds_client* c = grpcuds_client_connect_wait(sock, 3000);
    CHECK(c != NULL);

    // Unary through the deferred server path.
    ble_InitRequest ireq = ble_InitRequest_init_zero;
    ble_InitReply irep = ble_InitReply_init_zero;
    CHECK(ble_BleService_Init(c, &ireq, &irep) == GRPCUDS_OK);
    CHECK(irep.ok == true);

    // Sync unary, OK + error status.
    ble_AddScanFilterRequest freq = ble_AddScanFilterRequest_init_zero;
    ble_AddScanFilterReply frep = ble_AddScanFilterReply_init_zero;
    freq.has_filter = true;
    snprintf(freq.filter.mac_prefix, sizeof(freq.filter.mac_prefix), "AA:");
    CHECK(ble_BleService_AddScanFilter(c, &freq, &frep) == GRPCUDS_OK);
    CHECK(frep.filter_id == 7);
    ble_AddScanFilterRequest empty = ble_AddScanFilterRequest_init_zero;
    CHECK(ble_BleService_AddScanFilter(c, &empty, &frep) == GRPCUDS_NOT_FOUND);

    // Server streaming.
    ble_ScanResultStreamRequest sreq = ble_ScanResultStreamRequest_init_zero;
    grpcuds_stream* st = ble_BleService_ScanResultStream_start(c, &sreq);
    CHECK(st != NULL);
    int count = 0;
    ble_ScanResult r;
    int rc;
    while ((rc = ble_BleService_ScanResultStream_next(st, &r)) == 1) {
        CHECK(r.rssi == -40 - count);
        ++count;
    }
    CHECK(rc == 0 && count == 3);
    CHECK(grpcuds_stream_status(st) == GRPCUDS_OK);
    grpcuds_stream_free(st);

    // A NULL table entry was never registered -> UNIMPLEMENTED.
    ble_StopLeScanRequest streq = ble_StopLeScanRequest_init_zero;
    ble_StopLeScanReply strep = ble_StopLeScanReply_init_zero;
    CHECK(ble_BleService_StopLeScan(c, &streq, &strep) == GRPCUDS_UNIMPLEMENTED);

    // Undecodable request bytes (invalid wire type 7) -> the generated
    // trampoline fails the call with INVALID_ARGUMENT, not a crash.
    const uint8_t garbage[] = {0xFF, 0xFF, 0xFF};
    grpcuds_response* bad = grpcuds_client_unary(
        c, "/ble.BleService/AddScanFilter", garbage, sizeof(garbage));
    CHECK(bad != NULL);
    CHECK(grpcuds_response_status(bad) == GRPCUDS_INVALID_ARGUMENT);
    grpcuds_response_free(bad);

    grpcuds_client_free(c);
    printf("ble-cgen-c client: OK\n");
    return 0;
}

// ---- server poll loop (parent) ------------------------------------------------

int main(void) {
    char sock[64];
    snprintf(sock, sizeof(sock), "/tmp/grpcuds-cgen-%d.sock", (int)getpid());
    unlink(sock);

    grpcuds_server* server = grpcuds_server_new();
    CHECK(server != NULL);
    CHECK(grpcuds_server_bind_uds(server, sock) == 0);

    ble_BleService_service svc;
    memset(&svc, 0, sizeof(svc)); // unimplemented RPCs stay NULL
    svc.Init = init_handler;
    svc.AddScanFilter = add_filter_handler;
    svc.ScanResultStream = scan_stream_handler;
    CHECK(ble_BleService_register(server, &svc) == 0);

    pid_t pid = fork();
    if (pid == 0) {
        int rc = run_client(sock);
        _exit(rc);
    }

    grpcuds_conn* conn = NULL;
    int status = 1;
    for (;;) {
        struct pollfd fds[2];
        nfds_t n = 0;
        fds[n].fd = grpcuds_server_listener_fd(server);
        fds[n].events = POLLIN;
        ++n;
        if (conn) {
            fds[n].fd = grpcuds_conn_fd(conn);
            fds[n].events = POLLIN;
            if (grpcuds_conn_wants_write(conn)) fds[n].events |= POLLOUT;
            ++n;
        }
        if (poll(fds, n, 100) < 0) continue;
        if (fds[0].revents & POLLIN) {
            grpcuds_conn* c = grpcuds_server_accept(server);
            if (c) conn = c;
        }
        if (conn && n > 1 && fds[1].revents != 0) {
            if (grpcuds_conn_tick(conn) != 0) {
                grpcuds_conn_free(conn);
                conn = NULL;
            }
        }
        int wstatus;
        if (waitpid(pid, &wstatus, WNOHANG) == pid) {
            status = WIFEXITED(wstatus) ? WEXITSTATUS(wstatus) : 1;
            break;
        }
    }
    if (conn) grpcuds_conn_free(conn);
    grpcuds_server_free(server);
    unlink(sock);
    if (status == 0) printf("ble-cgen-c: OK\n");
    return status;
}
