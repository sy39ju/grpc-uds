// SPDX-License-Identifier: MIT OR Apache-2.0
//
// grpcuds::ServerBuilder + grpcuds::Server.
//
// Mirrors the gRPC C++ shape:
//
//     grpcuds::ServerBuilder builder;
//     builder.AddListeningPort("unix:/tmp/foo.sock");
//     builder.RegisterService(&svc);
//     auto server = builder.BuildAndStart();
//
// Differences from gRPC C++:
//   - There is no `Server::Wait()` because we don't own an event loop.
//     Use `ListenerFd()` / `Accept()` to plug the server into your loop
//     (e.g. epoll / libevent fd-watch registration).
//   - `AddListeningPort` only understands `unix:<path>` URIs — this
//     library is UDS-only.

#ifndef GRPCUDSPP_SERVER_BUILDER_H_
#define GRPCUDSPP_SERVER_BUILDER_H_

#include <memory>
#include <string>
#include <utility>
#include <vector>

#include <grpcuds.h>

#include "grpcudspp/service.h"

namespace grpcuds {

class Server {
 public:
    Server() = default;
    explicit Server(grpcuds_server* impl) : impl_(impl) {}

    ~Server() {
        if (impl_) grpcuds_server_free(impl_);
    }

    // Non-copyable, move-only.
    Server(const Server&) = delete;
    Server& operator=(const Server&) = delete;
    Server(Server&& other) noexcept : impl_(other.impl_) {
        other.impl_ = nullptr;
    }
    Server& operator=(Server&& other) noexcept {
        if (this != &other) {
            if (impl_) grpcuds_server_free(impl_);
            impl_ = other.impl_;
            other.impl_ = nullptr;
        }
        return *this;
    }

    // Raw handle for code that needs the C ABI directly.
    grpcuds_server* raw() const { return impl_; }

    // fd for the AF_UNIX listener — add to your event loop.
    int ListenerFd() const {
        return impl_ ? grpcuds_server_listener_fd(impl_) : -1;
    }

    // Non-blocking accept. Returns the new connection (caller owns and
    // must `grpcuds_conn_free` it), or nullptr if no client is pending.
    grpcuds_conn* Accept() {
        return impl_ ? grpcuds_server_accept(impl_) : nullptr;
    }

    // --- Thread-safe outbound (see docs/THREADING.md) ---
    //
    // Call once, on the thread that runs your poll loop, before serving:
    // it marks that thread as the only one allowed to touch the core.
    // Without it every thread takes the direct path (single-threaded
    // users need no setup).
    void RegisterIoThread() { grpcuds_mailbox_register_io_thread(); }

    // Add this fd to your poll(2)/event-loop set (level-triggered POLLIN).
    // It becomes readable when a Write()/Finish() happened off the I/O
    // thread and there is queued work to flush.
    int WakeupFd() { return grpcuds_mailbox_wakeup_fd(); }

    // Run on the I/O thread when WakeupFd() is readable (or each loop
    // iteration). Flushes queued off-thread Write/Finish into the core.
    void DrainOutbound() { grpcuds_mailbox_drain(); }

 private:
    grpcuds_server* impl_ = nullptr;
};

class ServerBuilder {
 public:
    // `uri` must start with `unix:`. Anything after the colon is the
    // socket path. Multiple ports are not supported.
    ServerBuilder& AddListeningPort(const std::string& uri) {
        uri_ = uri;
        return *this;
    }

    ServerBuilder& RegisterService(Service* svc) {
        services_.push_back(svc);
        return *this;
    }

    // Build the server, bind the socket, and have each registered
    // Service install its method trampolines. Returns nullptr on
    // allocation / bind failure.
    std::unique_ptr<Server> BuildAndStart() {
        if (uri_.empty()) return nullptr;
        const std::string prefix = "unix:";
        if (uri_.compare(0, prefix.size(), prefix) != 0) return nullptr;
        const std::string path = uri_.substr(prefix.size());

        grpcuds_server* impl = grpcuds_server_new();
        if (!impl) return nullptr;
        if (grpcuds_server_bind_uds(impl, path.c_str()) != 0) {
            grpcuds_server_free(impl);
            return nullptr;
        }
        for (Service* svc : services_) {
            svc->BindToServer(impl);
        }
        return std::unique_ptr<Server>(new Server(impl));
    }

 private:
    std::string uri_;
    std::vector<Service*> services_;
};

}  // namespace grpcuds

#endif  // GRPCUDSPP_SERVER_BUILDER_H_
