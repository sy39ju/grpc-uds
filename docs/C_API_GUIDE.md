<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
# grpcuds from plain C

The C ABI (`grpcuds.h` + `libgrpcuds_ffi.a`) is the library's primary
boundary — the C++ wrapper (`grpcudspp/`) is a header-only layer on top of
it. A pure-C consumer uses the same artifacts every C++ consumer links,
just without the sugar. The runnable proof lives at `example/c/`
(`example-c/` in the SDK bundle): a complete server + client in two `.c`
files.

## What you need

| Piece | Where | Language |
| --- | --- | --- |
| `grpcuds.h` | `sdk/include/` (repo: `rust/grpcuds-ffi/include/`) | C |
| `libgrpcuds_ffi.a` (or `.so`) | `target/lib/` (repo: `rust/target/release/`) | — |
| nanopb runtime + generator | `sdk/nanopb/` (repo: `example/nanopb/`) | C / python |
| link line | `-lnghttp2 -lpthread -ldl -lm` | — |

**Codegen.** `nanopb_generator` turns your `.proto` into pure-C structs +
field descriptors (`echo.pb.{h,c}`) — that part is required. Service
codegen is **optional**: hand-registering by path string (below) is a few
lines per RPC, and for larger protos the same `protoc-gen-grpcudspp`
plugin emits plain-C stubs:

```sh
protoc --plugin=protoc-gen-grpcudspp=host/bin/protoc-gen-grpcudspp \
       --grpcudspp_out=gen --grpcudspp_opt=c -I proto proto/echo.proto
# -> gen/echo.grpcuds.h  (service table, typed client wrappers — static inline)
#    gen/echo.grpcuds.c  (server trampolines + register — link into servers only)
```

The generated surface (per service `pkg.Svc`):

```c
pkg_Svc_service svc = {0};            /* NULL entry = method not registered  */
svc.user_data = &my_state;
svc.Say  = my_say_handler;            /* unary:  (ref, req, resp, ud) -> status
                                         GRPCUDS_HANDLER_DEFERRED keeps the
                                         call open; complete later with
                                         pkg_Svc_Say_respond(ref, &resp)     */
svc.Scan = my_scan_handler;           /* stream: (ref, req, ud) -> status;
                                         push pkg_Svc_Scan_send(ref, &msg),
                                         end  pkg_Svc_Scan_finish(ref, st)   */
pkg_Svc_register(server, &svc);       /* svc must outlive the server         */

/* client side — typed wrappers, static inline in the header: */
pkg_Svc_Say(client, &req, &resp);                  /* -> gRPC status        */
grpcuds_stream* st = pkg_Svc_Scan_start(client, &req);
while (pkg_Svc_Scan_next(st, &msg) == 1) { /* ... */ }
```

Everything client-side (and the `_respond`/`_send`/`_finish` helpers) is
`static inline` in the header, so server-only / client-only builds never
see undefined symbols for the half they don't use — compile the `.c` into
server binaries only. The on-stack encode scratch defaults to 1 KiB; to
raise it, define `GRPCUDSC_MAX_MESSAGE_SIZE` **project-wide** (sync unary
responses are encoded inside the generated `.c`, so a per-TU override in
your own code would silently not apply there).

## The server shape

```c
#include <grpcuds.h>
#include <pb_decode.h>
#include <pb_encode.h>
#include "echo.pb.h"

/* One handler per RPC. This glue (~15 lines) is exactly what a generated
 * stub would hide. */
static int say_handler(void* call, int32_t call_id, const uint8_t* req,
                       size_t req_len, void* user_data) {
    echo_EchoRequest in = echo_EchoRequest_init_zero;
    pb_istream_t is = pb_istream_from_buffer(req, req_len);
    if (!pb_decode(&is, echo_EchoRequest_fields, &in))
        return GRPCUDS_INVALID_ARGUMENT;     /* auto-finishes with this status */

    echo_EchoReply out = echo_EchoReply_init_zero;
    /* ... business logic ... */

    uint8_t buf[256];
    pb_ostream_t os = pb_ostream_from_buffer(buf, sizeof buf);
    if (!pb_encode(&os, echo_EchoReply_fields, &out)) return GRPCUDS_INTERNAL;
    grpcuds_call_write(call, call_id, buf, os.bytes_written);
    grpcuds_call_finish(call, call_id, GRPCUDS_OK);
    return GRPCUDS_OK;
}

grpcuds_server* s = grpcuds_server_new();
grpcuds_server_bind_uds(s, "/run/echo.sock");
grpcuds_server_register_method(s, "/echo.Echo/Say", say_handler, NULL);
```

