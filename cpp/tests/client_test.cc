// SPDX-License-Identifier: MIT OR Apache-2.0
//
// End-to-end test for grpcuds::Client (the C++ client wrapper). A byte-level
// echo + streaming service runs on a ServerThread; the blocking C++ Client
// (on the main thread) calls it over a real UDS. Requires grpcuds-ffi built
// with BOTH the `server` and `client` features.

#include <grpcudspp/client.h>
#include <grpcudspp/grpcudspp.h>

#include <unistd.h>

#include <atomic>
#include <cstdio>
#include <cstdlib>
#include <memory>
#include <string>
#include <vector>

#define CHECK(cond)                                                      \
  do {                                                                   \
    if (!(cond)) {                                                       \
      std::fprintf(stderr, "CHECK failed: %s (%s:%d)\n", #cond,          \
                   __FILE__, __LINE__);                                  \
      std::exit(1);                                                      \
    }                                                                    \
  } while (0)

class EchoService : public grpcuds::Service {
 public:
  grpcuds::Status Echo(grpcuds::ServerContext* ctx, const uint8_t* req,
                       size_t req_len) {
    grpcuds::RawWriter w(*ctx);
    w.Write(req, req_len);
    w.Finish(grpcuds::Status::Ok());  // C ABI register_method is generic:
                                      // the handler owns its Finish.
    return grpcuds::Status::Ok();
  }
  grpcuds::Status Stream(grpcuds::ServerContext* ctx, const uint8_t* req,
                         size_t req_len) {
    grpcuds::RawWriter w(*ctx);
    for (uint8_t i = 0; i < 3; ++i) {
      std::vector<uint8_t> m(req, req + req_len);
      m.push_back(i);
      w.Write(m.data(), m.size());
    }
    w.Finish(grpcuds::Status::Ok());
    return grpcuds::Status::Ok();
  }
  void BindToServer(grpcuds_server* server) override {
    grpcuds_server_register_method(server, "/echo.Echo/Unary", &UnaryTr, this);
    grpcuds_server_register_method(server, "/echo.Echo/Stream", &StreamTr, this);
    grpcuds_server_register_method(server, "/echo.Echo/Hang", &HangTr, this);
  }

 private:
  static int UnaryTr(void* call, int32_t call_id, const uint8_t* req,
                     size_t req_len, void* ud) {
    auto* self = static_cast<EchoService*>(ud);
    grpcuds::ServerContext ctx(call, call_id);
    return self->Echo(&ctx, req, req_len).ok() ? 0 : 0;
  }
  static int StreamTr(void* call, int32_t call_id, const uint8_t* req,
                      size_t req_len, void* ud) {
    auto* self = static_cast<EchoService*>(ud);
    grpcuds::ServerContext ctx(call, call_id);
    self->Stream(&ctx, req, req_len);
    return 0;
  }
  // Parks the call forever: OK without Finish keeps the stream open — the
  // shape of a hung/long-running backend, for the SetTimeout test. Also
  // checks the handler-visible deadline budget (grpc-timeout came along
  // with the client's SetTimeout).
  static int HangTr(void* call, int32_t call_id, const uint8_t*, size_t, void*) {
    int64_t left = grpcuds_call_time_remaining_ms(call, call_id);
    CHECK(left > 0 && left <= 200);
    return 0;
  }
};

static std::atomic<int> g_log_events{0};
static std::atomic<int> g_deadline_events{0};

int main() {
  // Log facility: capture-less lambda sink; the flows below must surface
  // events (deadline expiry at least once, at INFO).
  grpcuds::SetLogCallback(
      [](int level, const char* msg, int64_t arg, void*) {
        CHECK(msg != nullptr && msg[0] != '\0');
        (void)arg;
        g_log_events.fetch_add(1);
        if (level == GRPCUDS_LOG_INFO &&
            std::string(msg).find("deadline") != std::string::npos) {
          g_deadline_events.fetch_add(1);
        }
      },
      grpcuds::LOG_DEBUG);

  std::string path =
      "/tmp/grpcudspp-client-test-" + std::to_string(getpid()) + ".sock";
  ::unlink(path.c_str());

  grpcuds::ServerBuilder builder;
  builder.AddListeningPort("unix:" + path);
  EchoService svc;
  builder.RegisterService(&svc);
  auto server = builder.BuildAndStart();
  CHECK(server != nullptr);
  auto server_thread =
      std::make_unique<grpcuds::ServerThread>(std::move(server));

  // The connect-wait constructor rides any startup race (retry + backoff).
  grpcuds::Client client(path, 3000);
  CHECK(static_cast<bool>(client));

  // Unary echo.
  const uint8_t req[] = {'h', 'i'};
  std::vector<uint8_t> out;
  grpcuds::Status s = client.UnaryRaw("/echo.Echo/Unary", req, 2, &out);
  CHECK(s.ok());
  CHECK(out.size() == 2 && out[0] == 'h' && out[1] == 'i');

  // Server streaming: 3 messages "m0", "m1", "m2".
  const uint8_t m[] = {'m'};
  grpcuds_stream* stream = client.ServerStreamingRaw("/echo.Echo/Stream", m, 1);
  CHECK(stream != nullptr);
  int count = 0;
  for (;;) {
    size_t len = 0;
    const uint8_t* msg = grpcuds_stream_next(stream, &len);
    if (!msg) break;
    CHECK(len == 2 && msg[0] == 'm' && msg[1] == static_cast<uint8_t>(count));
    ++count;
  }
  CHECK(grpcuds_stream_status(stream) == 0);
  CHECK(count == 3);
  grpcuds_stream_free(stream);

  std::printf("client_test: OK\n");
  // Client-side deadline: a parked call fails locally with
  // DEADLINE_EXCEEDED instead of blocking forever, and the connection
  // remains usable for the next call once the timeout is cleared.
  client.SetTimeout(200);
  out.clear();
  s = client.UnaryRaw("/echo.Echo/Hang", req, 2, &out);
  CHECK(s.error_code() == grpcuds::DEADLINE_EXCEEDED);

  client.SetTimeout(0);
  out.clear();
  s = client.UnaryRaw("/echo.Echo/Unary", req, 2, &out);
  CHECK(s.ok());
  CHECK(out.size() == 2);

  // Daemon restart: the call that hits the dead connection fails (its
  // stream is gone — and thanks to MSG_NOSIGNAL it is an error, not a
  // SIGPIPE), then the NEXT call rides the lazy reconnect. Same Client
  // object across the restart, no recreation.
  server_thread.reset();  // the daemon dies
  out.clear();
  s = client.UnaryRaw("/echo.Echo/Unary", req, 2, &out);
  CHECK(!s.ok());

  ::unlink(path.c_str());
  grpcuds::ServerBuilder builder2;
  builder2.AddListeningPort("unix:" + path);
  builder2.RegisterService(&svc);
  auto server2 = builder2.BuildAndStart();
  CHECK(server2 != nullptr);
  grpcuds::ServerThread server_thread2(std::move(server2));

  out.clear();
  s = client.UnaryRaw("/echo.Echo/Unary", req, 2, &out);
  CHECK(s.ok());
  CHECK(out.size() == 2 && out[0] == 'h' && out[1] == 'i');
  std::printf("client_test: reconnect OK\n");

  // The log sink heard the session: events flowed, and the deadline
  // scenario above surfaced at least one INFO deadline event.
  CHECK(g_log_events.load() > 0);
  CHECK(g_deadline_events.load() >= 1);
  std::printf("client_test: logging OK (%d events)\n", g_log_events.load());

  return 0;
}
