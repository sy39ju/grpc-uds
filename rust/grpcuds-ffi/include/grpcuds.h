// SPDX-License-Identifier: MIT OR Apache-2.0
/*
 * grpcuds — gRPC over UDS, C ABI.
 *
 * This header is the stable boundary between the Rust runtime
 * (grpcuds-ffi.{a,so}) and any consumer (C, C++, or other languages).
 * The C++ ServerBuilder pattern wrapper (grpcuds::*) is layered on top
 * of this; protoc-generated service stubs ultimately bottom out here.
 *
 * The runtime is single-threaded — meant to share an event loop with the
 * caller (e.g. epoll / libevent). All `grpcuds_*` functions are
 * non-blocking unless stated otherwise.
 */
#ifndef GRPCUDS_H_
#define GRPCUDS_H_

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* ------------------------------------------------------------------------
 * Status codes — match the standard gRPC RFC values.
 * ------------------------------------------------------------------------ */
typedef enum {
    GRPCUDS_OK                  = 0,
    GRPCUDS_CANCELLED           = 1,
    GRPCUDS_UNKNOWN             = 2,
    GRPCUDS_INVALID_ARGUMENT    = 3,
    GRPCUDS_DEADLINE_EXCEEDED   = 4,
    GRPCUDS_NOT_FOUND           = 5,
    GRPCUDS_ALREADY_EXISTS      = 6,
    GRPCUDS_PERMISSION_DENIED   = 7,
    GRPCUDS_RESOURCE_EXHAUSTED  = 8,
    GRPCUDS_FAILED_PRECONDITION = 9,
    GRPCUDS_ABORTED             = 10,
    GRPCUDS_OUT_OF_RANGE        = 11,
    GRPCUDS_UNIMPLEMENTED       = 12,
    GRPCUDS_INTERNAL            = 13,
    GRPCUDS_UNAVAILABLE         = 14,
    GRPCUDS_DATA_LOSS           = 15,
    GRPCUDS_UNAUTHENTICATED     = 16
} grpcuds_status;

/* ------------------------------------------------------------------------
 * Generated-code support (protoc-gen-grpcudspp C mode).
 *
 * A bound (call, call_id) pair — the handle generated C service stubs pass
 * to handlers for streaming sends and deferred completion. Plain data;
 * copy it freely. Valid until the call finishes or is cancelled.
 * ------------------------------------------------------------------------ */
typedef struct {
    void*   call;
    int32_t call_id;
} grpcuds_call_ref;

/* Returned BY a generated unary handler to keep the call open past the
 * handler's return ("I'll complete later" — grpc++'s ServerUnaryReactor
 * shape). Complete with the generated <Svc>_<Rpc>_respond() or
 * grpcuds_call_finish(). Far outside both the gRPC status range (0..16)
 * and -errno, so it can never be confused with either. */
#define GRPCUDS_HANDLER_DEFERRED (-1000)

/* Returned by generated client wrappers (alongside gRPC statuses >= 0):
 * the transport failed / a nanopb encode or decode failed. */
#define GRPCUDS_ERR_TRANSPORT    (-1001)
#define GRPCUDS_ERR_CODEC        (-1002)

/* ------------------------------------------------------------------------
 * Logging — severity log with a host-owned sink (gpr_log's shape).
 *
 * The library NEVER formats: every event is a static NUL-terminated
 * message (owned by the library, valid forever) plus ONE numeric argument
 * giving the event's context — an errno for I/O failures, a call/stream
 * id for call events, a queue capacity for backpressure events.
 * Formatting, timestamps and routing are the sink's business.
 *
 * Unregistered (the default) the library is fully silent. Register once
 * at startup, before serving traffic; the callback may fire from the I/O
 * thread and from any thread using a client, so the sink must be
 * thread-safe (fprintf(stderr, ...) qualifies).
 *
 * Sink rules: it runs INSIDE library callbacks (including nghttp2 event
 * handlers), so it must not call back into grpcuds_* and must not throw
 * or unwind. Unregistering does not synchronize with in-flight events —
 * a racing event may still fire once around the switch; either never
 * unregister while traffic flows, or make the sink tolerate it.
 * ------------------------------------------------------------------------ */
typedef enum {
    GRPCUDS_LOG_ERROR = 0,   /* broken I/O, protocol failures            */
    GRPCUDS_LOG_INFO  = 1,   /* lifecycle: deadlines, reconnects, ...    */
    GRPCUDS_LOG_DEBUG = 2    /* per-connection chatter: accept, EOF, RST */
} grpcuds_log_level;

