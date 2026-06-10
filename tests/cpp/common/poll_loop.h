// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The single-threaded poll(2) I/O loop shared by every grpcuds C++ **server**
// example, so each cell's `server_main.cc` is a few lines: build, register,
// run_poll_loop.
//
// Prints `READY` once the listener is bound (the sentinel the Rust cross-
// language harness waits for), then drives the I/O loop until SIGINT/SIGTERM.
#ifndef GRPCUDS_EXAMPLES_POLL_LOOP_H_
#define GRPCUDS_EXAMPLES_POLL_LOOP_H_

#include <grpcudspp/grpcudspp.h>

#include <poll.h>
#include <signal.h>

#include <cerrno>
#include <cstdio>
#include <functional>
#include <memory>
#include <string>
#include <vector>

namespace grpcuds_ex {

inline volatile sig_atomic_t g_shutdown = 0;
inline void on_signal(int) { g_shutdown = 1; }

// Build a server on `unix:<path>`, register `svc`, and run the I/O loop until
// signalled. `join_workers` is called at teardown (join producer threads
// before the connections are freed). Returns a process exit code.
inline int run_poll_loop(const std::string& path, grpcuds::Service* svc,
                         const std::function<void()>& join_workers) {
    struct sigaction sa = {};
    sa.sa_handler = on_signal;
    sigaction(SIGINT, &sa, nullptr);
    sigaction(SIGTERM, &sa, nullptr);

    grpcuds::ServerBuilder builder;
    builder.AddListeningPort("unix:" + path);
    builder.RegisterService(svc);
    auto server = builder.BuildAndStart();
    if (!server) {
        std::fprintf(stderr, "server: BuildAndStart failed\n");
        return 1;
    }
    server->RegisterIoThread();
    const int wakeup_fd = server->WakeupFd();
    const int listener_fd = server->ListenerFd();

    // Sentinel consumed by the Rust cross-language harness.
    std::printf("READY\n");
    std::fflush(stdout);

    std::vector<grpcuds_conn*> conns;
    while (!g_shutdown) {
        std::vector<struct pollfd> pfds;
        pfds.push_back({listener_fd, POLLIN, 0});
        for (auto* c : conns) {
            short events = POLLIN;
            if (grpcuds_conn_wants_write(c) == 1) events |= POLLOUT;
            pfds.push_back({grpcuds_conn_fd(c), events, 0});
        }
        const size_t wakeup_idx = pfds.size();
        pfds.push_back({wakeup_fd, POLLIN, 0});

        int rc = poll(pfds.data(), pfds.size(), 100);
        if (rc < 0) {
            if (errno == EINTR) continue;
            std::perror("poll");
            break;
        }
        // rc == 0 falls through: a due grpc-timeout deadline needs a tick
        // even with no I/O (the 100 ms cap bounds expiry latency).

        if (pfds[wakeup_idx].revents & POLLIN) server->DrainOutbound();
        const size_t existing_count = pfds.size() - 2;

        if (pfds[0].revents & (POLLERR | POLLHUP | POLLNVAL)) {
            std::fprintf(stderr, "server: listener fd error (0x%x)\n", pfds[0].revents);
            break;
        }
        if (pfds[0].revents & POLLIN) {
            while (auto* c = server->Accept()) {
                // Tombstones are handled automatically by the C ABI
                // (grpcuds_server_accept / grpcuds_conn_free).
                conns.push_back(c);
            }
        }

        size_t cap = existing_count;
        for (size_t i = 0; i < cap;) {
            const short revents = pfds[i + 1].revents;
            int t = 0;
            if (revents & (POLLIN | POLLHUP | POLLERR | POLLNVAL)) {
                t = grpcuds_conn_tick_read(conns[i]);
            } else if (revents & POLLOUT) {
                t = grpcuds_conn_tick_write(conns[i]);
            } else if (grpcuds_conn_next_deadline_ms(conns[i]) == 0) {
                // A due grpc-timeout deadline ticks like a read: the expiry
                // sweep runs and the trailers flush.
                t = grpcuds_conn_tick_read(conns[i]);
            } else {
                ++i;
                continue;
            }
            if (t != 0) {
                grpcuds_conn_free(conns[i]);
                conns.erase(conns.begin() + static_cast<long>(i));
                --cap;
                continue;
            }
            ++i;
        }
    }

    join_workers();
    server->DrainOutbound();
    for (auto* c : conns) grpcuds_conn_tick(c);
    for (auto* c : conns) grpcuds_conn_free(c);
    return 0;
}

}  // namespace grpcuds_ex

#endif  // GRPCUDS_EXAMPLES_POLL_LOOP_H_