You own the event loop: watch `grpcuds_server_listener_fd` for accepts and
each `grpcuds_conn_fd` for readability (add `POLLOUT` while
`grpcuds_conn_wants_write`), call `grpcuds_conn_tick` on readiness, bound
the poll timeout with `grpcuds_conn_next_deadline_ms` so `grpc-timeout`
deadlines fire on idle connections. `example/c/server.c` is a complete
~60-line `poll(2)` loop; epoll / libevent plug in the same way.

Handler semantics worth knowing (all in `grpcuds.h`):

- **Streaming / deferred completion** — return `GRPCUDS_OK` *without*
  finishing and the call stays open; finish later (same thread) with
  `grpcuds_call_write` / `grpcuds_call_finish`. Returning non-OK
  auto-finishes with that status.
- **Cancellation** — `grpcuds_call_set_cancel_hook` fires when the peer
  resets the stream.
- **Deadline budget** — `grpcuds_call_time_remaining_ms` inside a handler.
- **Backpressure** — `grpcuds_call_set_backpressure_bounded` (reject /
  drop-oldest).

## The client shape

```c
grpcuds_client* c = grpcuds_client_connect_wait("/run/echo.sock", 3000);
grpcuds_client_set_timeout_ms(c, 500);              /* per-call deadline */
grpcuds_response* r = grpcuds_client_unary(c, "/echo.Echo/Say", buf, len);
if (r && grpcuds_response_status(r) == GRPCUDS_OK) {
    size_t n; const uint8_t* body = grpcuds_response_body(r, &n);
    /* pb_decode(body, n) ... */
}
grpcuds_response_free(r);
```

`connect_wait` retries with backoff (daemon-startup race); a connection
that later dies reconnects lazily on the next call. Server-streaming:
`grpcuds_client_server_streaming` + `grpcuds_stream_next` until NULL.

## Logging

The library is silent until you hand it a sink:

```c
static void sink(int level, const char* msg, int64_t arg, void* ud) {
    fprintf(stderr, "grpcuds[%c] %s (%lld)\n", "EID"[level], msg, (long long)arg);
}
grpcuds_set_log_callback(sink, GRPCUDS_LOG_INFO, NULL);
```

Messages are static strings + one numeric argument — the library never
formats (see the contract in `grpcuds.h`).

## Threading

The core is single-threaded — the nghttp2 session belongs to one thread. But
**`grpcuds_call_write` / `_finish` / `_finish_msg` are thread-safe**: off the
registered I/O thread they enqueue into the C ABI's outbound mailbox instead of
touching the core. So a producer thread (a radio/sensor callback) can push
stream messages directly. The I/O thread:

1. calls `grpcuds_mailbox_register_io_thread()` once,
2. adds `grpcuds_mailbox_wakeup_fd()` to its poll set,
3. calls `grpcuds_mailbox_drain()` when that fd is readable (or each loop).

A single-threaded server needs none of this — until an I/O thread is
registered, every write takes the direct path. `example/c/server.c` streams
from a producer thread this way; `docs/THREADING.md` has the full design.

## Health checking

The standard `grpc.health.v1.Health` service is built in — register it and
stock probers (`grpc_health_probe`, `grpcurl`, tonic-health) work unmodified:

```c
grpcuds_health_register(server);                       // "" starts SERVING
grpcuds_health_set_status("my.Service", GRPCUDS_HEALTH_SERVING);
// ... later, when the backend degrades:
grpcuds_health_set_status("my.Service", GRPCUDS_HEALTH_NOT_SERVING);
```

`Check` is unary (unknown service → `NOT_FOUND`, per the protocol); `Watch` is
server-streaming (immediate status, then every change). `set_status` is
thread-safe. A server that never calls `grpcuds_health_register` pays nothing —
the code is dropped at link time. `example/c` demonstrates a Check + Watch probe.

## Everything is in the C ABI

The thread-safe outbound mailbox, the standard health service, deadlines,
cancellation, backpressure, reconnect, logging, and `wirelog` capture all work
identically from plain C — there is no C++-only functionality.