typedef void (*grpcuds_log_fn)(int level, const char* msg, int64_t arg,
                               void* user_data);

/* Register (or with NULL, remove) the process-global log sink. Events
 * more verbose than `max_level` are not delivered. */
void grpcuds_set_log_callback(grpcuds_log_fn callback, int max_level,
                              void* user_data);

/* ------------------------------------------------------------------------
 * Opaque handles.
 *
 *  - grpcuds_server: owns the UDS listener fd + registered method table.
 *  - grpcuds_conn:   one accepted connection (one HTTP/2 session).
 *
 * Inside a handler invocation, the runtime hands the user an opaque
 * `void* call` pointer plus an `int32_t call_id`. Pass that pair to
 * grpcuds_call_write / grpcuds_call_finish; do not dereference it.
 * The pair is valid for the lifetime of the call (until grpcuds_call_finish
 * returns OK or the call is cancelled).
 * ------------------------------------------------------------------------ */
typedef struct grpcuds_server grpcuds_server;
typedef struct grpcuds_conn   grpcuds_conn;

/* ------------------------------------------------------------------------
 * Handler signature.
 *
 *   `call`      — opaque, pass back unchanged to grpcuds_call_*.
 *   `call_id`   — stream identifier scoped to this connection.
 *   `req`       — pointer to the request message payload (already
 *                 unframed; the 5-byte gRPC prefix has been stripped).
 *   `req_len`   — length of the payload in bytes.
 *   `user_data` — whatever was passed to grpcuds_server_register_method.
 *
 * Return value: a gRPC status code (0 = OK). If the handler returns
 * non-zero without having called grpcuds_call_finish, the runtime
 * auto-finishes the call with that status.
 *
 * A handler may write zero or more messages with grpcuds_call_write
 * before finishing. For streaming handlers, save (`call`, `call_id`)
 * and call grpcuds_call_write / grpcuds_call_finish from a later
 * event-loop callback (e.g. a BLE notification).
 * ------------------------------------------------------------------------ */
typedef int (*grpcuds_handler_fn)(
    void*          call,
    int32_t        call_id,
    const uint8_t* req,
    size_t         req_len,
    void*          user_data
);

/* ------------------------------------------------------------------------
 * Server lifecycle.
 * ------------------------------------------------------------------------ */

/* Allocate a new server. Returns NULL on OOM. */
grpcuds_server* grpcuds_server_new(void);

/* Free a server. Closes the listener fd and unlinks the bound socket
 * path. Outstanding `grpcuds_conn*` from this server become invalid;
 * the caller must free those first. */
void grpcuds_server_free(grpcuds_server* s);

/* Bind to a UNIX domain socket path (nul-terminated). Returns 0 on
 * success, a negative POSIX errno on failure (-EINVAL for bad arguments,
 * negated errno for socket / bind / listen failures, else -1). */
int grpcuds_server_bind_uds(grpcuds_server* s, const char* path);

/* Returns the listener socket fd, or -1 if not bound. Plug it into your
 * event loop (e.g. epoll / libevent fd-watch registration). */
int grpcuds_server_listener_fd(const grpcuds_server* s);

/* Register a handler for a `:path` value (e.g. "/pkg.Svc/Method").
 * Handlers registered here are applied to every connection accepted
 * after this call. Returns 0 on success, -EINVAL on bad arguments,
 * -ENOMEM on allocation failure. */
int grpcuds_server_register_method(
    grpcuds_server*    s,
    const char*        path,
    grpcuds_handler_fn handler,
    void*              user_data
);

/* Try to accept a pending connection. Returns a new `grpcuds_conn*`
 * (caller owns; free with grpcuds_conn_free), or NULL if no client is
 * pending (EAGAIN). */
grpcuds_conn* grpcuds_server_accept(grpcuds_server* s);

/* ------------------------------------------------------------------------
 * Connection driver.
 * ------------------------------------------------------------------------ */

/* The connection's socket fd. Add it to your event loop with POLLIN;
 * arm POLLOUT only when grpcuds_conn_wants_write returns 1. */
int grpcuds_conn_fd(const grpcuds_conn* c);

