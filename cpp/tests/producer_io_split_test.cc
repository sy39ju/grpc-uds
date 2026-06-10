// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Thread-separation test (docs/THREADING.md).
//
// This is the *inverse* role split from outbound_thread_test.cc, modeling a
// typical embedded layout:
//
//   * a dedicated "grpc thread" is the I/O thread (Server::RegisterIoThread):
//     it runs the poll loop, accepts connections, and ticks conns — i.e. it
//     is the only thread that handles inbound messages and touches the core.
//   * the main thread is the *producer*: once a server-streaming call is open
//     it calls ServerWriter::Write/Finish directly, with NO application-level
//     mutex around the stream. Off the I/O thread those calls route through
//     the outbound mailbox + wakeup fd; the grpc thread drains them.
//
// So: main writes the stream, the grpc thread handles messages, and the
// service code holds no lock. A real nghttp2 client still observes every
// frame in order + grpc-status 0 + END_STREAM. Run under ThreadSanitizer to
// prove the hand-off is race-free.

#include <grpcudspp/grpcudspp.h>

#include <nghttp2/nghttp2.h>

#include <fcntl.h>
#include <poll.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

#include <atomic>
#include <chrono>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <string>
#include <thread>
#include <vector>

#define CHECK(x)                                                         \
    do {                                                                 \
        if (!(x)) {                                                      \
            std::fprintf(stderr, "CHECK failed: %s (%s:%d)\n", #x,       \
                         __FILE__, __LINE__);                            \
            return 1;                                                    \
        }                                                                \
    } while (0)

namespace {

constexpr int kNumMessages = 5;

// Published by the handler (on the grpc/I/O thread) and consumed by the main
// (producer) thread. The release store of `call` publishes `call_id`; the
// acquire load on main pairs with it. No application mutex around the stream.
std::atomic<void*> g_call{nullptr};
std::atomic<int32_t> g_call_id{0};

// ---- Service: the handler does NOT write. It only opens the stream and
// hands the call to the producer (main), then returns OK without finishing.
class HandoffService : public grpcuds::Service {
 public:
    grpcuds::Status Open(grpcuds::ServerContext* ctx, const uint8_t*, size_t) {
        g_call_id.store(ctx->call_id(), std::memory_order_relaxed);
        g_call.store(ctx->call(), std::memory_order_release);
        return grpcuds::Status::Ok();
    }

    void BindToServer(grpcuds_server* server) override {
        grpcuds_server_register_method(server, "/svc/Open", &Trampoline, this);
    }

 private:
    static int Trampoline(void* call, int32_t call_id, const uint8_t* req,
                          size_t req_len, void* user_data) {
        auto* self = static_cast<HandoffService*>(user_data);
        grpcuds::ServerContext ctx(call, call_id);
        grpcuds::Status s = self->Open(&ctx, req, req_len);
        return static_cast<int>(s.error_code());
    }
};

// ---- Client-side nghttp2 plumbing (mirrors outbound_thread_test.cc) --------

struct ClientState {
    std::vector<uint8_t> status_value;
    std::vector<uint8_t> grpc_status_value;
    std::vector<uint8_t> data;
    bool end_stream_seen = false;
};

int on_header_cb(nghttp2_session*, const nghttp2_frame*, const uint8_t* name,
                 size_t namelen, const uint8_t* value, size_t valuelen,
                 uint8_t, void* user_data) {
    auto* st = static_cast<ClientState*>(user_data);
    if (namelen == 7 && std::memcmp(name, ":status", 7) == 0) {
        st->status_value.assign(value, value + valuelen);
    } else if (namelen == 11 && std::memcmp(name, "grpc-status", 11) == 0) {
        st->grpc_status_value.assign(value, value + valuelen);
    }
    return 0;
}

int on_data_chunk_recv_cb(nghttp2_session*, uint8_t, int32_t,
                          const uint8_t* data, size_t len, void* user_data) {
    auto* st = static_cast<ClientState*>(user_data);
    st->data.insert(st->data.end(), data, data + len);
    return 0;
}

int on_frame_recv_cb(nghttp2_session*, const nghttp2_frame* frame,
                     void* user_data) {
    auto* st = static_cast<ClientState*>(user_data);
    // END_STREAM only counts on response DATA / (trailing) HEADERS; other
    // frame types reuse bit 0x1 (e.g. SETTINGS ACK).
    if ((frame->hd.type == NGHTTP2_DATA || frame->hd.type == NGHTTP2_HEADERS) &&
        (frame->hd.flags & NGHTTP2_FLAG_END_STREAM)) {
        st->end_stream_seen = true;
    }
    return 0;
}

struct ClientReq {
    std::vector<uint8_t> bytes;
    size_t offset = 0;
};

ssize_t data_source_read_cb(nghttp2_session*, int32_t, uint8_t* buf,
                            size_t length, uint32_t* data_flags,
                            nghttp2_data_source* source, void*) {
    auto* req = static_cast<ClientReq*>(source->ptr);
    size_t remaining = req->bytes.size() - req->offset;
    size_t n = remaining < length ? remaining : length;
    if (n > 0) {
        std::memcpy(buf, req->bytes.data() + req->offset, n);
        req->offset += n;
    }
    if (req->offset == req->bytes.size()) {
        *data_flags |= NGHTTP2_DATA_FLAG_EOF;
    }
    return static_cast<ssize_t>(n);
}

nghttp2_nv make_nv(const char* name, const char* value) {
    return nghttp2_nv{
        reinterpret_cast<uint8_t*>(const_cast<char*>(name)),
        reinterpret_cast<uint8_t*>(const_cast<char*>(value)),
        std::strlen(name),
        std::strlen(value),
        NGHTTP2_NV_FLAG_NONE,
    };
}

std::string unique_path() {
    char buf[64];
    std::snprintf(buf, sizeof(buf), "/tmp/grpcudspp-split-%d.sock", getpid());
    return std::string(buf);
}

}  // namespace

