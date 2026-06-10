<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
# Building grpcuds — native and cross

How to build the Rust runtime artifacts (`libgrpcuds_ffi.{a,so}` + the
`protoc-gen-grpcudspp` plugin) for the **host** and for a **cross** target
(armv7), in either nghttp2 link mode:

- **dynamic** (default) — link the *system* `libnghttp2` at runtime. This is the
  project invariant: it keeps the binary small (see `CLAUDE.md`).
- **bundled** (opt-in `--features bundled`) — build `libnghttp2` from the pinned
  `rust/grpcuds-sys/vendor/nghttp2-src` submodule (inside the crate, so the published package ships the source) and **statically link** it. Self-contained, no
  runtime `libnghttp2.so` dependency, at a ~100–180 KB size cost.

All `cargo` commands run from the `rust/` workspace directory.

**Server / client split.** `grpcuds-ffi` exposes `server` (default) and
`client` Cargo features that select which C ABI the `.a`/`.so` exports —
build server-only (unchanged), `--no-default-features --features client`
for a client-only library, or `--features client` for both. An embedder
links only the side it uses, so the unused half is never in the binary.

`./build.sh` wraps this with `--side server | client | both` (default
`both`; `--server` / `--client` are shortcuts). The C++ CMake projects
(`cpp/`, `tests/cpp/`) **auto-detect** which halves the linked
`libgrpcuds_ffi.a` exports (via `nm`) and build only the matching targets, so a
server-only or client-only library still configures and builds cleanly. Force
it with `-DGRPCUDS_SIDE=server|client|both`.

## Prerequisites

| | dynamic (default) | bundled (`--features bundled`) |
| --- | --- | --- |
| Rust | stable toolchain (`rust/rust-toolchain.toml` pins it) | same |
| `libclang` | yes — `build.rs` runs bindgen | yes |
| nghttp2 headers + lib | from the sysroot (`libnghttp2-dev`; host sysroot = `/`) — header and lib always come from the same place | built from the in-crate submodule (sysroot ignored) |
| `libnghttp2.so` | needed at **link** and **runtime** (same sysroot as the headers) | not needed |
| CMake + C toolchain | — | yes — builds nghttp2 |
| submodule | — | `git submodule update --init rust/grpcuds-sys/vendor/nghttp2-src` |

The submodule is pinned to nghttp2 **v1.59.0**. In a git checkout, a `bundled`
build auto-initializes it (build.rs runs the documented `git submodule
update --init` for you); published packages ship the source subset outright.

## Native (host x86-64)

### Dynamic (default)

```sh
cd rust
cargo build --release           # whole workspace
# or just the C ABI artifacts:
cargo build --release -p grpcuds-ffi
# -> target/release/libgrpcuds_ffi.a  and  libgrpcuds_ffi.so
```

