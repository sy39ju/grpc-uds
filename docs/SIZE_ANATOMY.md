<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
# Size anatomy: where grpc++'s 8 MB goes, and why grpcuds is ~82 KB

A self-contained C++ server doing the *same BLE service* is **~8.2 MB with
stock grpc++** and **~82 KB with grpcuds** — ~100× smaller. This is not because
grpcuds implements HTTP/2 more cleverly; it's because **~95% of grpc++ is
distributed-RPC machinery that a local-IPC server never uses**, while grpcuds
delegates HTTP/2 to the system `libnghttp2` and replaces protobuf with nanopb.

Measured on x86-64: the grpc++ binary statically linked (libgrpc++/libgrpc/
libprotobuf/abseil `.a`, `--start-group`), both sides dead-stripped
(`-ffunction-sections -Wl,--gc-sections`), symbol sizes bucketed with `nm -S`.
See [FOOTPRINT.md](FOOTPRINT.md) for the headline sizes.

## The breakdown (grpc++ 8.2 MB → grpcuds 82 KB)

| Layer | grpc++ | grpcuds | how grpcuds avoids it |
| --- | ---: | ---: | --- |
| **HTTP/2 framing** (chttp2 + HPACK) | ~167 KB | ~166 KB | **delegated** to system `libnghttp2.so` (shared, not in the binary) |
| Load balancing + subchannels (xDS, grpcLB, ~8 policies) | ~1.0 MB | 0 | **omitted** — a local-UDS server has no backends to balance |
| `protobuf-full` (50% reflection/descriptors, 8% codec) | ~1.3 MB | ~9 KB | **replaced** by nanopb (wire codec into flat C structs) |
| Call / channel / filter machinery | ~440 KB | — | folded into the ~20 KB core |
| Name resolution (DNS / c-ares) | ~335 KB | 0 | **omitted** — a UDS path needs no resolver |
| Security / credentials plumbing | ~300 KB | 0 | **omitted** — UDS + filesystem permissions, no TLS |
| Typed metadata (`MetadataMap`/`Table` templates ×28 types) | ~283 KB | — | core handles framed bytes, not typed metadata |
| Abseil | ~619 KB | 0 | not used |
| Async runtime (iomgr / closures / EventEngine) | ~137 KB | 0 | runs on the **host's** event loop |
| gpr + grpc++ glue, std, misc | ~1.4 MB | ~13 KB | minimal handler + libstdc++/glibc glue |
| **grpcuds transport core** (no_std + C ABI + mailbox/health) | — | **~20 KB** | the whole transport |
| Binary overhead (rodata, dynamic-link tables, `.eh_frame`, PLT/GOT, PIE) | (amortized) | ~36 KB | unavoidable for any small C++ binary |
| **Total (self-contained binary)** | **~8.2 MB** | **~82 KB** | + a shared `libnghttp2.so` (~166 KB) on the device |

The two largest grpc++ chunks — `grpc_core` (~3.5 MB) and `protobuf` (~1.3 MB) —
break down below. The recurring pattern: **the part that does the actual work is
tiny; the bulk is optional features.**

---

## 1. The HTTP/2 framing is *not* the difference

`grpc_core` is ~3.5 MB (42% of the binary), but the actual HTTP/2 layer is a
sliver:

- **HTTP/2 framing (chttp2 + HPACK): ~167 KB — only 4.8% of grpc_core.**
- That ~167 KB is **the same size as `libnghttp2` (~166 KB)** — a full,
  standalone HTTP/2 implementation. So "a complete HTTP/2 stack is ~166 KB" is
  simply true on both sides; this is *not* where the 8 MB comes from.

The other 95% of `grpc_core` sits *on top of* HTTP/2:

| grpc_core component | size | what it is |
| --- | ---: | --- |
| Load balancing + subchannels | ~974 KB | see §2 |
| Other (channelz, stats, tracing, census, compression, config) | ~770 KB | observability + plumbing |
| Call / channel / filter machinery | ~444 KB | the gRPC call abstraction, filter stack, interceptors |
| Name resolution (DNS, c-ares) | ~335 KB | turn `dns:///host` into addresses |
| Security / credentials plumbing | ~300 KB | auth context, credential machinery (linked even without TLS) |
| Typed metadata (`MetadataMap`/`Table`) | ~283 KB | header handling instantiated over ~28 metadata types → template bloat |
| Async runtime (iomgr / closures / EventEngine) | ~137 KB | gRPC's own event/timer/poller system |
| Service config / retry / hedging | ~65 KB | policy engine |
| HTTP/2 framing (chttp2 / HPACK) | ~167 KB | **the actual protocol** |

grpcuds keeps only the equivalent of that last row — and delegates even it to
`libnghttp2`. The ~20 KB grpcuds core is just the **gRPC framing on top of
nghttp2**: the 5-byte length prefix, `:path` routing, `grpc-status` trailers,
and a one-connection stream state machine.