int main() {
    const std::string path = unique_path();
    ::unlink(path.c_str());

    grpcuds::ServerBuilder builder;
    builder.AddListeningPort("unix:" + path);
    HandoffService svc;
    builder.RegisterService(&svc);
    auto server = builder.BuildAndStart();
    CHECK(server && server->ListenerFd() > 0);
    const int listener_fd = server->ListenerFd();

    // ---- The dedicated grpc / I/O thread: poll loop, accept, tick, drain.
    // This is the ONLY thread that touches the core. It mirrors the sample
    // server's main.cc poll loop.
    std::atomic<bool> io_stop{false};
    std::thread grpc_thread([&server, &io_stop, listener_fd]() {
        server->RegisterIoThread();
        const int wakeup_fd = server->WakeupFd();
        std::vector<grpcuds_conn*> conns;

        while (!io_stop.load(std::memory_order_acquire)) {
            std::vector<struct pollfd> pfds;
            pfds.push_back({listener_fd, POLLIN, 0});
            for (auto* c : conns) {
                short events = POLLIN;
                if (grpcuds_conn_wants_write(c) == 1) events |= POLLOUT;
                pfds.push_back({grpcuds_conn_fd(c), events, 0});
            }
            const size_t wakeup_idx = pfds.size();
            pfds.push_back({wakeup_fd, POLLIN, 0});

            int rc = ::poll(pfds.data(), pfds.size(), 50);
            if (rc < 0) {
                if (errno == EINTR) continue;
                break;
            }
            if (rc == 0) continue;

            if (pfds[wakeup_idx].revents & POLLIN) server->DrainOutbound();

            const size_t existing = pfds.size() - 2;

            if (pfds[0].revents & POLLIN) {
                while (auto* c = server->Accept()) conns.push_back(c);
            }

            size_t cap = existing;
            for (size_t i = 0; i < cap;) {
                const short revents = pfds[i + 1].revents;
                int t = 0;
                if (revents & (POLLIN | POLLHUP | POLLERR | POLLNVAL)) {
                    t = grpcuds_conn_tick_read(conns[i]);
                } else if (revents & POLLOUT) {
                    t = grpcuds_conn_tick_write(conns[i]);
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

        // Final flush so any late Write/Finish reaches the client before we
        // tear the conns down.
        server->DrainOutbound();
        for (auto* c : conns) grpcuds_conn_tick(c);
        for (auto* c : conns) grpcuds_conn_free(c);
    });

    // ---- Client socket (driven entirely by main) ----
    int client_fd = ::socket(AF_UNIX, SOCK_STREAM, 0);
    CHECK(client_fd > 0);
    sockaddr_un addr{};
    addr.sun_family = AF_UNIX;
    std::strncpy(addr.sun_path, path.c_str(), sizeof(addr.sun_path) - 1);
    CHECK(::connect(client_fd, reinterpret_cast<sockaddr*>(&addr),
                    sizeof(addr)) == 0);
    int flags = ::fcntl(client_fd, F_GETFL, 0);
    ::fcntl(client_fd, F_SETFL, flags | O_NONBLOCK);

    ClientState cli_state;
    nghttp2_session_callbacks* cbs = nullptr;
    CHECK(nghttp2_session_callbacks_new(&cbs) == 0);
    nghttp2_session_callbacks_set_on_header_callback(cbs, on_header_cb);
    nghttp2_session_callbacks_set_on_data_chunk_recv_callback(
        cbs, on_data_chunk_recv_cb);
    nghttp2_session_callbacks_set_on_frame_recv_callback(cbs, on_frame_recv_cb);
    nghttp2_session* client = nullptr;
    CHECK(nghttp2_session_client_new(&client, cbs, &cli_state) == 0);
    nghttp2_session_callbacks_del(cbs);
    CHECK(nghttp2_submit_settings(client, NGHTTP2_FLAG_NONE, nullptr, 0) == 0);

    ClientReq req_src;
    req_src.bytes = {0, 0, 0, 0, 0};  // empty gRPC message: just opens the call
    nghttp2_nv nva[] = {
        make_nv(":method", "POST"),
        make_nv(":scheme", "http"),
        make_nv(":path", "/svc/Open"),
        make_nv(":authority", "localhost"),
        make_nv("te", "trailers"),
        make_nv("content-type", "application/grpc"),
    };
    nghttp2_data_provider provider{};
    provider.source.ptr = &req_src;
    provider.read_callback = data_source_read_cb;
    int32_t sid =
        nghttp2_submit_request(client, nullptr, nva, 6, &provider, nullptr);
    CHECK(sid > 0);

    bool produced = false;
    for (int loop = 0; loop < 4000; ++loop) {
        bool did_work = false;

        // Client → socket
        for (;;) {
            const uint8_t* p = nullptr;
            ssize_t n = nghttp2_session_mem_send(client, &p);
            CHECK(n >= 0);
            if (n == 0 || p == nullptr) break;
            ssize_t written = 0;
            while (written < n) {
                ssize_t w = ::send(client_fd, p + written, n - written, 0);
                if (w < 0) {
                    if (errno == EINTR) continue;
                    if (errno == EAGAIN || errno == EWOULDBLOCK) {
                        std::this_thread::sleep_for(
                            std::chrono::milliseconds(1));
                        continue;
                    }
                    std::perror("send");
                    return 1;
                }
                written += w;
            }
            did_work = true;
        }

        // Producer: once the grpc thread has opened the call, main writes the
        // whole stream itself — no lock. Off the I/O thread these enqueue into
        // the mailbox; the grpc thread drains + flushes them.
        if (!produced) {
            void* call = g_call.load(std::memory_order_acquire);
            if (call != nullptr) {
                int32_t call_id = g_call_id.load(std::memory_order_relaxed);
                grpcuds::RawWriter w(call, call_id);
                for (int i = 0; i < kNumMessages; ++i) {
                    std::string msg = "msg" + std::to_string(i);
                    // RawWriter::Write adds the gRPC length prefix itself, so
                    // hand it the raw payload.
                    w.Write(reinterpret_cast<const uint8_t*>(msg.data()),
                            msg.size());
                }
                w.Finish(grpcuds::Status::Ok());
                produced = true;
                did_work = true;
            }
        }

        // Socket → client
        uint8_t buf[4096];
        for (;;) {
            ssize_t n = ::recv(client_fd, buf, sizeof(buf), 0);
            if (n > 0) {
                ssize_t rc = nghttp2_session_mem_recv(
                    client, buf, static_cast<size_t>(n));
                CHECK(rc >= 0);
                did_work = true;
                continue;
            }
            if (n == 0) break;
            if (errno == EAGAIN || errno == EWOULDBLOCK || errno == EINTR)
                break;
            std::perror("recv");
            return 1;
        }

        if (produced && cli_state.end_stream_seen && !did_work) break;
        if (!did_work) std::this_thread::sleep_for(std::chrono::milliseconds(1));
    }

    io_stop.store(true, std::memory_order_release);
    grpc_thread.join();

    nghttp2_session_del(client);
    ::close(client_fd);

    // ---- Validate: 200 / grpc-status 0 / END_STREAM, and all 5 frames in
    // order.
    auto eq = [](const std::vector<uint8_t>& got, const char* want) {
        const size_t wl = std::strlen(want);
        return got.size() == wl && std::memcmp(got.data(), want, wl) == 0;
    };
    CHECK(eq(cli_state.status_value, "200"));
    CHECK(eq(cli_state.grpc_status_value, "0"));
    CHECK(cli_state.end_stream_seen);
    CHECK(produced);

    // Parse the length-prefixed frames out of the data stream.
    const std::vector<uint8_t>& d = cli_state.data;
    size_t off = 0;
    int got = 0;
    while (off + 5 <= d.size()) {
        uint32_t len = (uint32_t{d[off + 1]} << 24) |
                       (uint32_t{d[off + 2]} << 16) |
                       (uint32_t{d[off + 3]} << 8) | uint32_t{d[off + 4]};
        CHECK(off + 5 + len <= d.size());
        std::string want = "msg" + std::to_string(got);
        CHECK(len == want.size());
        CHECK(std::memcmp(d.data() + off + 5, want.data(), len) == 0);
        off += 5 + len;
        ++got;
    }
    CHECK(got == kNumMessages);
    CHECK(off == d.size());

    std::printf("producer/io-thread split round-trip OK (%d frames)\n", got);
    return 0;
}
