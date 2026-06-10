# Migrating from gRPC C++ to grpcuds

This is the practical guide for porting an existing gRPC C++ server to
grpcuds. We assume you already have:

- A `.proto` file with one or more `service`s.
- A `grpc::Service` subclass that implements them.
- A main that wires everything via `grpc::ServerBuilder` + `Server::Wait()`.

The grpcuds C++ API (`grpcudspp/`) was deliberately shaped to look like
`grpcpp/` so the porting work is mostly mechanical. Three architectural
differences require real decisions; we cover them first, then walk
through a BLE service end-to-end.

---

## TL;DR mapping

| gRPC C++                                    | grpcuds equivalent                          |
| ------------------------------------------- | ------------------------------------------- |
| `<grpcpp/grpcpp.h>`                         | `<grpcudspp/grpcudspp.h>`                   |
| `grpc::ServerBuilder`                       | `grpcuds::ServerBuilder`                    |
| `grpc::Server`                              | `grpcuds::Server`                           |
| `grpc::Service`                             | `grpcuds::Service`                          |
| `grpc::ServerContext`                       | `grpcuds::ServerContext`                    |
| `grpc::ServerWriter<T>`                     | `grpcuds::ServerWriter<T>`                  |
| `grpc::Status` / `StatusCode`               | `grpcuds::Status` / `grpcuds::StatusCode`   |
| `protoc-gen-grpc-cpp`                       | `protoc-gen-grpcudspp`                      |
| `protoc-gen-cpp` (protobuf-full messages)   | `nanopb_generator` (flat C structs)         |
| `MyService::Service` base                   | `MyService::Service` base (same name)       |
| `MyService::NewStub(channel)`               | `MyService::NewStub(client)` — same typed stub shape |
| callback API `ServerUnaryReactor` (deferred unary) | `grpcuds::UnaryResponder<T>` overload — complete from any thread |
| `ClientContext::set_deadline(...)`          | `client.SetTimeout(ms)` — per-client default deadline |
| `grpc::CreateChannel` + `wait_for_ready`    | `grpcuds::Client(path, timeout_ms)` — connect-retry w/ backoff; lazy reconnect on the next call after a restart |
| `gpr_set_log_function` / `GRPC_VERBOSITY`   | `grpcuds::SetLogCallback` / `EnableStderrLogging` (static msg + one numeric arg; no formatting in the library) |
| `EnableDefaultHealthCheckService()`         | `grpcuds::health::HealthService` (header-only `grpc.health.v1`) |
| `mypkg::MyRequest` (C++ class)              | `::mypkg_MyRequest` (nanopb C struct)       |
| `request->mac()` / `set_mac(...)`           | `request->mac` / direct field write         |
| `Server::Wait()`                            | — caller owns the event loop                |
| `ServerContext::IsCancelled()` poll         | `ServerContext::SetCancelHook(cb, ud)`      |
| TLS via `SslServerCredentials`              | — UDS-only, fs-permission auth              |

---

## Three architectural differences

### 1. Transport — UDS only, no TLS

```cpp
// gRPC C++:
auto creds = grpc::SslServerCredentials(opts);
builder.AddListeningPort("0.0.0.0:50051", creds);

// grpcuds:
builder.AddListeningPort("unix:/run/myservice.sock");
// Access control via the filesystem:
//   chmod 0660 /run/myservice.sock
//   chown myuser:mygroup /run/myservice.sock
```

`AddListeningPort` only accepts URIs of the form `unix:<path>`. Anything
else makes `BuildAndStart()` return `nullptr`. This is intentional —
grpcuds was designed for local IPC on a single host and explicitly punts TLS.

### 2. Event loop — caller-owned, single-threaded

gRPC C++ spins worker threads internally and `server->Wait()` blocks the
calling thread. grpcuds does neither. The integration model expects you to
plug the listener fd + per-connection fds into your existing event loop
(epoll, libevent, …). The sketch below uses raw `epoll`; the shape is
the same for any fd-watch API.

