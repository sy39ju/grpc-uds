// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Thread-safety test for the grpcudspp wrapper (docs/THREADING.md).
//
// Proves the off-I/O-thread outbound path end-to-end:
//
//   * main() is the I/O thread (Server::RegisterIoThread); only it touches
//     the core (Accept / tick / DrainOutbound).
//   * the service handler returns OK immediately (stream stays open) and
//     hands {call, call_id} to a *worker thread*.
//   * the worker calls RawWriter::Write/Finish — which, off the I/O thread,
//     copy into the outbound mailbox + poke the wakeup fd instead of
//     calling grpcuds_call_* directly.
//   * the main poll loop drains the wakeup fd (Server::DrainOutbound),
//     replaying Write/Finish into the core on the I/O thread.
//   * a real nghttp2 client over AF_UNIX still sees the echoed bytes +
//     grpc-status 0 + END_STREAM.
//
// Unlike echo_test.cc this uses a CHECK macro (not assert), so it also
// runs correctly under NDEBUG / Release builds.

#include <grpcudspp/grpcudspp.h>

#include <nghttp2/nghttp2.h>

#include <fcntl.h>
#include <poll.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

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

// ---- Service whose handler writes from a *separate* thread ----------------

class AsyncEchoService : public grpcuds::Service {
 public:
    ~AsyncEchoService() override { Join(); }

    void Join() {
        if (worker_.joinable()) worker_.join();
    }

    grpcuds::Status Echo(grpcuds::ServerContext* ctx, const uint8_t* req,
                         size_t req_len) {
        // The core owns `req` only for the duration of this call — copy it
        // so the worker can use it after we return.
        std::vector<uint8_t> payload(req, req + req_len);
        void* call = ctx->call();
        int32_t call_id = ctx->call_id();
        worker_ = std::thread([call, call_id, payload]() {
            // Simulate an async producer (e.g. a BLE GATT result arriving
            // on the event-loop thread) that fires after the handler returned.
            std::this_thread::sleep_for(std::chrono::milliseconds(10));
            grpcuds::RawWriter w(call, call_id);
            w.Write(payload.data(), payload.size());
            w.Finish(grpcuds::Status::Ok());
        });
        // Return OK without finishing: stream stays open for the worker.
        return grpcuds::Status::Ok();
    }

    void BindToServer(grpcuds_server* server) override {
        grpcuds_server_register_method(server, "/svc/Echo", &Trampoline, this);
    }

 private:
    static int Trampoline(void* call, int32_t call_id, const uint8_t* req,
                          size_t req_len, void* user_data) {
        auto* self = static_cast<AsyncEchoService*>(user_data);
        grpcuds::ServerContext ctx(call, call_id);
        grpcuds::Status s = self->Echo(&ctx, req, req_len);
        return static_cast<int>(s.error_code());
    }

    std::thread worker_;
};

// ---- Client-side nghttp2 plumbing (mirrors echo_test.cc) ------------------

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
    // END_STREAM only counts on response DATA / (trailing) HEADERS. Other
    // frame types reuse bit 0x1 for unrelated flags (e.g. SETTINGS ACK), so
    // masking blindly would trip end_stream on the SETTINGS ACK.
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
    std::snprintf(buf, sizeof(buf), "/tmp/grpcudspp-thr-%d.sock", getpid());
    return std::string(buf);
}

}  // namespace

int main() {
    const std::string path = unique_path();
    ::unlink(path.c_str());

    grpcuds::ServerBuilder builder;
    builder.AddListeningPort("unix:" + path);
    AsyncEchoService svc;
    builder.RegisterService(&svc);
    auto server = builder.BuildAndStart();
    CHECK(server && server->ListenerFd() > 0);

    // main() is the I/O thread. The wakeup fd joins our poll set.
    server->RegisterIoThread();
    const int wakeup_fd = server->WakeupFd();
    CHECK(wakeup_fd >= 0);

    int client_fd = ::socket(AF_UNIX, SOCK_STREAM, 0);
    CHECK(client_fd > 0);
    sockaddr_un addr{};
    addr.sun_family = AF_UNIX;
    std::strncpy(addr.sun_path, path.c_str(), sizeof(addr.sun_path) - 1);
    CHECK(::connect(client_fd, reinterpret_cast<sockaddr*>(&addr),
                    sizeof(addr)) == 0);
    int flags = ::fcntl(client_fd, F_GETFL, 0);
    ::fcntl(client_fd, F_SETFL, flags | O_NONBLOCK);

    grpcuds_conn* conn = nullptr;
    for (int i = 0; i < 100 && conn == nullptr; ++i) {
        conn = server->Accept();
        if (!conn) std::this_thread::sleep_for(std::chrono::milliseconds(1));
    }
    CHECK(conn);

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
    req_src.bytes = {0, 0, 0, 0, 5, 'h', 'e', 'l', 'l', 'o'};
    nghttp2_nv nva[] = {
        make_nv(":method", "POST"),
        make_nv(":scheme", "http"),
        make_nv(":path", "/svc/Echo"),
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

    // Drive the round trip. Generous iteration budget: the worker sleeps
    // ~10ms before producing, and we only make progress once it has
    // enqueued + we've drained.
    for (int loop = 0; loop < 2000; ++loop) {
        bool did_work = false;

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

        // Flush any off-thread Write/Finish into the core before ticking.
        struct pollfd wp{wakeup_fd, POLLIN, 0};
        if (::poll(&wp, 1, 0) > 0 && (wp.revents & POLLIN)) {
            server->DrainOutbound();
            did_work = true;
        }

        int tick_rc = grpcuds_conn_tick(conn);
        CHECK(tick_rc == 0 || tick_rc == 1);

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

        if (cli_state.end_stream_seen && !did_work) break;
        if (!did_work) {
            std::this_thread::sleep_for(std::chrono::milliseconds(1));
        }
    }

    svc.Join();
    nghttp2_session_del(client);
    grpcuds_conn_free(conn);
    ::close(client_fd);

    auto eq = [](const std::vector<uint8_t>& got, const char* want) {
        const size_t wl = std::strlen(want);
        return got.size() == wl && std::memcmp(got.data(), want, wl) == 0;
    };
    CHECK(eq(cli_state.status_value, "200"));
    CHECK(eq(cli_state.grpc_status_value, "0"));
    CHECK(cli_state.end_stream_seen);
    CHECK(cli_state.data.size() >= 10);
    const uint8_t* d = cli_state.data.data();
    uint32_t pl = (uint32_t{d[1]} << 24) | (uint32_t{d[2]} << 16) |
                  (uint32_t{d[3]} << 8) | uint32_t{d[4]};
    CHECK(pl == 5);
    CHECK(std::memcmp(d + 5, "hello", 5) == 0);

    std::printf("off-thread outbound round-trip OK\n");
    return 0;
}
