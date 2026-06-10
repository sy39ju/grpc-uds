# grpcuds vs tonic — UDS benchmark

Head-to-head over the same `proto/ble.proto`, driven by the same tonic client
over a UNIX domain socket:

- `grpcuds_server` — the safe Rust wrapper (`rust/grpcuds`), single-threaded
  poll loop, prost encode/decode in byte-level handlers.
- `tonic_server` — tonic 0.12 with its production defaults (tokio
  multi-thread), `serve_with_incoming` on a `UnixListener`.

Both build in this crate's single `release` profile (`lto`, `codegen-units=1`,
`strip`) so binary sizes and codegen are directly comparable.

```sh
cargo build --release
./target/release/runner          # prints a markdown table
BENCH_STREAM_N=200000 ./target/release/runner   # bigger stream burst
```

Measured per server: unary p50/p99/mean + RPS (sequential, one connection),
streaming msgs/s + payload MB/s (best of 3 drains of one
`ScanResultStream` call), VmRSS after the unary phase and after the stream
burst, and the stripped binary size. Current results live below, in the
"vs tonic (measured)" section.

The **client side** of the burst is measured by two extra bins that drain the
identical 50k-msg stream (decoding every message; a checksum proves the drains
match) and self-report PSS / VmRSS / VmHWM. The grpcuds blocking client shows
VmHWM == settled RSS — it never balloons mid-burst, because it reads ≤16 KB per
pump only when its message queue is empty:

```sh
./target/release/grpcuds_server /tmp/b.sock &
./target/release/grpcuds_client_burst /tmp/b.sock   # ≈2.3 MB RSS after 50k msgs
./target/release/tonic_client_burst   /tmp/b.sock   # ≈4.4 MB RSS after 50k msgs
```

This is a **standalone workspace** on purpose: tonic's ~75-crate dependency
closure stays out of the library workspace and out of its `cargo-deny` gate.

## vs tonic / grpc++ (measured)

grpcuds trades gRPC features that local IPC doesn't need (the ✗ rows in the root
README's Features table) for footprint. The headline of what that buys: a ~4×
smaller server binary and ~2× smaller RSS/PSS than tonic over UDS, with unary
latency at parity and a deliberate ~1.1–1.6× streaming-ceiling gap (one I/O
thread); and two orders of magnitude smaller than a stock grpc++ C++ server.