/* Drive one I/O cycle (read + dispatch + opportunistic write). Returns:
 *    0   — connection still alive; keep polling.
 *    1   — connection closed (peer EOF or fully drained); the caller
 *          must grpcuds_conn_free.
 *    <0  — error (negated errno or -1 if unknown).
 *
 * Equivalent to grpcuds_conn_tick_read. Prefer the revents-aware pair
 * below in a poll(2)-style loop — skipping the read syscall on
 * POLLOUT-only iterations is the whole reason the split exists. */
int grpcuds_conn_tick(grpcuds_conn* c);

/* Read-phase tick. Call when revents includes POLLIN | POLLHUP | POLLERR.
 * Drains the socket into nghttp2, runs any newly-Complete handler
 * dispatches, then opportunistically flushes nghttp2's outbound queue —
 * so a single call covers the common "both POLLIN and POLLOUT fired" case.
 * Same return convention as grpcuds_conn_tick. */
int grpcuds_conn_tick_read(grpcuds_conn* c);

/* Write-phase tick. Call when revents is POLLOUT only (a previous write
 * hit EAGAIN and the event loop re-armed write interest). Skips the
 * read syscall + dispatch pass entirely; just drains buffered output.
 * Same return convention as grpcuds_conn_tick. */
int grpcuds_conn_tick_write(grpcuds_conn* c);

/* Returns 1 iff the connection currently has outbound work — either
 * nghttp2 has frames queued or a previous write left bytes buffered.
 * Use this to decide whether to keep POLLOUT armed. Returns 0 for false,
 * or a negative value on null input. */
int grpcuds_conn_wants_write(const grpcuds_conn* c);

/* The opaque `call` handle this connection's handlers receive. C++ hosts
 * pass it to the outbound mailbox's RegisterCall/UnregisterCall so writes
 * queued by producer threads are dropped — not dereferenced — once the
 * connection is freed. Returns NULL on NULL. */
void* grpcuds_conn_call_handle(grpcuds_conn* c);

/* Deadlines: the runtime parses the client's `grpc-timeout` header. A
 * dispatched call whose deadline passes is finished with grpc-status 4
 * (DEADLINE_EXCEEDED) and its cancel hook fires — checked on every
 * grpcuds_conn_tick*. For deadlines to fire on an IDLE connection, bound
 * your poll timeout with the earliest pending deadline:
 *
 *     int64_t d = grpcuds_conn_next_deadline_ms(conn);
 *     poll(fds, n, d < 0 ? base_timeout : (int)(d < base_timeout ? d : base_timeout));
 *
 * Returns remaining ms (0 = due now — tick the connection), -1 when no
 * in-flight call has a deadline, -EINVAL on null. */
int64_t grpcuds_conn_next_deadline_ms(const grpcuds_conn* c);

/* Free a connection. Closes its socket fd. Safe on NULL. */
void grpcuds_conn_free(grpcuds_conn* c);

/* ------------------------------------------------------------------------
 * Per-call output.
 *
 *  - grpcuds_call_write: enqueue one gRPC message. The runtime prepends
 *    the 5-byte length prefix; do not include it in `data`.
 *  - grpcuds_call_finish: ship the trailing HEADERS with `status` and
 *    close the server side of the stream.
 *
 * Negative return values are POSIX errno-style codes:
 *    -EINVAL   null pointer or invalid argument
 *    -ENOENT   no active stream for the given (call, call_id)
 *    -EPIPE    stream already finished — subsequent writes/finishes refused
 *    -EAGAIN   outbound queue is full and policy is Reject; caller should
 *              retry later or surface the loss to its source
 *    -ENOMEM   internal allocation failure
 *    other     pass-through from libnghttp2; see grpcuds_session_*
 *
 * THREAD-SAFE (always): call these from inside the handler, from a later
 * event callback, OR from any other thread (a worker/producer). On the
 * thread registered via grpcuds_mailbox_register_io_thread() they touch the
 * core directly (honoring backpressure); off it they copy the payload into
 * the process-global outbound mailbox and poke its wakeup fd — the real core
 * call then happens on the I/O thread inside grpcuds_mailbox_drain(). Until an
 * I/O thread is registered, every caller takes the direct path (zero setup for
 * single-threaded servers). The connection must not have been freed. See the
 * "Outbound mailbox" section below and docs/THREADING.md.
 * ------------------------------------------------------------------------ */
int grpcuds_call_write(
    void*          call,
    int32_t        call_id,
    const uint8_t* data,
    size_t         len
);

