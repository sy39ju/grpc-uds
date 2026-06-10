# grpcuds design document

This document records the decisions, rationale, and implementation patterns
settled during design discussion. The starting point for a new session.

---

## 1. Purpose and context

A lightweight server transport library that is **wire-level compatible with stock
gRPC clients** over UDS (Unix Domain Socket). On an embedded device the server
performs domain operations (canonical example: BLE scan/GATT) and responds to /
streams the results back to gRPC clients.

- The client uses stock gRPC and is **unchangeable** → the server must speak
  HTTP/2 + the gRPC wire. A lightweight length-prefix framing alone is not
  compatible (the client fails at the HTTP/2 handshake).
- Server ↔ client is local IPC within the same device → UDS, **no security (no
  TLS)**.
- Server platform: **embedded Linux (armv7 etc.)**. Domain logic (BLE scan/GATT
  etc.) stays in the host's native C API, and the core does not know about it.
- Main loop: **single-threaded event loop** (epoll/libevent — host's
  choice).

---

## 2. Core architecture decisions (with rationale)

### 2.1 No own HTTP/2 implementation → dynamically link the system libnghttp2
- The heavy part of gRPC is not message framing but the **HTTP/2 layer** (HPACK
  header compression, flow control, frame handling, multiplexing). Implementing
  this ourselves would be thousands of lines + a mandatory HPACK Huffman decoder
  + edge-case bugs → judged **not maintainable long-term**.
- libnghttp2 handles all of this. `libnghttp2.so.14` is **confirmed present** on
  the armv7 target.
- Because it is **dynamically linked**, the app binary's contribution is only a
  few KB of PLT/GOT. A static bundle (+100~180KB) is forbidden.

### 2.2 Implement only the gRPC framing ourselves
- The 5-byte message prefix (1B compressed flag + 4B big-endian length) +
  payload.
- `:path /package.Service/Method` routing, `content-type: application/grpc`,
  `te: trailers`.
- Response termination via the trailing HEADERS frame's `grpc-status`
  (+`grpc-message`).

### 2.3 Messages via nanopb (no protobuf-full)
- Assumes an environment that already uses `.proto` → nanopb codegen. Reuse the
  message types directly → preserve the business logic (the message-handling
  code).
- protobuf-full (C++ `google::protobuf::Message`) explodes in size → forbidden.

### 2.4 The core does not know about message serialization (important)
- The core only handles **framed bytes + path + stream id**. nanopb
  encode/decode is performed in the generated C stub layer.
- Effect: the Rust core does not even need a `pb_msgdesc_t` FFI binding, so the
  boundary stays clean and the core is independent of any particular `.proto`
  (= reusable).

### 2.5 Language = Rust (no_std), C ABI surface. BLE stays C.
- Reason for choosing it: **long-term safety + reuse across products**. We
  already have a Rust cross-build environment.
- The transport parses untrusted input (IPC) → memory safety is highly valuable.
  Model the stream state machine / deferred-resume / cancel safely with types
  (`enum`, `Result`).
- Domain logic such as BLE is the host C API, so it stays C. The core does not
  know BLE (generic) → the FFI boundary is only "byte injection".

---

## 3. Crate structure

```
grpcuds-sys   raw nghttp2 FFI (generated directly with bindgen)
grpcuds-core  safe wrapper + gRPC framing + UDS + stream state machine (no_std)
grpcuds-ffi   C ABI surface (staticlib + cdylib, panic=abort, symbols grpcuds_*)
```

### 3.1 grpcuds-sys
- **Do not use existing crates**: `nghttp2-sys` (0.1.1, bindgen ^0.40, docs.rs
  build failing), `libnghttp2-sys`, etc. have been abandoned since ~2018. They
  conflict with the long-term-asset goal.
- In `build.rs`, convert `nghttp2.h` with bindgen (host uses the crate-internal
  `vendor/nghttp2/`, cross resolves it via `--sysroot`) + dynamically link the
  system `.so`:
  ```rust
  println!("cargo:rustc-link-lib=dylib=nghttp2");
  bindgen::Builder::default().header("nghttp2.h")
      .allowlist_function("nghttp2_.*").allowlist_type("nghttp2_.*")
      .allowlist_var("NGHTTP2_.*").generate()?;
  ```
- Only the symbols we need: `session_server_new`/`session_del`,
  `session_mem_recv`/`mem_send`, `session_callbacks_*`, `submit_response`,
  `submit_trailer`, `submit_rst_stream`, `session_resume_data`, and the callback
  signatures. On the order of a few dozen, so the maintenance burden is small.

### 3.2 grpcuds-core
- `#![no_std]` + `alloc`, system malloc global allocator.
- panic-free (no catch_unwind). Does not use `core::fmt`.
- Fills in 5 callbacks: `on_begin_headers`, `on_header` (collects `:path`),
  `on_frame_recv` (request complete → dispatch), `on_data_chunk` (accumulates the
  request message), `on_stream_close` (cancel/termination cleanup).

### 3.3 grpcuds-ffi
- `crate-type = ["staticlib","cdylib"]`, `panic="abort"`.
- Public symbols `grpcuds_*` (e.g. `grpcuds_server_create`,
  `grpcuds_stream_write`).

