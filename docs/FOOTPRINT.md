<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
# Footprint & memory

**The single source of truth for grpcuds' measured size and memory numbers.**
Every other doc (README, DESIGN, MIGRATING, the `tests/*` READMEs) points
here instead of quoting figures inline, so a re-measurement is a one-file
update.

Reproduce any of the tables below:

```sh
tests/bench/measure_footprint.sh   # C-embed probes, std floor, example
                                   #   binaries, heap / RSS / PSS
tests/bench/measure_tables.sh      # grpcuds vs grpc++ / tonic, size + PSS
cd tests/bench && ./target/release/runner   # latency / throughput
```

## How history is kept

Numbers are recorded **per measurement run, newest first**. The top `##`
entry is current — that is what the rest of the docs describe. When you
re-measure (a release, a new target, a size-affecting change), **prepend a
new `## <version> — <date> (<arch>)` section**; never edit an older one.
That way the footprint's evolution across versions stays visible.

---

## v0.1.0 — 2026-06-12 (x86-64)

`panic="abort"` + `no_std`, after link + strip, `libnghttp2` linked
dynamically. Measured on x86-64.

### C ABI contribution to a host C/C++ app

Probe method: link a minimal C program referencing one side's entry symbols
against the side-only `libgrpcuds_ffi.a`
(`-Os -ffunction-sections -fdata-sections -Wl,--gc-sections -s`) and take the
delta over an empty-`main` baseline (~14 KB).

| side | code size | binary delta |
| --- | ---: | ---: |
| **server** contribution | **~22 KB** (22,399 B section-sum) | 20,856 B |
| **client** contribution | **~15 KB** (15,305 B section-sum) | 12,472 B |
| C++ wrapper (header-only) | 0 KB | 0 B |

The server figure includes the always-on thread-safe outbound mailbox
(pthread_mutex + queue + eventfd); it added ~2 KB over the pre-mailbox ~20 KB.

### Standalone binaries (stripped)

The C++ binaries are **dead-stripped** (`-ffunction-sections -fdata-sections
-Wl,--gc-sections`, as the examples and `tests/cpp` build) — the realistic
embedded link, so a client does not carry the server-only mailbox/health.

| binary | size | note |
| --- | ---: | --- |
| **C++** server / client (`example/ble`, MinSizeRel, section-sum) | ~68 KB / ~44 KB | 68,438 / 44,322 B |
| **Rust** server / client | ~352 KB / ~327 KB | ≈278 KB is Rust's static `std` floor |
| Rust `std` floor (`fn main(){}`, same size profile) | ~278 KB | 284,784 B |

### Memory

| metric | value |
| --- | ---: |
| Heap per active connection | ~16 KB |
| Server idle RSS (Rust core), steady state | ~2.0 MB (flat vs call count) |
| Server idle PSS (Rust core) | ~780 KB |
| C++ server (`example/ble`) idle RSS / PSS | ~3.8 MB / ~370 KB |
| C++ server at 100 concurrent connections | ~5.4 MB RSS / ~2.7 MB PSS |

### grpcuds (C++) vs stock grpc++ (C++)

Apple-to-apple, same service logic. grpc++ is linked **statically**
(`libgrpc++.a` / `libgrpc.a` / `libprotobuf.a` / `libabsl_*.a` via
`-Wl,--start-group`; openssl left as the system `.so`) so it is comparable to
grpcuds' self-contained binary. Linked *dynamically* grpc++ is only a ~280 KB
app stub but then needs **~17 MB** of grpc/protobuf/abseil shared libraries
resident on the device. grpcuds C++ statically links its ~20 KB core and links
the system `libnghttp2` (~166 KB) dynamically.

**Binary size (stripped, statically linked, dead-stripped both sides):**

| role | grpcuds (C++) | grpc++ (C++) |
| --- | ---: | ---: |
| ble server   | 82 KB | 8240 KB |
| ble client   | 54 KB | 8012 KB |
| agent server | 86 KB | 8228 KB |
| agent client | 58 KB | 7980 KB |

**Idle PSS (one connection, same static binary):**

| role | grpcuds (C++) | grpc++ (C++) |
| --- | ---: | ---: |
| ble server   | ~375 KB | ~7.5 MB |
| ble client   | ~356 KB | ~7.7 MB |
| agent server | ~388 KB | ~7.5 MB |
| agent client | ~373 KB | ~7.2 MB |

A stock grpc++ server is ~8.2 MB of code and idles at ~7.5 MB PSS; the grpcuds
C++ server doing the identical job is ~82 KB and idles at ~0.38 MB —
**~100× smaller on disk, ~20× in memory.**

Where does grpc++'s 8 MB actually go (HTTP/2 is only ~4.8% of it; ~0.8 MB is
xDS service-mesh code), and how grpcuds avoids it layer by layer? See
[SIZE_ANATOMY.md](SIZE_ANATOMY.md).

### grpcuds (Rust) vs tonic (Rust)

Same role + same service logic, `opt-level="z"` + fat LTO + strip. X.509 is
excluded (real cert agent vs mock).

| role | grpcuds size | tonic size | grpcuds PSS | tonic PSS |
| --- | ---: | ---: | ---: | ---: |
| ble server   | 352 KB | 981 KB | ~430 KB | ~1050 KB |
| ble client   | 327 KB | 851 KB | ~445 KB |  ~890 KB |
| agent server | 353 KB | 962 KB | ~415 KB | ~1075 KB |
| agent client | 321 KB | 854 KB | ~440 KB |  ~925 KB |

grpcuds is **~3× smaller on disk and ~2–2.5× smaller in PSS** than tonic. Both
pay the ~278 KB statically-linked `std` floor; the difference is the transport
— grpcuds links system `libnghttp2` dynamically and adds only its small core,
while tonic statically links the `tonic`/`tokio`/`hyper`/`h2` stack.

### Latency / throughput

Not pinned here. grpcuds runs on a single I/O thread, so its timing is sensitive
to whatever else shares the cores — a number measured on a shared/loaded host is
not reproducible and tends to understate it.

Instead it is **tracked in CI**: the `bench` job (`.github/workflows/ci.yml`,
on every push to `main`) runs `tests/bench/runner`, posts the full
grpcuds-vs-tonic table to the GitHub **run summary** (look there when you want
current numbers), and gates on the grpcuds/tonic **ratio** rather than absolute
values — the same noise hits both stacks on a shared runner, so the ratio is the
robust signal. For real absolute numbers, run the same `runner` on a
**dedicated, idle machine** (or the target device). Harness + methodology:
[`tests/bench/README.md`](../tests/bench/README.md).

The sizes and per-connection memory above are deterministic and stand regardless
of host load.