int grpcuds_call_finish(
    void*   call,
    int32_t call_id,
    int     status
);

/* Like grpcuds_call_finish, but also ships a `grpc-message` trailer.
 * `msg`/`msg_len` are the raw (un-encoded) message bytes; the runtime
 * percent-encodes them per the gRPC wire spec. A NULL `msg` or `msg_len == 0`
 * behaves exactly like grpcuds_call_finish (status-only trailer). */
int grpcuds_call_finish_msg(
    void*          call,
    int32_t        call_id,
    int            status,
    const uint8_t* msg,
    size_t         msg_len
);

/* Remaining milliseconds of this call's `grpc-timeout` budget — stock
 * gRPC's "context deadline" for handlers that want to skip work that
 * cannot finish in time:
 *
 *     int64_t left = grpcuds_call_time_remaining_ms(call, call_id);
 *     if (left >= 0 && left < COST_MS) return GRPCUDS_DEADLINE_EXCEEDED;
 *
 * Returns >= 0 when the client sent a deadline (0 = already due), -1 when
 * it sent none, -ENOENT if call_id has no active stream, -EINVAL on null. */
int64_t grpcuds_call_time_remaining_ms(void* call, int32_t call_id);

/* ------------------------------------------------------------------------
 * Cancel hooks.
 *
 * Install a cleanup callback that fires once when the stream is closed
 * with a non-zero error code (peer RST_STREAM, protocol error,
 * connection drop). Use this from a streaming handler to stop an async
 * producer (BLE scan, GATT notify) and free per-call state.
 *
 * The hook does NOT fire on graceful close (server writes the
 * `grpc-status:0` trailer and the peer ACKs). Handlers that need a
 * "called exactly once on every path" lifecycle should also run their
 * cleanup from the graceful-finish path.
 *
 * `user_data` lifetime contract — the pointer MUST remain valid until
 * either:
 *    (a) `callback(user_data)` fires (cancel path), or
 *    (b) the call closes gracefully and the connection itself drops
 *        (the hook is then forgotten without firing — your other
 *        cleanup path is responsible for `free`).
 *
 * Stack-bound pointers (locals in the streaming handler) DO NOT WORK
 * because the handler returns long before the hook fires.
 *
 * Safe pattern:
 *
 *     struct scan_state {
 *         int  scan_handle;
 *         // ... whatever your BLE source needs ...
 *     };
 *
 *     static void scan_cleanup(void* ud) {
 *         struct scan_state* s = (struct scan_state*) ud;
 *         bt_adapter_le_stop_scan(s->scan_handle);
 *         free(s);
 *     }
 *
 *     int start_scan_handler(void* call, int32_t call_id,
 *                            const uint8_t* req, size_t req_len,
 *                            void* user_data) {
 *         struct scan_state* s = malloc(sizeof(*s));
 *         if (!s) return GRPCUDS_RESOURCE_EXHAUSTED;
 *         bt_adapter_le_start_scan(&s->scan_handle, on_scan_result, call,
 *                                  call_id);
 *         grpcuds_call_set_cancel_hook(call, call_id, scan_cleanup, s);
 *         return GRPCUDS_OK;   // leave stream open; BLE callback pushes
 *     }
 *
 * Returns 0 on success, -ENOENT if call_id has no active stream, or
 * -EINVAL on null pointer / null callback.
 * ------------------------------------------------------------------------ */
int grpcuds_call_set_cancel_hook(
    void*   call,
    int32_t call_id,
    void  (*callback)(void* user_data),
    void*   user_data
);

/* ------------------------------------------------------------------------
 * Backpressure.
 *
 * Per-call queue policy for streaming RPCs whose async producer can
 * outpace the wire (BLE scan / GATT notifications). The bounded /
 * unbounded cases are split into two entry points so the misuse-prone
 * "capacity == 0 but I picked a policy" combo isn't representable on the
 * wire. The default for a freshly-dispatched call is unbounded.
 *
 * Returns 0 on success, -ENOENT if call_id has no active stream,
 * -EINVAL on null pointer, capacity == 0 (bounded only), or an unknown
 * policy_kind.
 * ------------------------------------------------------------------------ */