The dynamic build resolves `<nghttp2/nghttp2.h>` AND `libnghttp2.so` from the
same sysroot (`/` for host builds) so the header can never skew from the lib
you link: install `libnghttp2-dev`. If the headers are missing, the build
fails immediately with instructions instead of producing a mismatched
binary. (docs.rs alone falls back to the submodule's headers — it links nothing.)

### Bundled (static nghttp2)

```sh
cd rust
git submodule update --init grpcuds-sys/vendor/nghttp2-src   # once
cargo build --release -p grpcuds-ffi --features bundled
```

`build.rs` invokes CMake (lib-only, static, no external deps) to produce
`libnghttp2.a`, statically links it, and runs bindgen against the freshly built
headers. The resulting `libgrpcuds_ffi.so` has **no** `libnghttp2.so` dependency.

Verify the link mode:

```sh
ldd target/release/libgrpcuds_ffi.so | grep nghttp2   # dynamic: shows libnghttp2.so.14
                                                       # bundled: prints nothing
```

## Cross-compiling (armv7) — Docker (what CI runs)

A self-contained cross environment ships in `docker/` and runs in CI on every
push/PR (the `armv7` job):

```sh
docker build -t grpcuds-armv7 -f docker/armv7-cross.Dockerfile docker/
docker run --rm -v "$PWD":/src:ro -v /tmp/armv7-out:/out grpcuds-armv7 \
    /src/docker/armv7-build.sh
```

The image bundles the `arm-linux-gnueabihf` GCC toolchain, an ubuntu-ports
armhf sysroot with `libnghttp2` (dynamic link — the project invariant), and
`qemu-user`. The script builds all three `--side` variants of
`libgrpcuds_ffi` (the cargo cross flow end to end), measures the C-embed
size contributions, then cross-builds the BLE example with the CMake
toolchain file and runs it under `qemu-arm` to a passing round-trip.
Sizes + artifacts land in `/out`.

Validation on real device hardware (your actual device sysroot) remains a
release step — the sections below cover pointing the build at your own
toolchain.

## Cross-compiling (armv7) — your own toolchain

The cross toolchain, sysroot, and linker are **environment-specific** — install
and point at them yourself. `rust/.cargo/config.toml` has the target block (cross
linker + sysroot link-arg); fill in the two placeholders for your setup.

```sh
rustup target add armv7-unknown-linux-gnueabihf      # once
```

### Dynamic (default)

Resolve `<nghttp2/nghttp2.h>` (and the libc headers it pulls in) from the cross
sysroot via `SYSROOT`, which `build.rs` passes to clang as `--sysroot` /
`--target`:

```sh
cd rust
export SYSROOT=/abs/path/to/armv7-sysroot            # bindgen + link
cargo build --release --target armv7-unknown-linux-gnueabihf -p grpcuds-ffi
```

The sysroot must contain the `libnghttp2-dev` headers and `libnghttp2.so.14`
(under `"$SYSROOT"/usr/lib/`) — it is the dynamic-link target. See
`rust/bindings/armv7/README.md` for the sysroot checklist.

### Bundled (static nghttp2)

The `cmake` crate cross-builds nghttp2 using your cross C compiler. Tell it which
one (`CC_<target>` or a `CC` it recognizes), and still export `SYSROOT` so
bindgen's clang resolves the target libc headers:

```sh
cd rust
git submodule update --init grpcuds-sys/vendor/nghttp2-src
export SYSROOT=/abs/path/to/armv7-sysroot
export CC_armv7_unknown_linux_gnueabihf=arm-linux-gnueabihf-gcc
cargo build --release --target armv7-unknown-linux-gnueabihf \
    -p grpcuds-ffi --features bundled
```

`build.rs` adds `--target=armv7-...` so bindgen emits 32-bit struct layouts, and
links the cross-built `libnghttp2.a`. No system `libnghttp2.so` is needed in the
sysroot in this mode (only the libc headers for bindgen).

## Cross-compiling with a CMake toolchain file

Every CMake step in the project can take a toolchain file instead of ad-hoc
`CC_*` exports. A reusable, environment-driven one ships at
[`cmake/armv7-linux-gnueabihf.cmake`](../cmake/armv7-linux-gnueabihf.cmake):
it sets the cross compilers from `CROSS_PREFIX` (default
`arm-linux-gnueabihf-`; `TOOLCHAIN_DIR` locates the tools when they are not on
`PATH`), `CMAKE_SYSROOT` + pkg-config lookup from `SYSROOT`,
and restricts `find_*` to the sysroot while keeping host *tools* (protoc, the
grpcudspp plugin, `nanopb_generator`) native.

**Flow A — the `bundled` nghttp2 build inside cargo.** The `cmake` crate
honors a target-scoped toolchain-file env (checked in this order:
`CMAKE_TOOLCHAIN_FILE_<triple>`, `CMAKE_TOOLCHAIN_FILE_<triple with _>`,
`TARGET_CMAKE_TOOLCHAIN_FILE`, `CMAKE_TOOLCHAIN_FILE`), so instead of the
`CC_*` exports above:

```sh
cd rust
export SYSROOT=/abs/path/to/armv7-sysroot          # bindgen still wants this
export CMAKE_TOOLCHAIN_FILE_armv7_unknown_linux_gnueabihf=\
$PWD/../cmake/armv7-linux-gnueabihf.cmake
cargo build --release --target armv7-unknown-linux-gnueabihf \
    -p grpcuds-ffi --features bundled
```

**Flow B — cross-building a C++ server (e.g. `tests/cpp`).** Codegen
runs on the host; only the compile/link is cross. Build the target runtime
and the host plugin first, then point CMake at the toolchain file and the
target-arch artifacts:

```sh
# 1. host plugin + target runtime lib
cd rust && cargo build --release -p protoc-gen-grpcudspp
SYSROOT=/abs/armv7-sysroot cargo build --release \
    --target armv7-unknown-linux-gnueabihf -p grpcuds-ffi
cd ..

# 2. cross-configure the server: toolchain file + target .a + host tools
export SYSROOT=/abs/armv7-sysroot                  # read by the toolchain file
cmake -S tests/cpp -B build-armv7 \
  -DCMAKE_TOOLCHAIN_FILE=$PWD/cmake/armv7-linux-gnueabihf.cmake \
  -DGRPCUDS_FFI=$PWD/rust/target/armv7-unknown-linux-gnueabihf/release/libgrpcuds_ffi.a
  # nanopb auto-found at example/nanopb (override with -DNANOPB_DIR=...)
cmake --build build-armv7
```

`GRPCUDSPP_PLUGIN` defaults to the host build from step 1; override it with
`-DGRPCUDSPP_PLUGIN=/abs/path` if yours lives elsewhere. The nanopb runtime
(`pb_*.c`) is compiled by the cross compiler as part of the server target —
nothing prebuilt is needed for it. For a different triple, copy the
toolchain file and adjust `CMAKE_SYSTEM_PROCESSOR` / the default prefix.

## pkg-config for consumers

After installing the artifacts, generate a `grpcuds.pc` matching the link mode so
C/C++ consumers can `pkg-config --cflags --libs grpcuds`:

```sh
# dynamic: nghttp2 is a Requires.private on the system libnghttp2
./scripts/gen-pkgconfig.sh --prefix /usr/local \
    -o /usr/local/lib/pkgconfig/grpcuds.pc

# bundled: the static nghttp2 archive is named directly, no system .pc required
./scripts/gen-pkgconfig.sh --prefix /opt/grpcuds --bundled \
    -o /opt/grpcuds/lib/pkgconfig/grpcuds.pc
```

Template: `rust/grpcuds-ffi/pkgconfig/grpcuds.pc.in`.

## nanopb (message codegen)

Message encode/decode is nanopb's job (the Rust runtime only moves framed
bytes). nanopb is a **pinned submodule** at `example/nanopb` @ `0.4.9.1`:

