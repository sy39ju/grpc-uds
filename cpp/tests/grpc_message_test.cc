// SPDX-License-Identifier: MIT OR Apache-2.0
//
// End-to-end test for the grpc-message status string trailer.
//
// A service handler finishes a call with a non-OK Status carrying a human
// message that contains bytes requiring percent-encoding ('%' and a control
// char). We drive a real nghttp2 client over AF_UNIX and assert that the
// client observes grpc-status = the numeric code AND grpc-message = the
// percent-encoded text, per the gRPC HTTP/2 wire spec.

#include <grpcudspp/grpcudspp.h>

#include <nghttp2/nghttp2.h>

#include <fcntl.h>
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

// Raw byte the handler ships in the grpc-message; '%' and 0x01 must be
// percent-encoded, the ASCII text must pass through verbatim (spaces too).
// Adjacent literals terminate the \x escape: "\x01" "done" is byte 0x01 then
// "done", not the greedy hex escape \x01d == 0x1D.
const char* kRawMessage = "boom 50% \x01" "done";
const char* kEncodedMessage = "boom 50%25 %01done";

class FailService : public grpcuds::Service {
 public:
    grpcuds::Status Fail(grpcuds::ServerContext* ctx) {
        grpcuds::RawWriter w(*ctx);
        w.Finish(grpcuds::Status(grpcuds::INTERNAL, kRawMessage));
        return grpcuds::Status::Ok();
    }

    void BindToServer(grpcuds_server* server) override {
        grpcuds_server_register_method(server, "/svc/Fail", &Trampoline, this);
    }

 private:
    static int Trampoline(void* call, int32_t call_id, const uint8_t*, size_t,
                          void* user_data) {
        auto* self = static_cast<FailService*>(user_data);
        grpcuds::ServerContext ctx(call, call_id);
        grpcuds::Status s = self->Fail(&ctx);
        return static_cast<int>(s.error_code());
    }
};

struct ClientState {
    std::vector<uint8_t> grpc_status_value;
    std::vector<uint8_t> grpc_message_value;
    bool end_stream_seen = false;
};

int on_header_cb(nghttp2_session*, const nghttp2_frame*, const uint8_t* name,
                 size_t namelen, const uint8_t* value, size_t valuelen,
                 uint8_t, void* user_data) {
    auto* st = static_cast<ClientState*>(user_data);
    if (namelen == 11 && std::memcmp(name, "grpc-status", 11) == 0) {
        st->grpc_status_value.assign(value, value + valuelen);
    } else if (namelen == 12 && std::memcmp(name, "grpc-message", 12) == 0) {
        st->grpc_message_value.assign(value, value + valuelen);
    }
    return 0;
}

int on_frame_recv_cb(nghttp2_session*, const nghttp2_frame* frame,
                     void* user_data) {
    auto* st = static_cast<ClientState*>(user_data);
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
    std::snprintf(buf, sizeof(buf), "/tmp/grpcudspp-msg-%d.sock", getpid());
    return std::string(buf);
}

}  // namespace

int main() {
    const std::string path = unique_path();
    ::unlink(path.c_str());

    grpcuds::ServerBuilder builder;
    builder.AddListeningPort("unix:" + path);
    FailService svc;
    builder.RegisterService(&svc);
    auto server = builder.BuildAndStart();
    CHECK(server && server->ListenerFd() > 0);

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
    nghttp2_session_callbacks_set_on_frame_recv_callback(cbs, on_frame_recv_cb);
    nghttp2_session* client = nullptr;
    CHECK(nghttp2_session_client_new(&client, cbs, &cli_state) == 0);
    nghttp2_session_callbacks_del(cbs);
    CHECK(nghttp2_submit_settings(client, NGHTTP2_FLAG_NONE, nullptr, 0) == 0);

    ClientReq req_src;
    req_src.bytes = {0, 0, 0, 0, 0};  // empty gRPC message (5B prefix, len 0)

    nghttp2_nv nva[] = {
        make_nv(":method", "POST"),
        make_nv(":scheme", "http"),
        make_nv(":path", "/svc/Fail"),
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

    for (int loop = 0; loop < 128; ++loop) {
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
                        std::this_thread::sleep_for(std::chrono::milliseconds(1));
                        continue;
                    }
                    perror("send");
                    return 1;
                }
                written += w;
            }
            did_work = true;
        }

        int tick_rc = grpcuds_conn_tick(conn);
        CHECK(tick_rc == 0 || tick_rc == 1);

        uint8_t buf[4096];
        for (;;) {
            ssize_t n = ::recv(client_fd, buf, sizeof(buf), 0);
            if (n > 0) {
                ssize_t rc = nghttp2_session_mem_recv(client, buf,
                                                      static_cast<size_t>(n));
                CHECK(rc >= 0);
                did_work = true;
                continue;
            }
            if (n == 0) break;
            if (errno == EAGAIN || errno == EWOULDBLOCK || errno == EINTR) break;
            perror("recv");
            return 1;
        }

        if (!did_work && cli_state.end_stream_seen) break;
        if (!did_work) {
            std::this_thread::sleep_for(std::chrono::milliseconds(1));
        }
    }

    nghttp2_session_del(client);
    grpcuds_conn_free(conn);
    ::close(client_fd);

    auto check = [](const std::vector<uint8_t>& got, const char* want) {
        const size_t wl = std::strlen(want);
        if (got.size() != wl) return false;
        return std::memcmp(got.data(), want, wl) == 0;
    };
    CHECK(cli_state.end_stream_seen);
    CHECK(check(cli_state.grpc_status_value, "13"));  // INTERNAL
    CHECK(check(cli_state.grpc_message_value, kEncodedMessage));

    std::printf("grpc-message round-trip OK\n");
    return 0;
}