typedef enum {
    /* Refuse new writes when the queue is at capacity. grpcuds_call_write
     * returns -EAGAIN; the producer is expected to log, buffer, or shed.
     * Suitable for lossless streams (GATT notifications). */
    GRPCUDS_BACKPRESSURE_REJECT      = 0,
    /* Evict the oldest *unstarted* message to make room. grpcuds_call_write
     * always returns 0. Suitable for latest-N streams (BLE scan results).
     * The currently in-flight head is never dropped. */
    GRPCUDS_BACKPRESSURE_DROP_OLDEST = 1
} grpcuds_backpressure_policy;

int grpcuds_call_set_backpressure_unbounded(
    void*   call,
    int32_t call_id
);

int grpcuds_call_set_backpressure_bounded(
    void*   call,
    int32_t call_id,
    size_t  capacity,    /* must be > 0 */
    int     policy_kind  /* grpcuds_backpressure_policy */
);


/* ------------------------------------------------------------------------
 * Outbound mailbox — thread-safe off-I/O-thread writes.
 *
 * grpcuds_call_write / _finish / _finish_msg are ALWAYS thread-safe (above):
 * off the registered I/O thread they enqueue into a process-global mailbox
 * instead of touching the single-threaded core. These three wire that mailbox
 * into your poll loop. They are needed only if a thread OTHER than the I/O
 * thread ever writes (e.g. a BLE/sensor producer thread); a purely
 * single-threaded server can ignore them entirely. One Server per process.
 *
 * Integration (the reference poll loop does exactly this):
 *
 *     grpcuds_mailbox_register_io_thread();          // once, on the I/O thread
 *     int wfd = grpcuds_mailbox_wakeup_fd();         // add wfd to your poll set
 *     for (;;) {
 *         poll(fds, n, timeout);                     // fds includes wfd + conns
 *         if (revents(wfd) & POLLIN) {
 *             read(wfd, &c, 8);                      // (optional) clear counter
 *             grpcuds_mailbox_drain();               // replay queued writes
 *         }
 *         // ... tick connections ...
 *     }
 *
 * A producer thread then just calls grpcuds_call_write(call, ...) — it is
 * copied into the mailbox and shipped on the next drain. See docs/THREADING.md.
 * ------------------------------------------------------------------------ */

/* The mailbox wakeup eventfd (created on first call). Add it to your poll set;
 * when it is readable, call grpcuds_mailbox_drain() on the I/O thread. Returns
 * the fd, or -1 if eventfd creation failed. */
int grpcuds_mailbox_wakeup_fd(void);

/* Mark the calling thread as the I/O thread — the one that runs the poll loop
 * and drains the mailbox. Writes from this thread go straight to the core;
 * writes from any other thread are queued. Until this is called, every thread
 * is treated as the I/O thread (so single-threaded servers need no setup). */
void grpcuds_mailbox_register_io_thread(void);

/* Replay every queued off-thread write into the core, in FIFO order. Call on
 * the I/O thread when the wakeup fd is readable (or once per poll iteration).
 * Writes for a connection freed since they were queued are dropped. */
void grpcuds_mailbox_drain(void);


/* ------------------------------------------------------------------------
 * Health checking — the standard grpc.health.v1.Health service.
 *
 * Register it and stock tooling (grpc_health_probe, grpcurl, tonic-health,
 * orchestrator probes) can ask "is this daemon serving?" over the same socket,
 * no custom protocol:
 *
 *     grpcuds_health_register(server);                            // "" = SERVING
 *     grpcuds_health_set_status("ble.BleScanner", GRPCUDS_HEALTH_SERVING);
 *     // ... later, when the radio dies:
 *     grpcuds_health_set_status("ble.BleScanner", GRPCUDS_HEALTH_NOT_SERVING);
 *
 * Check is unary (an unknown service name fails NOT_FOUND, per the protocol);
 * Watch is server-streaming (immediate status — SERVICE_UNKNOWN for an
 * unregistered name — then every change). The two messages (a string field and
 * a varint enum) are encoded in-library; no nanopb. A server that never calls
 * grpcuds_health_register pays nothing (the code is dropped at link time).
 * ------------------------------------------------------------------------ */

/* grpc.health.v1.HealthCheckResponse.ServingStatus wire values. */
enum grpcuds_health_status {
    GRPCUDS_HEALTH_UNKNOWN         = 0,
    GRPCUDS_HEALTH_SERVING         = 1,
    GRPCUDS_HEALTH_NOT_SERVING     = 2,
    GRPCUDS_HEALTH_SERVICE_UNKNOWN = 3  /* Watch-only: name not registered */
};

