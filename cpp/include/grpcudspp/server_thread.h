// SPDX-License-Identifier: MIT OR Apache-2.0
//
// grpcuds::ServerThread — run a Server's I/O loop on a dedicated thread.
//
// `ServerBuilder::BuildAndStart()` is intentionally caller-owned: it binds
// the socket but does NOT spawn a thread, so you can plug ListenerFd() /
// Accept() into your own event loop. If instead you want the server to "just
// run" in the background while your main thread does other work — never
// touching the gRPC core — wrap it:
//
//     grpcuds::ServerBuilder builder;
//     builder.AddListeningPort("unix:/tmp/foo.sock");
//     builder.RegisterService(&svc);
//     grpcuds::ServerThread server(builder.BuildAndStart());  // runs now
//     // ... main thread is free; the I/O loop lives on the bg thread ...
//     // server.Stop();  // optional; the destructor also stops + joins.
//
// The single-thread constraint of this library is about the I/O loop, not
// about which thread is "main": the nghttp2 session must be driven from one
// thread, and that thread is the one ServerThread owns. Off-I/O-thread
// Write()/Finish() are still safe — they queue into the outbound mailbox and
// are flushed here via DrainOutbound() (see docs/THREADING.md).

#ifndef GRPCUDSPP_SERVER_THREAD_H_
#define GRPCUDSPP_SERVER_THREAD_H_

#include <poll.h>
#include <sys/eventfd.h>
#include <unistd.h>

#include <cerrno>
#include <cstdint>
#include <cstdio>
#include <functional>
#include <memory>
#include <thread>
#include <vector>

#include <grpcuds.h>

#include "grpcudspp/server_builder.h"

namespace grpcuds {

class ServerThread {
 public:
    // Takes ownership of `server` and immediately launches the I/O loop on a
    // background thread. `on_quiesce`, if set, runs on the I/O thread during
    // teardown BEFORE connections are flushed and freed — use it to join your
    // own producer threads so none writes into a call we're about to free
    // (the standalone sample calls svc.JoinWorkers() at this point).
    explicit ServerThread(std::unique_ptr<Server> server,
                          std::function<void()> on_quiesce = {})
        : server_(std::move(server)), on_quiesce_(std::move(on_quiesce)) {
        if (!server_ || !server_->raw()) return;
        stop_fd_ = eventfd(0, EFD_NONBLOCK | EFD_CLOEXEC);
        if (stop_fd_ < 0) return;
        thread_ = std::thread(&ServerThread::Run, this);
    }

    ~ServerThread() {
        Stop();
        if (stop_fd_ >= 0) close(stop_fd_);
    }

    // Non-copyable, non-movable: the running thread captures `this`.
    ServerThread(const ServerThread&) = delete;
    ServerThread& operator=(const ServerThread&) = delete;
    ServerThread(ServerThread&&) = delete;
    ServerThread& operator=(ServerThread&&) = delete;

    // Signal the I/O loop to stop and join it. Idempotent; called by the
    // destructor. Safe to call from any thread other than the I/O thread.
    void Stop() {
        if (!thread_.joinable()) return;
        const uint64_t one = 1;
        // Best-effort wake; EAGAIN only if the counter saturated, which still
        // leaves the fd readable, so the loop wakes regardless.
        ssize_t n = write(stop_fd_, &one, sizeof(one));
        (void)n;
        thread_.join();
    }

    bool running() const { return thread_.joinable(); }

    // Access the underlying Server (e.g. for ListenerFd()/WakeupFd()).
    // The I/O thread owns it — do NOT touch the core from here.
    Server* get() const { return server_.get(); }

 private:
    void Run() {
        // We are now the I/O thread: claim it before serving so off-thread
        // Write()/Finish() take the queue-and-wake path.
        server_->RegisterIoThread();
        const int wakeup_fd = server_->WakeupFd();
        const int listener_fd = server_->ListenerFd();

        std::vector<grpcuds_conn*> conns;

        for (;;) {
            std::vector<struct pollfd> pfds;
            pfds.push_back({listener_fd, POLLIN, 0});
            for (auto* c : conns) {
                short events = POLLIN;
                if (grpcuds_conn_wants_write(c) == 1) events |= POLLOUT;
                pfds.push_back({grpcuds_conn_fd(c), events, 0});
            }
            // wakeup fd then stop fd go LAST so conn index math (conns live at
            // pfds[1 .. existing_count]) is unaffected.
            const size_t wakeup_idx = pfds.size();
            pfds.push_back({wakeup_fd, POLLIN, 0});
            const size_t stop_idx = pfds.size();
            pfds.push_back({stop_fd_, POLLIN, 0});

            // Block until work — but no longer than the earliest pending
            // grpc-timeout deadline, so idle connections still expire.
            int timeout = -1;
            for (auto* c : conns) {
                int64_t d = grpcuds_conn_next_deadline_ms(c);
                if (d >= 0 && (timeout < 0 || d < timeout)) {
                    timeout = static_cast<int>(d > 0 ? d : 1);
                }
            }
            int rc = poll(pfds.data(), pfds.size(), timeout);
            if (rc < 0) {
                if (errno == EINTR) continue;
                std::perror("grpcuds::ServerThread poll");
                break;
            }

            if (pfds[stop_idx].revents & POLLIN) break;  // Stop() requested

            if (pfds[wakeup_idx].revents & POLLIN) server_->DrainOutbound();

            // conns occupy pfds[1 .. existing_count]; exclude listener +
            // wakeup + stop (the two trailing fds). Newly-accepted conns are
            // not in this pfds snapshot and get polled next iteration.
            const size_t existing_count = pfds.size() - 3;

            if (pfds[0].revents & (POLLERR | POLLHUP | POLLNVAL)) {
                std::fprintf(stderr,
                             "grpcuds::ServerThread: listener fd error "
                             "(revents=0x%x), stopping\n",
                             pfds[0].revents);
                break;
            }
            if (pfds[0].revents & POLLIN) {
                while (auto* c = server_->Accept()) {
                    // Mailbox tombstones are cleared/scrubbed automatically by
                    // grpcuds_server_accept / grpcuds_conn_free in the C ABI.
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
                    // A due grpc-timeout deadline ticks like a read: the
                    // expiry sweep runs and the trailers flush.
                    t = grpcuds_conn_tick_read(conns[i]);
                } else {
                    ++i;
                    continue;
                }
                if (t == 1 || t < 0) {
                    if (t < 0) {
                        std::fprintf(
                            stderr,
                            "grpcuds::ServerThread: tick error %d, "
                            "dropping conn\n",
                            t);
                    }
                    grpcuds_conn_free(conns[i]);
                    conns.erase(conns.begin() + static_cast<long>(i));
                    --cap;
                    continue;
                }
                ++i;
            }
        }

        // Teardown on this (I/O) thread: let producers finish, flush the
        // mailbox into the core, push pending bytes, then free conns.
        if (on_quiesce_) on_quiesce_();
        server_->DrainOutbound();
        for (auto* c : conns) grpcuds_conn_tick(c);
        for (auto* c : conns) grpcuds_conn_free(c);
    }

    std::unique_ptr<Server> server_;
    std::function<void()> on_quiesce_;
    int stop_fd_ = -1;
    std::thread thread_;
};

}  // namespace grpcuds

#endif  // GRPCUDSPP_SERVER_THREAD_H_