```cpp
// gRPC C++:
auto server = builder.BuildAndStart();
server->Wait();   // blocks the thread; gRPC manages everything

// grpcuds (sketch for an epoll loop):
auto server = builder.BuildAndStart();

int ep = epoll_create1(0);
watch(ep, server->ListenerFd());          // watch the listener for reads

for (;;) {
    epoll_event events[64];
    int n = epoll_wait(ep, events, 64, -1);
    for (int i = 0; i < n; ++i) {
        int fd = events[i].data.fd;
        if (fd == server->ListenerFd()) {
            while (auto* c = server->Accept()) {
                watch(ep, grpcuds_conn_fd(c));   // watch each new conn
            }
        } else {
            auto* c = conn_for(fd);
            int rc = grpcuds_conn_tick(c);
            if (rc != 0) {                 // 1 = Closed, <0 = -errno
                unwatch(ep, fd);
                grpcuds_conn_free(c);
            }
        }
    }
}
```

(`watch` / `unwatch` / `conn_for` stand in for your loop's fd registration
and fd→connection bookkeeping.)

For a self-contained binary (no host event loop), use a `poll(2)` loop —
see `tests/cpp/common/poll_loop.h` for a working reference (~60 lines).

### 3. Messages — nanopb C structs, not protobuf-full classes

gRPC C++ uses generated C++ classes with getters/setters. grpcuds uses
nanopb's flat C structs. The protoc plugin (`protoc-gen-grpcudspp`)
emits *service* stubs that take nanopb message pointers.

```cpp
// gRPC C++ message access:
const std::string& mac = req->mac();
reply->set_ok(true);
auto* item = reply->add_results();  // repeated field

// grpcuds (nanopb) access:
const char* mac = req->mac;          // fixed-size char[18]
reply->ok = true;
// repeated → see "Variable-size fields" below
```

#### Variable-size fields

By default nanopb represents strings / bytes / repeated fields via
callbacks, which is awkward for handlers. Pin them to fixed-size arrays
with a `.options` file alongside the `.proto`:

```
# ble.options
ble.ScanResult.mac          max_size:18
ble.ScanResult.adv_data     max_size:64
ble.ScanFilter.mac_prefix   max_size:18
```

This generates:
- `char mac[18]` for strings.
- A `{ pb_size_t size; pb_byte_t bytes[N]; }` struct for `bytes` and
  fixed-size `repeated` fields.

Pick `max_size` to fit the largest legitimate payload — going over at
runtime causes `pb_encode` to fail and the handler to return INTERNAL.

There is a second hard cap on top of nanopb's `max_size`: each generated
`*.grpc.pb.cc` trampoline allocates an on-stack scratch buffer of
`GRPCUDSPP_MAX_MESSAGE_SIZE` bytes for the encode / decode step. The
default is **1024 bytes** (the comment above the `#ifndef` in the
generated source spells this out), tuned for the typical BLE control /
event payload. If your `max_size`-pinned message itself fits in 1 KB
you're done; if any reply or request can grow past that — a GATT blob
write, a 4 KB advertising-data dump — raise the cap at compile time for
every translation unit that includes a generated `*.grpc.pb.cc`:

```cmake
# CMake
target_compile_definitions(my-app PRIVATE GRPCUDSPP_MAX_MESSAGE_SIZE=4096)
```

```sh
# bare cc
cc -DGRPCUDSPP_MAX_MESSAGE_SIZE=4096 ...
```

The override must be supplied to *every* TU that includes a generated
`*.grpc.pb.cc`. Mixing different values across trampolines silently
diverges the buffer sizes and the symptom looks like a sporadic
`pb_encode` INTERNAL on the larger-message methods.

#### Delegation boundary: what the plugin does *not* touch