---

## 4. codegen (protoc plugin)

- The nanopb generator emits only `message`s and **ignores** `service { rpc ...
  }`. The service definitions already exist in the `.proto` (confirmed).
- Our plugin turns the service definitions into C stubs (a lightweight C version
  of gRPC's protoc-gen): a dispatch table + trampolines (void* ↔ concrete-type
  casting) + a registration function.
- The generated stubs are **included in the host app build**, not in the core
  library binary. → No core rebuild when the `.proto` changes, and the core size
  is fixed regardless of the service.
- Dispatch table (concept):
  ```c
  typedef struct {
      const char *method;             // "StartScan"
      const pb_msgdesc_t *req_fields; // ScanRequest_fields
      const pb_msgdesc_t *resp_fields;
      bool server_stream;
      grpcuds_handler fn;
  } grpcuds_method;
  ```
  (nanopb encode/decode happens on the stub side. The core handles only bytes.)

---

## 5. streaming implementation pattern (the core complexity)

Connect BLE (push) ↔ the nghttp2 data provider (pull) via **deferred/resume**.

```
read_callback: if the queue is empty, return NGHTTP2_ERR_DEFERRED (sleep it)
BLE callback:  enqueue(framed msg) + nghttp2_session_resume_data (wake it)
termination:   DATA EOF + NGHTTP2_DATA_FLAG_NO_END_STREAM
               + nghttp2_submit_trailer (grpc-status:0)
cancel:        detect RST_STREAM in on_stream_close → clean up the BLE
               scan/subscription (prevent leaks)
```

- **Backpressure**: queue cap + policy. scan = keep only the latest N, GATT
  notification = buffer (when loss is not acceptable). nghttp2 handles the flow
  control window, so we only handle the queue policy.
- **Event-loop integration**: the host event loop (epoll/libevent) watches
  the UDS fd → on readable, `nghttp2_session_mem_recv`; when a write is needed,
  `nghttp2_session_mem_send`. Single-threaded, so the domain callback and gRPC
  transmission run on the same thread → minimal synchronization.

### Complexity grading
- Easy: session setup, unary request/response, header parsing, fd integration
  (write once, then fixed).
- Moderate (the learning-curve focus): the deferred/resume cycle, wiring BLE
  callback → queue → resume.
- Caution (bug-prone): BLE cleanup on cancel, queue backpressure policy.
- Core glue ~600~900 lines. The HTTP/2 standard is stable, so long-term
  maintenance is feasible (risk is confined to our glue code). A different league
  from implementing HTTP/2 ourselves.

---

## 6. Migrating from an existing gRPC API

- **Not a full drop-in.** The mental model is the same as gRPC, and because of
  nanopb the message code is preserved as-is.
- What changes: server bootstrap, the handler registration mechanism, the write
  call in streaming handlers.
- Mapping examples: `ServerWriter::Write` → `grpcuds_stream_write`, `Status::OK`
  → `GRPCUDS_OK`.
- Handler signatures (C):
  ```c
  grpcuds_status ble_Connect(grpcuds_call *call, const ConnectRequest *req,
                             ConnectReply *reply);            // unary
  grpcuds_status ble_StartScan(grpcuds_call *call, const ScanRequest *req); // stream
  // stream: repeat grpcuds_stream_write(call, &result);
  ```
- Deliverables: the core library (.a/.so + headers) + the protoc plugin + a
  **migration mapping table**.

---

## 7. Size

- The dynamic link to libnghttp2 is settled (confirmed present) → only the core
  addition counts against the budget.
- Measured (x86-64, no_std+alloc+panic=abort+fmt-avoidance): the server- and
  client-only `libgrpcuds_ffi.a` each add only tens of KB over the link
  baseline, well inside budget; going to std would add the static-std floor
  (hundreds of KB) → keeping no_std is essential. Exact, per-version figures:
  [docs/FOOTPRINT.md](docs/FOOTPRINT.md).
- Verification: measure with `grpcuds-size-probe/`. Reported values =
  [1] presence/size of libnghttp2, [4] `--gc-sections` linked binary size,
  [5] cargo-bloat top entries (check for std/fmt leakage).
- If it goes well past 60KB, check cargo-bloat for `core::fmt`/std symbols →
  remove them. As a last resort, build-std + panic_immediate_abort (nightly) can
  save more, but it is low priority.

---

## 8. Delivery status

Everything this document plans is implemented: the bindgen FFI, the no_std
core (state machine, framing, UDS, nghttp2 callbacks incl. NO_COPY direct
send), the C ABI + header, the protoc plugin, the C++ wrapper, a safe Rust
server API, and the migration guide. Measured size/perf/memory numbers live
in the README and `tests/bench/README.md`; releases in `CHANGELOG.md`.

## 9. Pitfalls / cautions
- panic=abort → no `catch_unwind` → the core must be panic-free.
- System malloc only guarantees alignment up to max_align_t. Over-aligned types
  need `posix_memalign`.
- If `core::fmt` gets pulled in even once, size balloons — check whether
  dependency crates use fmt too.
- The build-std family is nightly and unstable. For long-term maintenance, prefer
  stable no_std first.