```sh
git submodule update --init example/nanopb
# The generator's only python dependency is protobuf. PEP-668 distros
# (Debian 12+ / Ubuntu 23.04+) block bare `pip3 install --user`, so use
# the distro package — or any venv with `pip install protobuf`.
sudo apt install python3-protobuf
```

Both the **generator** (`generator/nanopb_generator.py`) and the **runtime**
(`pb_decode.c` / `pb_encode.c` / `pb_common.c`) come from that one checkout, so
they cannot drift to mismatched versions. The build wiring (`tests/cpp`)
prefers the in-tree generator under `NANOPB_DIR` over any `nanopb_generator` on
`PATH`. It self-bootstraps its
proto bindings using the python `protobuf` module + a protoc (the same system
`protoc` used for the service-stub plugin; `grpcio-tools` also works).

## Build the protoc plugin

`protoc-gen-grpcudspp` is a host build tool (runs on your build machine, emits
C++); build it for the host regardless of the target you ship to:

```sh
cd rust && cargo build --release -p protoc-gen-grpcudspp
# -> target/release/protoc-gen-grpcudspp
```

## Wire logging for Wireshark (dev only)

`--wirelog` compiles in a packet capture of everything that crosses the
socket — **off by default, zero code or cost in normal builds, never ship
it**. UDS traffic has no TCP/IP framing, so each chunk is wrapped in a
synthetic IPv4+TCP header on **port 80** — the port Wireshark's HTTP
dissector owns by default, so it spots the HTTP/2 connection preface and
dissects HTTP/2 → gRPC with no "Decode As" step (filter: `http2` or
`grpc`).

```sh
./build.sh lib --wirelog          # C ABI runtime with capture compiled in
cargo build --features wirelog    # or any Rust crate (grpcuds / grpcuds-ffi)

# Compiled in ≠ enabled: capture only happens when the env var is set.
GRPCUDS_WIRELOG=/tmp/grpcuds.pcap ./your-server
wireshark /tmp/grpcuds.pcap
```

Both the server and the client side log when built with the feature; in a
single process each connection shows up as its own fake TCP stream
(client ports 40000+, server port 80).

Files rotate at **1 MiB** into `<path>.1` and `<path>.2` (oldest dropped):
at most 3 files / 3 MiB on disk including the live one. Both knobs are
environment-tunable:

```sh
GRPCUDS_WIRELOG_FILE_KB=4096   # per-file cap in KiB   (default 1024, 4..=1048576)
GRPCUDS_WIRELOG_FILES=5        # total files incl. live (default 3, 1..=10)
```

Two caveats:

- A rotated file starts mid-stream (no HTTP/2 preface), so for `.1`/`.2`
  use *Analyze → Decode As → TCP port 80 → HTTP2*.
- Rotation renames are per-process: when server and client run as separate
  processes, point each at its own path.