`protoc-gen-grpcudspp` only generates **service** scaffolding — the base
class, the virtual method signatures, the per-RPC trampolines, and the
`ServerWriter<T>` specializations. It never reasons about message shape.
Everything about how a field is laid out, encoded, or decoded is
`nanopb`'s job, reached through the generated `<msg>_fields` descriptor
and `pb_encode` / `pb_decode`. Concretely:

| proto feature                     | who handles it | how                                     |
| --------------------------------- | -------------- | --------------------------------------- |
| `oneof`                           | nanopb         | union + `which_<name>` tag in the struct |
| nested messages (`Outer.Inner`)   | nanopb         | mangled to flat `pkg_Outer_Inner` symbols |
| `proto2` field presence / defaults | nanopb        | `has_<field>` bools, `syntax` directive in `.options` |
| repeated / string / bytes sizing  | nanopb         | `max_size` / `max_count` in `.options`  |

Because of this split the plugin is **syntax-neutral**: it reads the
service definitions out of the `FileDescriptorProto` and ignores
`file.syntax()` entirely, so a `proto2` and a `proto3` file with the same
services produce byte-identical stubs. A nested request type like
`.ble.Container.Request` becomes `::ble_Container_Request` purely by the
flat-mangling rule (`replace('.', "_")` + `::` global-scope prefix) —
the plugin does not need to know it was nested; it just has to spell the
symbol the way nanopb already spelled it.