## 2. The biggest single chunk is xDS — a service-mesh control plane

Load balancing + subchannels is ~1 MB, and most of it is **xDS**:

| LB component | size | what it is |
| --- | ---: | --- |
| **xDS + Envoy API** | **~794 KB** | the Envoy/Kubernetes dynamic-config control plane (LDS/RDS/CDS/EDS, ADS, the xDS client, and the entire Envoy config protobuf API) |
| Subchannel pool + connectivity | ~172 KB | pooling outbound backend connections + a connectivity state machine |
| grpcLB (legacy) + RLS | ~190 KB | more discovery/LB protocols |
| pick_first / round_robin / weighted / ring_hash / priority / outlier / least_request | ~150 KB | the built-in client-side LB policies |

- **Why it's linked even when unused:** gRPC registers every LB policy and
  resolver into a global registry via **static initializers** (a constructor
  that runs at startup and self-registers). The linker cannot dead-strip global
  registration, so `--gc-sections` can't drop it. **Every** grpc binary carries
  xDS + all the LB policies, even a server that only ever talks to one local
  client.
- **Why grpcuds has 0 of it:** it's a server over a local UNIX socket. There are
  no backends to balance, no subchannel pool (no outbound connections), and no
  service discovery (the socket path *is* the address). The whole concept is
  absent, not stripped.

## 3. protobuf is mostly a runtime schema, not a codec

`protobuf-full` is ~1.3 MB, but the part that actually serializes bytes is
small:

| protobuf component | size | in nanopb? |
| --- | ---: | --- |
| **Reflection + descriptors** (runtime schema-as-data) | ~662 KB (50%) | ❌ no |
| Other (extensions, unknown fields, oneof, util) | ~192 KB | ❌ mostly no |
| **Wire codec + IO streams** (encode/decode) | ~110 KB (8%) | ✅ kept — as ~9 KB |
| Repeated-field containers | ~96 KB | ❌ fixed arrays instead |
| `map<k,v>` machinery | ~72 KB | ❌ no |
| Message base (merge / copy / serialize) | ~69 KB | ❌ plain C struct |
| Arena allocator | ~54 KB | ❌ no |
| Text format (`field: value`) | ~47 KB | ❌ no |
| Well-known types (Any / Timestamp / …) | ~5 KB | ❌ no |
| JSON | (dead-stripped here) | ❌ no |

- **protobuf-full** models each message as a **C++ class plus a runtime
  schema** (descriptors) — which is what enables reflection, JSON/text
  conversion, `DynamicMessage`, and gRPC server reflection. That power costs
  ~662 KB of reflection + the whole object model (containers, maps, arenas,
  merge/copy).
- **nanopb** models each message as a **flat C struct plus a tiny compile-time
  field table** (`pb_msgdesc_t`). It does encode/decode and nothing else — no
  runtime schema, no introspection. Even the codec it keeps is ~12× leaner
  (~9 KB vs ~110 KB) because there's no IO-stream abstraction, just structs.
- **What you give up:** runtime field access by name/number, JSON/text output,
  dynamic messages, gRPC reflection, and variable-size fields (nanopb pins them
  to compile-time `max_size`/`max_count`). For an embedded device with a fixed
  `.proto`, none of that is needed.

---

## What grpcuds's 82 KB is actually made of

| part | size | note |
| --- | ---: | --- |
| grpcuds transport core (`no_std` + C ABI + mailbox/health) | ~20 KB | matches the ~22 KB C-embed contribution |
| handler (`server_main.cc`) + libstdc++/glibc glue | ~13 KB | your code + C++ runtime glue |
| nanopb runtime (`pb_encode`/`pb_decode`/`pb_common`) | ~9 KB | the message codec |
| generated BLE stub + message descriptors | ~1 KB | from `protoc-gen-grpcudspp` + nanopb |
| `libnghttp2` | 0 in the binary | dynamic `.so` (~166 KB), shared on the device |
| binary overhead (rodata, dynsym/dynstr, `.eh_frame`, PLT/GOT, PIE) | ~36 KB | proportionally large for a small binary |

## The one-sentence summary

grpc++ is large because it bundles a **full distributed-RPC stack** — its own
HTTP/2, plus service-mesh load balancing (xDS), name resolution, retries,
credentials, observability, and a reflective protobuf object model — and links
all of it (via global self-registration) even into a server that never leaves
one machine. grpcuds keeps the ~5% that a **local-IPC transport** needs
(framing, routing, trailers, streaming), delegates HTTP/2 to `libnghttp2`,
swaps protobuf for nanopb, and omits the rest entirely.

> Numbers are approximate: symbol-size bucketing attributes template
> instantiations and inlined code heuristically, and `.rodata`/string tables are
> not counted in symbol sizes. The shape — HTTP/2 ≈ equal, the 8 MB being
> features-on-top — is robust. Reproduce with the `nm -S --size-sort` method
> above on a statically-linked, dead-stripped grpc++ binary.
