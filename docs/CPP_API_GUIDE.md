# grpcuds C++ API guide

The `grpcudspp` headers (`cpp/include/grpcudspp/`) are a header-only C++ wrapper
over the grpcuds C ABI. The API deliberately mirrors **gRPC C++** so an existing
stock-gRPC server ports with mostly mechanical edits — but there are a handful of
real differences, and they all come from one fact:

> **grpcuds is UDS-only and drives its HTTP/2 session from a single I/O thread.
> You own the event loop; the library never spawns one behind your back (unless
> you opt in with `ServerThread`).**

Keep that sentence in mind and the rest follows. If you are migrating an existing
server, read this alongside [MIGRATING_FROM_GRPC_CPP.md](https://github.com/sy39ju/grpc-uds/blob/main/docs/MIGRATING_FROM_GRPC_CPP.md)
and [THREADING.md](https://github.com/sy39ju/grpc-uds/blob/main/docs/THREADING.md).

One include pulls in everything (mirrors `grpcpp/grpcpp.h`):

```cpp
#include <grpcudspp/grpcudspp.h>
```

---

## 1. The 30-second mental model

| You already know (gRPC C++)        | grpcuds equivalent                                   |
|------------------------------------|------------------------------------------------------|
| `grpc::ServerBuilder`              | `grpcuds::ServerBuilder`                             |
| `AddListeningPort(addr, creds)`    | `AddListeningPort("unix:<path>")` — **UDS only, no creds** |
| `builder.BuildAndStart()`          | `builder.BuildAndStart()` → `std::unique_ptr<Server>` (binds, does **not** spawn a thread) |
| `server->Wait()`                   | **gone** — you drive the loop, or wrap in `grpcuds::ServerThread` |
| `grpc::ServerContext`              | `grpcuds::ServerContext`                             |
| `ctx->IsCancelled()` (poll)        | **gone** — `ctx->SetCancelHook(cb, user_data)` (push) |
| `grpc::ServerWriter<T>`            | `grpcuds::ServerWriter<T>` (Write/Finish are mailbox-safe off-thread) |
| `grpc::Status` / `StatusCode`      | `grpcuds::Status` / `grpcuds::StatusCode`           |
| `ClientContext::set_deadline(...)` | `client.SetTimeout(ms)` — per-client, applies to every following call |
| `gpr_set_log_function` / `GRPC_VERBOSITY` | `grpcuds::SetLogCallback(fn, level)` / `EnableStderrLogging(level)` — silent until registered |
| channel `wait_for_ready` / reconnect | `Client(path, timeout_ms)` retries connect with backoff; a dead connection lazily reconnects on the next call |
| `EnableDefaultHealthCheckService()`| `grpcuds::health::HealthService` — register like any service (`health.h`) |
| message types (protobuf C++)       | **nanopb C structs** (flat names, `_init_zero`)      |

The proto contract and the on-the-wire bytes are identical — a stock gRPC client
cannot tell the difference.

---

## 2. Standing up a server

`BuildAndStart()` binds the socket but returns control to you immediately. There
are two ways to drive it.

### 2a. You own the event loop (epoll / libevent / your reactor)

```cpp
grpcuds::ServerBuilder builder;
builder.AddListeningPort("unix:/tmp/my.sock");
builder.RegisterService(&svc);
std::unique_ptr<grpcuds::Server> server = builder.BuildAndStart();

int lfd = server->ListenerFd();   // add to your poller for read-readiness
// on listener readable:           grpcuds_conn* c = server->Accept();
// on a connection readable:        drive it via the C ABI tick (see THREADING.md)
// each loop iteration / on WakeupFd() readable: server->DrainOutbound();
```

`Server::WakeupFd()` is the fd that goes readable when another thread queued an
outbound `Write`/`Finish` (see §4). Poll it and call `DrainOutbound()` on the I/O
thread to flush.

### 2b. Let the library run the loop — ServerThread (opt-in)

When the main thread should be free and nothing else needs the fd, wrap the
server. `ServerThread` owns the I/O thread; **ownership is the lifecycle** (RAII):

```cpp
auto server = builder.BuildAndStart();
auto io = std::make_unique<grpcuds::ServerThread>(std::move(server)); // runs now
// ... main thread does other work; the I/O loop lives on the bg thread ...
io.reset();   // stops + joins. The destructor does the same — no Shutdown() call.
```

`io->running()` reports state. There is **no `Wait()`**; the thread runs until you
destroy it. Do not call `Stop()`/`reset()` from the I/O thread itself.

`ServerThread`'s constructor takes an optional `on_quiesce` callback that runs on
the I/O thread during teardown *before* connections are freed — the place to join
your own producer threads so a late `Write` can't touch a call being freed.

---

## 3. Implementing a service

The protoc plugin (`protoc-gen-grpcudspp`) emits `MyService::Service` with a pure
virtual per RPC. Derive and override, gRPC-C++ style. Handler signatures:

```cpp
// unary
grpcuds::Status Method(grpcuds::ServerContext* ctx,
                       const Req* request, Resp* response);

// server-streaming
grpcuds::Status Method(grpcuds::ServerContext* ctx,
                       const Req* request, grpcuds::ServerWriter<Resp>* writer);
```

`Req`/`Resp` are **nanopb C structs** (e.g. `ble_ScanResult` for a
`package ble; message ScanResult`), not protobuf C++ objects. Initialize with
the `_init_zero` macro; read fields directly (`request->rssi`); enum values are
flat (`ble_AdapterStateChange_State_ON`). The plugin also generates
chainable `<Msg>Mut` setters so the stock-gRPC `response.set_x(v)` pattern
ports cleanly:

```cpp
Resp response = ble_ScanResult_init_zero;
ble::ScanResultMut(response)
    .set_mac(addr)
    .set_rssi(rssi);
```

### The non-blocking handler rule (the one big difference)

Stock gRPC runs each RPC on its own pool thread, so streaming handlers can block
in `while (!IsCancelled()) { ...Write... }`. **grpcuds has a single I/O thread** —
a blocking handler starves the whole server. So a streaming handler must:

1. Write the first message inline if you have one,
2. hand the `ServerWriter` (it's copyable) to a producer that lives elsewhere
   (your BLE callback, a worker thread, a timer), and
3. **return `Status::Ok()` immediately.**

Further messages are produced off-handler and reach the I/O thread through the
outbound mailbox (§4). Replace `IsCancelled()` polling with a cancel hook:

```cpp
ctx->SetCancelHook(&on_cancel, user_data);  // fires on peer RST_STREAM
```

See [THREADING.md](https://github.com/sy39ju/grpc-uds/blob/main/docs/THREADING.md) for the full pattern and the BLE worked example.

### Deferred unary — long-running jobs (UnaryResponder)

The same rule applies to a unary RPC whose answer takes real time: don't
block in the handler. Every unary method has a second virtual overload that
takes a `grpcuds::UnaryResponder<Resp>` — the grpcuds analogue of grpc++'s
callback-API `ServerUnaryReactor`. Override it INSTEAD of the synchronous
one, hand the responder (copyable, mailbox-safe) to your worker, and return:

```cpp
void Embed(grpcuds::ServerContext*, const ::agent_EmbedRequest* req,
           grpcuds::UnaryResponder<::agent_Embedding> responder) override {
    StartJob(*req, [responder]() mutable {          // any thread, later
        ::agent_Embedding reply = agent_Embedding_init_zero;
        // ... fill reply ...
        responder.Respond(reply);                   // encode + write + finish OK
        // or: responder.Fail(grpcuds::Status(grpcuds::INTERNAL, "..."));
    });
}
```

Budget-aware handlers can read the client's remaining `grpc-timeout`
allowance — `ctx->TimeRemainingMs()` (-1 = no deadline) — and skip work that
cannot finish in time. The default implementation of the deferred overload
calls your synchronous handler and completes inline, so services that never
need it are unchanged.
The responder is single-use (first completion wins); a handler that neither
completes nor stores it leaves the call open — same contract as a streaming
handler that never `Finish()`es. Worked example: the agent service in
`tests/cpp/agent/` (exercised against grpcuds, tonic, and stock grpc++
clients in CI).

---

### Health checking (grpcudspp/health.h)

The standard `grpc.health.v1` service, header-only — stock probers
(`grpc_health_probe`, grpcurl, tonic-health) work against your daemon:

```cpp
grpcuds::health::HealthService health;            // "" starts SERVING
builder.RegisterService(&health);
health.SetStatus("ble.BleScanner", grpcuds::health::SERVING);   // any thread
```

`Check` fails unknown names with `NOT_FOUND`; `Watch` streams the current
status then every change. The example client in `example/ble` shows the
client side (the `Encode/DecodeCheckRequest`/`Response` wire helpers).

## 4. ServerWriter and RawWriter

`ServerWriter<T>::Write(const T&)` (plugin-generated per T: nanopb-encodes, then
forwards) and `Finish(const Status&)` close the server side. Both are
**thread-safe by design**:

- **On the I/O thread:** call the core directly (honoring backpressure's
  true/false return).
- **Off the I/O thread:** the payload is copied into the outbound mailbox, the
  wakeup fd is poked, and the call returns `true`; the actual core write happens
  later on the I/O thread via `Server::DrainOutbound()`.

That is what makes "produce from your BLE callback thread" safe. After `Finish`,
further `Write`/`Finish` calls fail (return `false`) rather than corrupt the
stream.

`RawWriter` is the bytes-in/bytes-out variant for hand-rolled services. Both
`ServerWriter<T>` and `RawWriter` can be constructed from a `ServerContext`, so
you can stash the writer and produce later:

```cpp
grpcuds::ServerWriter<Resp> w(*ctx);   // copyable; outlives the handler
```

### Backpressure

`SetBackpressure()` bounds the per-call outbound queue. Unlike Write/Finish it is
**not** mailbox-routed — call it **only on the I/O thread**, typically inside the
handler before handing the writer to a producer:

```cpp
writer->SetBackpressure(grpcuds::Backpressure::Unbounded());
writer->SetBackpressure(
    grpcuds::Backpressure::Bounded(4, grpcuds::OverflowPolicy::DropOldest));
```

---

## 5. Status and the grpc-message trailer

`grpcuds::Status` mirrors `grpc::Status`:

```cpp
return grpcuds::Status::Ok();
return grpcuds::Status(grpcuds::INVALID_ARGUMENT, "scan mode unspecified");
```

When the message is non-empty it is shipped as a `grpc-message` trailer
(percent-encoded by the runtime) next to the numeric `grpc-status`, so stock gRPC
clients surface it through their usual status-message accessor (C++
`Status::error_message()`, Python `call.details()`, Go `status.Message()`).
`StatusCode` values map one-for-one to `grpc::StatusCode`.

---

## 6. gRPC C++ → grpcuds cheat-sheet

```text
grpc::ServerBuilder                 grpcuds::ServerBuilder
AddListeningPort(a, creds)          AddListeningPort("unix:" + path)   // no creds
BuildAndStart()  // + Wait()        BuildAndStart()  // then drive loop OR ServerThread
grpc::Status::OK                    grpcuds::Status::Ok()
grpc::Status(CODE, msg)             grpcuds::Status(grpcuds::CODE, msg)
grpc::StatusCode::INTERNAL          grpcuds::INTERNAL
ctx->IsCancelled()  (poll loop)     ctx->SetCancelHook(cb, user_data)  (push)
writer->Write(resp)                 writer->Write(resp)                // thread-safe
resp.set_field(v)                   <Msg>Mut(resp).set_field(v)        // nanopb
request->field()                    request->field                    // nanopb struct
EnumName::VALUE                     pkg_EnumName_VALUE                 // nanopb flat
```

---

## 7. Calling a server — the C++ client

`grpcudspp/client.h` is a header-only blocking client (`grpcuds::Client`) that
mirrors the gRPC C++ client shape over UDS. It talks to a grpcuds server **or
any stock gRPC server** listening on a UNIX socket. It needs the C ABI's client
symbols, so build `grpcuds-ffi` with the `client` feature (server-only builds
don't export them).

A hung or slow server need not hang your app: `client.SetTimeout(ms)` arms
a per-call deadline (whole call, gRPC semantics) — on expiry the call fails
with `DEADLINE_EXCEEDED` and the stream is cancelled so the server stops
working on it. `SetTimeout(0)` restores wait-forever.

The generated header gives you a **typed stub** — the stock-gRPC `NewStub`
shape, with method paths + nanopb descriptors baked in:

```cpp
#include "ble.grpc.pb.h"   // protoc-gen-grpcudspp output

grpcuds::Client client("/run/ble.sock");
if (!client) { /* connect failed */ }
auto stub = ble::BleService::NewStub(client);   // client must outlive the stub

// Unary.
ble_InitReply reply = ble_InitReply_init_zero;
grpcuds::Status s = stub->Init(req, &reply);

// Server-streaming: read until Read() returns false, then check status().
auto reader = stub->ScanResultStream(sreq);
ble_ScanResult r = ble_ScanResult_init_zero;
while (reader.Read(&r)) { /* ... */ }
grpcuds::Status final = reader.status();
```

One call is in flight at a time (`Client` is move-only and blocking). Under the
stub sits the generic API — `client.Unary(path, req, req_fields, &reply,
resp_fields)` / `client.ServerStreaming<Req, Resp>(...)` for callers without
generated code, and byte-level `UnaryRaw` / `ServerStreamingRaw` when nanopb
isn't on the include path at all. See `example/ble/client_main.cc` and
`tests/cpp/<domain>/` for runnable clients, including ones that drive a
stock grpc++ server.

**Connection lifecycle** — there is no channel machinery, but the useful
slice of it survives in two pieces:

```cpp
// Startup race: retry with backoff (50ms x1.6 to a 1s cap, +-20% jitter;
// each attempt bounded at 250ms) until the daemon is up or the budget
// runs out. 0 = exactly one attempt.
grpcuds::Client client("/run/svc.sock", /*connect_timeout_ms=*/5000);
```

If the server restarts later, the call that hits the dead connection fails
(that stream cannot be saved), and the **next** call makes one lazy
reconnect attempt to the same path — stock-gRPC IDLE-channel style. The
same `Client` object keeps working across daemon restarts; there is no
reason to recreate it.

## 8. Generating the symbol reference (Doxygen)

This guide is the narrative; for a browsable symbol-level reference of every class
and method, generate the Doxygen HTML (the `Doxyfile` at the repo root uses this
file as its landing page):

```bash
doxygen Doxyfile        # writes docs/doxygen/html/index.html
```

The headers themselves remain the source of truth for exact signatures — they are
short and heavily commented; this guide and the Doxygen output both point back to
them.