Practical consequence: if a message feature misbehaves (a `oneof` that
won't round-trip, a `proto2` default that doesn't apply, a nested type
that won't compile), the fix lives in the `.proto` / `.options` /
`nanopb_generator` invocation — **not** in the grpc stubs. The only
message-adjacent knob the plugin owns is the
`GRPCUDSPP_MAX_MESSAGE_SIZE` scratch buffer documented above. Regression
coverage for the nested-mangling and proto2-neutrality guarantees lives
in `rust/protoc-gen-grpcudspp/src/main.rs`
(`nested_message_types_mangle_to_flat_nanopb_symbols`,
`proto2_services_generate_identically`).

---

## Step-by-step port of a BLE service

We'll walk through porting a hypothetical `ble.proto` server, showing a
diff at each step. The full working version lives in
`tests/cpp/` (binary) and `tests/cpp/proto/ble.proto` (schema).

### Step 0: Decide what doesn't migrate

| gRPC C++ feature                  | grpcuds | note                                |
| --------------------------------- | ------- | ----------------------------------- |
| TLS / ALTS                        | —       | UDS, filesystem perms               |
| Compression                       | —       | Refused on the wire (`grpc-encoding: identity` only) |
| Reflection RPC                    | —       | No `grpc.reflection.v1alpha`        |
| Client streaming                  | —       | Plugin rejects these at codegen     |
| Bidirectional streaming           | —       | Same                                |
| `Server::Wait()`                  | —       | Caller event loop                   |
| `IsCancelled()` polling           | `SetCancelHook`-style              |
| `grpc-message` in trailer         | `Status(code, msg)`               | Message shipped as the percent-encoded `grpc-message` trailer |

Strip any of these from your `.proto` / impl before porting.

### Step 1: Replace includes and namespace

```diff
- #include <grpcpp/grpcpp.h>
- #include "ble.grpc.pb.h"          // gRPC C++ generated
- #include "ble.pb.h"               // protoc-gen-cpp generated
+ #include <grpcudspp/grpcudspp.h>
+ #include "ble.grpc.pb.h"          // protoc-gen-grpcudspp generated
+ #include "ble.pb.h"               // nanopb_generator generated

- using grpc::Server;
- using grpc::ServerBuilder;
- using grpc::ServerContext;
- using grpc::ServerWriter;
- using grpc::Status;
+ using grpcuds::Server;
+ using grpcuds::ServerBuilder;
+ using grpcuds::ServerContext;
+ using grpcuds::ServerWriter;
+ using grpcuds::Status;
```

That's it for the boilerplate. The class names line up 1:1.

### Step 2: Regenerate stubs

```sh
# gRPC C++ (before):
protoc --grpc_out=./out --plugin=protoc-gen-grpc=$(which grpc_cpp_plugin) \
       --cpp_out=./out ble.proto

# grpcuds (after):
nanopb_generator -D ./out -I . -f ble.options ble.proto
protoc --plugin=protoc-gen-grpcudspp=$(which protoc-gen-grpcudspp) \
       --grpcudspp_out=./out --proto_path=. ble.proto
```

Output:
- `ble.pb.h` / `ble.pb.c` — nanopb structs + `_fields` descriptors.
- `ble.grpc.pb.h` / `ble.grpc.pb.cc` — service base class + trampolines.

### Step 3: Adjust the service implementation

The class shape is unchanged — `BleService::Service` is still the base
class, virtual methods still match each RPC. Only message references
change.

```diff
- class BleServiceImpl final : public ble::BleService::Service {
+ class BleServiceImpl final : public ble::BleService::Service {
   public:
-     Status Init(ServerContext* ctx,
-                 const ble::InitRequest* req,
-                 ble::InitReply* reply) override {
-         reply->set_ok(true);
-         return Status::OK;
-     }
+     Status Init(ServerContext* ctx,
+                 const ::ble_InitRequest* req,
+                 ::ble_InitReply* reply) override {
+         reply->ok = true;
+         return Status::Ok();
+     }
  };
```

Note:
- `ble::InitRequest` (C++ class) → `::ble_InitRequest` (flat nanopb C struct).
  The plugin emits `::` prefix to force global-scope lookup so the same
  source compiles regardless of which `namespace` it sits in.
- `set_ok(true)` → direct field assignment `reply->ok = true`.
- `Status::OK` → `Status::Ok()` (factory method, our equivalent).

For unary handlers that's the entire diff.

### Step 4: Streaming handlers change shape

This is the biggest semantic change. gRPC C++ streaming handlers BLOCK
the calling thread until the stream is done. grpcuds streaming handlers
RETURN IMMEDIATELY after setting up an async producer (BLE scan
callback, GATT subscription, etc.). The producer pushes via
`writer->Write` from its own callback path.

Before (gRPC C++ blocking pattern):

```cpp
Status StartLeScan(ServerContext* ctx,
                   const ble::StartLeScanRequest* req,
                   ServerWriter<ble::ScanResult>* writer) override {
    int scan_handle;
    bt_adapter_le_start_scan(&scan_handle, /* sync result delivery */);
    while (!ctx->IsCancelled()) {
        ble::ScanResult r = next_result();   // blocks waiting on condvar
        if (!writer->Write(r)) break;        // false → client cancelled
    }
    bt_adapter_le_stop_scan(scan_handle);
    return Status::OK;
}
```

After (grpcuds async producer pattern):

```cpp
struct scan_state {
    grpcuds::ServerWriter<::ble_ScanResult>* writer;
    int scan_handle;
};

static void on_scan_result(uint8_t* mac, int rssi, void* ud) {
    auto* s = static_cast<scan_state*>(ud);
    ::ble_ScanResult r = ble_ScanResult_init_zero;
    format_mac(r.mac, sizeof(r.mac), mac);
    r.rssi = rssi;
    s->writer->Write(r);   // returns false if queue is rejecting
}

static void scan_cleanup(void* ud) {
    auto* s = static_cast<scan_state*>(ud);
    bt_adapter_le_stop_scan(s->scan_handle);
    delete s;
}

Status StartLeScan(ServerContext* ctx,
                   const ::ble_StartLeScanRequest* req,
                   grpcuds::ServerWriter<::ble_ScanResult>* writer) override {
    // Drop old scan results if the client is slow — freshness > completeness.
    writer->SetBackpressure(
        grpcuds::Backpressure::Bounded(4, grpcuds::OverflowPolicy::DropOldest));

    auto* s = new scan_state{writer, 0};
    bt_adapter_le_start_scan(&s->scan_handle, on_scan_result, s);

    // Register cleanup BEFORE returning so a client cancel mid-stream
    // tears down the BLE scan even if no Write() has happened yet.
    ctx->SetCancelHook(scan_cleanup, s);

    // Return OK and let the BLE callbacks drive the stream. The trailer
    // will be sent when YOU call writer->Finish(...) — either from the
    // BLE source (success path) or from scan_cleanup (cancel path).
    return Status::Ok();
}
```

Key behaviour differences:

- **`IsCancelled()` polling → `SetCancelHook` callback**. Your async
  producer can't poll for cancellation between writes, so we deliver
  cancel as a one-shot callback. The hook fires on RST_STREAM but NOT
  on graceful close — see [Cancel hook lifetime](#cancel-hook-lifetime).
- **`Write` return**. `writer->Write` returns `bool`. With the default
  `Reject` overflow policy it returns `false` when the queue is full
  (the producer can buffer / log / drop). With `DropOldest` it returns
  `true` even when an older message was evicted.
- **No implicit `Finish` at handler return**. The handler returning OK
  leaves the stream open. The grpc-status trailer is shipped only when
  someone calls `writer->Finish(status)`. Auto-finish on the cancel
  path happens via the trailer that nghttp2 sends in response to the
  peer's RST_STREAM, so the cleanup hook doesn't need to call
  `Finish` itself.

### Step 5: Migrate the bootstrap (the `main`)

```diff
  int main(int argc, char** argv) {
-     std::string addr("0.0.0.0:50051");
+     const char* path = argc > 1 ? argv[1] : "/run/ble.sock";

      ServerBuilder builder;
-     builder.AddListeningPort(addr, grpc::SslServerCredentials(opts));
+     builder.AddListeningPort(std::string("unix:") + path);

      BleServiceImpl service;
      builder.RegisterService(&service);

      auto server = builder.BuildAndStart();
-     server->Wait();      // blocks
+     // Plug server->ListenerFd() and per-conn fds into your event loop here.
+     // See tests/cpp/common/poll_loop.h for a poll(2) loop reference.
+     run_event_loop(server.get());
      return 0;
  }
```

### Step 6: Build system

```cmake
# Before (gRPC C++):
find_package(gRPC CONFIG REQUIRED)
find_package(Protobuf REQUIRED)
add_executable(ble_server main.cc ble_service_impl.cc
                          ble.pb.cc ble.grpc.pb.cc)
target_link_libraries(ble_server PRIVATE gRPC::grpc++ protobuf::libprotobuf)

# After (grpcuds):
find_library(NGHTTP2_LIB NAMES nghttp2 REQUIRED)
set(NANOPB_DIR /path/to/nanopb)              # pb_*.c / pb*.h sources
set(GRPCUDS_FFI /path/to/libgrpcuds_ffi.a)
set(GRPCUDSPP_INCLUDE /path/to/grpcudspp/include)
set(GRPCUDS_INCLUDE /path/to/grpcuds-ffi/include)
set(GRPCUDSPP_PLUGIN /path/to/protoc-gen-grpcudspp)

set(GEN ${CMAKE_CURRENT_BINARY_DIR}/generated)
file(MAKE_DIRECTORY ${GEN})

add_custom_command(OUTPUT ${GEN}/ble.pb.h ${GEN}/ble.pb.c
    COMMAND ${NANOPB_GENERATOR} -D ${GEN} -I ${PROTO_DIR}
                                -f ${PROTO_DIR}/ble.options
                                ${PROTO_DIR}/ble.proto
    DEPENDS ${PROTO_DIR}/ble.proto ${PROTO_DIR}/ble.options)
add_custom_command(OUTPUT ${GEN}/ble.grpc.pb.h ${GEN}/ble.grpc.pb.cc
    COMMAND protoc --plugin=protoc-gen-grpcudspp=${GRPCUDSPP_PLUGIN}
                   --grpcudspp_out=${GEN} --proto_path=${PROTO_DIR}
                   ${PROTO_DIR}/ble.proto
    DEPENDS ${PROTO_DIR}/ble.proto ${GRPCUDSPP_PLUGIN})

add_executable(ble_server
    main.cc ble_service_impl.cc
    ${GEN}/ble.grpc.pb.cc ${GEN}/ble.pb.c
    ${NANOPB_DIR}/pb_common.c ${NANOPB_DIR}/pb_decode.c
    ${NANOPB_DIR}/pb_encode.c)
target_include_directories(ble_server PRIVATE
    ${GEN} ${NANOPB_DIR} ${GRPCUDSPP_INCLUDE} ${GRPCUDS_INCLUDE})
target_link_libraries(ble_server PRIVATE
    ${GRPCUDS_FFI} ${NGHTTP2_LIB} pthread dl m)
```

The full working example is in `tests/cpp/cmake/grpcuds_codegen.cmake`.

---

## Status codes

`grpcuds::StatusCode` mirrors `grpc::StatusCode` value-for-value:

```cpp
grpcuds::OK                   = 0   = grpc::OK
grpcuds::CANCELLED            = 1   = grpc::CANCELLED
grpcuds::UNKNOWN              = 2   = grpc::UNKNOWN
grpcuds::INVALID_ARGUMENT     = 3
grpcuds::DEADLINE_EXCEEDED    = 4
grpcuds::NOT_FOUND            = 5
grpcuds::ALREADY_EXISTS       = 6
grpcuds::PERMISSION_DENIED    = 7
grpcuds::RESOURCE_EXHAUSTED   = 8
grpcuds::FAILED_PRECONDITION  = 9
grpcuds::ABORTED              = 10
grpcuds::OUT_OF_RANGE         = 11
grpcuds::UNIMPLEMENTED        = 12
grpcuds::INTERNAL             = 13
grpcuds::UNAVAILABLE          = 14
grpcuds::DATA_LOSS            = 15
grpcuds::UNAUTHENTICATED      = 16
```

`Status` constructors:

```cpp
// gRPC C++:                     // grpcuds:
grpc::Status::OK               → grpcuds::Status::Ok()
grpc::Status(code, "msg")      → grpcuds::Status(code, "msg")
                                  (ships percent-encoded as grpc-message)
```

---

## Cancel hook lifetime

The cancel hook is the chief lifetime gotcha. Read carefully.

The hook fires **at most once** when the call closes with a non-zero
error code (peer RST_STREAM, protocol error, session shutdown). It
**never** fires on graceful close (server wrote `grpc-status:0`).

The `user_data` pointer you pass MUST stay valid until either:

1. The callback fires (cancel path), OR
2. The call closes gracefully AND the connection is freed (the hook
   never fires; nobody touches `user_data` again — your "finish" path
   is responsible for freeing whatever you allocated).

Safe pattern — heap-allocate, free in the callback, and also free from
the "graceful finish" path if it exists:

```cpp
struct call_state { /* per-call resources */ };

static void on_cancel(void* ud) {
    auto* s = static_cast<call_state*>(ud);
    release_resources(s);
    delete s;
}

Status SomeStream(...) override {
    auto* s = new call_state{};
    acquire_resources(s);
    ctx->SetCancelHook(on_cancel, s);
    start_async_producer(writer, s);
    return Status::Ok();
}

// When the producer reaches end-of-data (graceful path):
void on_producer_done(call_state* s) {
    s->writer->Finish(Status::Ok());
    release_resources(s);
    delete s;     // the cancel hook will not fire after a clean close
}
```

What does NOT work:

```cpp
// BAD — `s` lives on the stack; the handler returns and `s` is destroyed
// long before the hook can fire.
Status SomeStream(...) override {
    call_state s{};
    ctx->SetCancelHook(on_cancel, &s);   // dangling user_data!
    start_async_producer(writer, &s);
    return Status::Ok();
}
```

---

## Backpressure policy quick reference

Streaming RPCs whose source can outpace the wire need a backpressure
policy. grpcuds offers two:

| Policy        | Behavior                          | Use case                           |
| ------------- | --------------------------------- | ---------------------------------- |
| `Reject`      | Full queue → `Write` returns `false`. | GATT notifications: lossless required. Producer logs / buffers / surfaces the loss. |
| `DropOldest`  | Full queue → evict oldest unstarted. `Write` always returns `true`. | BLE scan: latest N wins. Old messages just disappear. |

`grpcuds::Backpressure` is a sum type with two factories so the
"capacity zero but I picked a policy" misuse isn't representable:

```cpp
namespace grpcuds {
    class Backpressure {
     public:
        static Backpressure Unbounded();
        static Backpressure Bounded(size_t capacity, OverflowPolicy policy);
        // ... immutable accessors ...
    };
}
```

Set either inside the handler (`writer->SetBackpressure(...)`) or, more
durably, at method registration time. Currently the registration-time
form is Rust-only on the server; the C++ surface uses the per-call form:

```cpp
Status ScanResultStream(...) override {
    writer->SetBackpressure(
        grpcuds::Backpressure::Bounded(4, grpcuds::OverflowPolicy::DropOldest));
    // ... start async producer ...
    return Status::Ok();
}

// Reset to unbounded later (rarely needed):
//   writer->SetBackpressure(grpcuds::Backpressure::Unbounded());
```

`Unbounded` is the default for a freshly-dispatched call (if you never
call `SetBackpressure`). The `DropOldest` policy never evicts the
in-flight head message (one that has already started shipping to the
wire).

---

## FAQ

### Q: Do I need to keep my existing `.proto`?

Yes — mostly unchanged. You'll add a sibling `.options` file for nanopb
size pinning, and remove any `stream` keywords on the *request* side
(client-streaming and bidi RPCs aren't supported; the plugin rejects
them at code-generation time).

### Q: Can grpcuds run alongside gRPC C++ in the same process?

Yes. They're separate libraries with separate symbol namespaces. You
can migrate service by service.

### Q: How do I test against my existing gRPC clients?

grpcuds speaks the standard gRPC-over-HTTP/2 wire (just on a UDS
transport instead of TCP). Any gRPC client that can connect to a
UNIX-domain socket works. `tests/rust/` has a tonic-based
reference implementation; the same pattern works for grpc-cpp,
grpc-java, etc.

### Q: What's the binary size cost?

Small. With `panic=abort` + `no_std`, the server-only `libgrpcuds_ffi.a`
contributes tens of KB of code over the link baseline after dead-strip; the
client side a little less. The C++ wrapper is header-only (no static bytes) and
nghttp2 is linked dynamically (no static bytes). Against the stack you're
migrating from, a stock **grpc++** server is multi-megabyte while a grpcuds C++
server doing the identical job is well under a tenth of a megabyte — roughly two
orders of magnitude smaller on disk and ~20× in memory. The
`tests/cpp/<domain>/grpcpp/` cells also prove the wire is identical (a grpcuds
C++ server serves a stock grpc++ client unmodified, and vice versa).

Exact, per-version figures and the apple-to-apple table + linking method:
[`docs/FOOTPRINT.md`](FOOTPRINT.md). (The armv7 target footprint is a
user-owned pre-release measurement, recorded there once built and verified.)

### Q: How does memory scale with concurrent connections?

Linearly and cheaply — on the order of ~16 KB heap per active connection,
dominated by nghttp2's internal session buffers, on top of a single-digit-MB
idle baseline (mostly shared `libstdc++` / `libnghttp2`, so the private cost
is far smaller). Concrete idle / 100-connection RSS and PSS numbers:
[`docs/FOOTPRINT.md`](FOOTPRINT.md).

### Q: Is there a `Server::Shutdown()`?

Not yet. The runtime exits when the event loop driving it exits and
`grpcuds::Server`'s destructor closes the listener fd. Signal handling lives
in your existing `main` / event loop.