/* Register grpc.health.v1.Health/{Check,Watch} on `server`. The overall
 * service ("") starts SERVING. Returns 0, or a negative errno on a null server
 * or registration failure. One health registry per process. */
int grpcuds_health_register(grpcuds_server* server);

/* Set (or register) `service`'s serving status and notify its Watchers.
 * `service` is a NUL-terminated name ("" = the server overall); `status` is a
 * GRPCUDS_HEALTH_* value. Thread-safe — call from any thread (off-I/O-thread
 * watcher writes ride the outbound mailbox). */
void grpcuds_health_set_status(const char* service, int status);


/* ======================================================================== *
 *  Client C ABI  (only present when grpcuds-ffi is built with `client`)
 *
 *  A blocking client over one UDS connection. One call is in flight at a
 *  time. Build server-only and these symbols are absent (and vice versa),
 *  so an embedder links only the side it uses.
 * ======================================================================== */

typedef struct grpcuds_client   grpcuds_client;
typedef struct grpcuds_response grpcuds_response;
typedef struct grpcuds_stream   grpcuds_stream;

/* Connect to a gRPC server on the UDS `path` (NUL-terminated). NULL on error.
 *
 * If a later call finds the connection dead (server restarted: EOF / EPIPE),
 * the handle makes ONE lazy reconnect attempt to the same path before that
 * call — stock-gRPC IDLE-channel style. No retry loop on the call path: a
 * failed reconnect fails the call immediately. */
grpcuds_client* grpcuds_client_connect(const char* path);

/* Like grpcuds_client_connect, but retries with exponential backoff (50 ms
 * x 1.6 up to a 1 s cap, +/-20% jitter — bounded CPU; each individual
 * attempt is bounded at 250 ms) until `timeout_ms` elapses. Covers the
 * daemon-startup race: the socket file may not exist yet, or may exist
 * before the server calls listen(); both retry. `timeout_ms == 0` makes
 * exactly one attempt. NULL when the deadline passes without a connection. */
grpcuds_client* grpcuds_client_connect_wait(const char* path, uint32_t timeout_ms);

void            grpcuds_client_free(grpcuds_client* client);

/* Per-call timeout in milliseconds; 0 clears it (the default: wait forever).
 * Applies to every call submitted afterwards and covers the WHOLE call —
 * the unary response, or the entire lifetime of a server-stream (gRPC
 * deadline semantics, enforced client-side). On expiry the call fails
 * locally with grpc-status 4 (DEADLINE_EXCEEDED) and the stream is
 * cancelled with RST_STREAM, so the server's cancel hook fires and any
 * deferred work can stop. Returns 0, or -EINVAL on a null client. */
int grpcuds_client_set_timeout_ms(grpcuds_client* client, uint32_t timeout_ms);

/* Unary call. `path` is "/pkg.Svc/Method". Returns a response (inspect its
 * status + body) on transport success, or NULL on transport failure. */
grpcuds_response* grpcuds_client_unary(
    grpcuds_client* client,
    const char*     path,
    const uint8_t*  req,
    size_t          req_len
);

/* Numeric grpc-status of a response (0 = OK). */
int            grpcuds_response_status(const grpcuds_response* resp);
/* Response body bytes; writes the length through `len`. Valid until free. */
const uint8_t* grpcuds_response_body(const grpcuds_response* resp, size_t* len);
/* grpc-message bytes (NOT NUL-terminated); writes length, NULL if none. */
const uint8_t* grpcuds_response_message_bytes(const grpcuds_response* resp, size_t* len);
void           grpcuds_response_free(grpcuds_response* resp);

/* Server-streaming call. Read with grpcuds_stream_next until it returns NULL.
 * The `client` must outlive the returned stream. NULL on transport failure. */
grpcuds_stream* grpcuds_client_server_streaming(
    grpcuds_client* client,
    const char*     path,
    const uint8_t*  req,
    size_t          req_len
);

/* Next message: writes its length through `len`, returns the bytes (valid
 * until the next call or free). NULL at end of stream (then check status). */
const uint8_t* grpcuds_stream_next(grpcuds_stream* stream, size_t* len);
/* Final grpc-status of a finished stream (0 = OK). */
int            grpcuds_stream_status(const grpcuds_stream* stream);
void           grpcuds_stream_free(grpcuds_stream* stream);

#ifdef __cplusplus
}
#endif

#endif /* GRPCUDS_H_ */