This crate's `runner` (`cd tests/bench && cargo build --release &&
./target/release/runner`) and the `*_client_burst` bins produce the
latency / throughput / RSS rows; `./measure_tables.sh` produces the size +
PSS comparison. **All of those numbers, kept per version, live in one place:
[`../../docs/FOOTPRINT.md`](../../docs/FOOTPRINT.md).** If you need full-featured
gRPC rather than the footprint, **use tonic — it is the better general-purpose
stack.**

## Findings & analysis

**Syscall profile** (strace, one 200k-message stream): grpcuds issues ~756
`write` + ~525 `poll` on one thread — the outbound queue packs ~290 small
messages per 16 KB HTTP/2 DATA frame — vs tonic's ~6.3k `writev` + ~6.1k
`epoll_wait` + futexes across 32 threads. grpcuds is ~10× more
syscall-efficient; the remaining throughput gap is user-space per-message
cost.

**Where the per-message cost lives**: tonic's `Bytes` pipeline moves
refcounted buffers end to end; grpcuds pays one encode allocation plus one
copy into nghttp2's frame buffer (its C API pulls data by copy).
`write_owned` / `MessageWriter::send` move bytes through the mailbox into
the outbound queue without intermediate copies, and messages ≥ 4 KB skip
nghttp2's buffers entirely (`NO_COPY` DATA frames `writev`'d straight from
the queue to the socket — a measured ~2–4% at 16 KB, where saved memcpys
trade against per-frame syscalls; small messages deliberately keep the
aggregating copy path).

**One I/O thread by design**: the streaming ceiling is one core's worth of
that user-space per-message cost — tonic spreads the same drain across a
32-thread tokio runtime. Embedded targets don't have the cores to spend,
and the single-threaded core is what keeps the library `no_std`-small and
lock-free; producers hand off through the outbound mailbox instead of
taking locks in the hot path (`docs/THREADING.md`).

**Why the gap is accepted**: the ceiling is millions of msgs/s; the target
workloads (BLE scan results, sensor events) are 10–100 msgs/s — five
orders of magnitude below it. Unary latency, the metric local IPC actually
feels, is at parity.

**Memory behavior after bursts**: the queue drains immediately, but glibc
retains the freed memory in its arena (and its *dynamic mmap threshold*
makes the second burst land in sbrk space — a one-time jump, then flat:
not a leak). Remedies, in order: bound the queue with `set_backpressure`
(caps the high-water itself); call `malloc_trim(0)` on idle
(`BENCH_TRIM_SEC=1` demonstrates: 7.9 MB high-water returns to ~450 KB);
or pin `GLIBC_TUNABLES=glibc.malloc.mmap_threshold=131072`.

**Flat memory by design**: a long-lived connection is the deployment
shape, so nothing on it may scale with call count — the per-call stream
context is dropped the moment nghttp2 closes the stream, and nghttp2's own
closed-stream retention is disabled (`no_closed_streams`; the priority
tree is unused). Effect: server RSS holds at ≈2.0 MB from 1k to 320k
sequential calls, and per-message stream lookups stay O(live streams).
This harness is the regression net for that property; `memcheck.sh`
additionally runs the stack under valgrind, and `helgrind.sh` runs a dedicated
C harness (producer threads hammering the C-ABI outbound mailbox + a
mid-flight connection free) under the race detector — both wired into CI on
main pushes.

**C-embed sizing** is the stronger story than the Rust-vs-Rust table: just the
`.text` added to an existing C app, vs linking a Rust runtime at all — see the
deep dive below.

## Size & memory deep dive

The structural *why* behind the figures, which live with their exact,
per-version values in
[`../../docs/FOOTPRINT.md`](../../docs/FOOTPRINT.md). All measured on x86-64,
stripped, `panic="abort"` + `no_std`, `libnghttp2` linked dynamically (a static
`--features bundled` would add ~100–180 KB).

**The C ABI contribution** is measured by linking a minimal C program that
references only one side's entry symbols against `libgrpcuds_ffi.a`
(`-ffunction-sections -fdata-sections -Wl,--gc-sections -s`) and comparing to
an empty-`main` baseline (`./measure_footprint.sh`). The C ABI splits into
`server` (default) and `client` Cargo features (`./build.sh --side
server|client|both`); the unused half is never compiled in — a server-only
build is byte-identical with or without the client feature available. (The
probe's *file* delta quantizes at 4 KB page boundaries — it once read the
client as "4 KB" purely because its text fit the baseline's padding — so the
section-sum is the primary figure.) The server contribution is the figure that
defines the project: it is the `.text` added to an *existing* C/C++ application,
which links `libstdc++` dynamically and so pays nothing for the standard library
— `libgrpcuds_ffi.a` + a header is the embedded path. The client side is smaller
still: no dispatch, backpressure, or stream state machine.

**The Rust `std` floor.** A standalone Rust binary is a different size class,
and the difference is structural, not ours: Rust statically links `std` (a fixed
floor dominated by the standard library, not by us), while C++ borrows
`libstdc++.so` from the OS — which is why the same standalone server/client in
C++ lands far smaller *while containing more code*. The floor cannot be removed
while `std` is linked statically: going below it needs dynamic `std` (a
per-rustc-version `.so` on the target — impractical for devices) or nightly
`build-std`, which this project avoids by design. So pitch the **C ABI path** on
binary size (no Rust std in the binary; link only the side you use), and the
**standalone Rust** path on dependency weight (4 crates vs tonic's 75) and
memory, not on binary size.

**Runtime memory** is flat by design (see "Flat memory by design" above): a
small per-connection heap cost (nghttp2 session + stream state + a small
outbound queue, measured as an RSS delta with connections held mid-stream via
`conn_hold` in `measure_footprint.sh`) on top of a steady-state RSS that does
not grow with call count. Exact heap / RSS / PSS figures, and the per-language
size + PSS tables vs tonic and stock grpc++:
[`../../docs/FOOTPRINT.md`](../../docs/FOOTPRINT.md).
