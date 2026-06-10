// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Mailbox concurrency stress for helgrind (valgrind's race detector).
//
// The thread-safe outbound mailbox lives in the C ABI (grpcuds-ffi-impl): off
// the I/O thread, grpcuds_call_write enqueues under a pthread_mutex instead of
// touching the single-threaded core. This harness hammers that path so
// helgrind can prove it race-free:
//
//   - N producer threads call grpcuds_call_write off the I/O thread (enqueue),
//   - the main (I/O) thread drains concurrently (grpcuds_mailbox_drain),
//   - one connection is freed mid-flight — the tombstone race: its queued and
//     later writes must be dropped, never replayed onto freed memory.
//
// All cross-thread synchronization is the mailbox's pthread_mutex, which
// helgrind models exactly, so a clean run means no data race. No HTTP/2 client
// is needed: a raw AF_UNIX connect makes grpcuds_server_accept() return a live
// Conn, and writes to an inactive stream id return an error the drain ignores —
// the concurrency surface is the point, not a real RPC.

#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

#include <grpcuds.h>

#define PRODUCERS 6
#define PER 5000

static int connect_unix(const char* path) {
    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (fd < 0) return -1;
    struct sockaddr_un addr;
    memset(&addr, 0, sizeof(addr));
    addr.sun_family = AF_UNIX;
    snprintf(addr.sun_path, sizeof(addr.sun_path), "%s", path);
    if (connect(fd, (struct sockaddr*)&addr, sizeof(addr)) != 0) {
        close(fd);
        return -1;
    }
    return fd;
}

static grpcuds_conn* accept_one(grpcuds_server* s) {
    for (int i = 0; i < 1000; ++i) {
        grpcuds_conn* c = grpcuds_server_accept(s);
        if (c) return c;
        usleep(1000);
    }
    return NULL;
}

static void* producer_fn(void* arg) {
    void* call = arg;
    unsigned char buf[24];
    memset(buf, 0xAB, sizeof(buf));
    for (int i = 0; i < PER; ++i) {
        // Stream id 1 has no active stream: off-thread this enqueues, and the
        // drain's core call returns "no stream" (ignored). The mailbox mutex +
        // queue are what get exercised.
        grpcuds_call_write(call, 1, buf, sizeof(buf));
    }
    return NULL;
}

int main(int argc, char** argv) {
    const char* sock = argc > 1 ? argv[1] : "/tmp/grpcuds-helgrind.sock";
    unlink(sock);

    grpcuds_server* server = grpcuds_server_new();
    if (!server || grpcuds_server_bind_uds(server, sock) != 0) {
        fprintf(stderr, "bind failed\n");
        return 1;
    }
    grpcuds_mailbox_register_io_thread();

    // Connection A: the producers' steady target, drained throughout.
    // Connection B: freed mid-flight to drive the tombstone race.
    int fda = connect_unix(sock);
    grpcuds_conn* ca = accept_one(server);
    int fdb = connect_unix(sock);
    grpcuds_conn* cb = accept_one(server);
    if (fda < 0 || fdb < 0 || !ca || !cb) {
        fprintf(stderr, "setup failed\n");
        return 1;
    }
    void* handle_a = grpcuds_conn_call_handle(ca);
    void* handle_b = grpcuds_conn_call_handle(cb);

    pthread_t prod[PRODUCERS];
    for (int i = 0; i < PRODUCERS - 1; ++i) {
        pthread_create(&prod[i], NULL, producer_fn, handle_a);
    }
    // The last producer targets B, which we free underneath it.
    pthread_create(&prod[PRODUCERS - 1], NULL, producer_fn, handle_b);

    // Main = I/O thread: drain concurrently with the producers, freeing B
    // partway through (its queued + future writes must be dropped).
    for (int round = 0; round < 4000; ++round) {
        grpcuds_mailbox_drain();
        if (round == 800 && cb) {
            grpcuds_conn_free(cb);
            cb = NULL;
        }
    }

    for (int i = 0; i < PRODUCERS; ++i) {
        pthread_join(prod[i], NULL);
    }
    grpcuds_mailbox_drain(); // final sweep

    grpcuds_conn_free(ca);
    grpcuds_server_free(server);
    close(fda);
    close(fdb);
    unlink(sock);
    printf("helgrind_mailbox: OK\n");
    return 0;
}
