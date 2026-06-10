<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
# BLE scanner — the grpcuds showcase (C++)

A complete grpcuds⇄grpcuds service in two files: a BLE central exposed over
gRPC — adapter init, advert scanning as a server-stream, and a GATT
characteristic read. The radio is **simulated** (a fixed device table swept
on a timer); swap the producer for your platform's BLE API and the contract
stays the same.

| file | what it is |
| --- | --- |
| `proto/ble.proto` + `.options` | the contract + nanopb buffer caps |
| `server_main.cc` | the whole server: unary handlers + the streaming **producer thread** |
| `client_main.cc` | the whole client: generated `NewStub`, drives every RPC, self-checking |

```sh
../../build.sh                              # once: libgrpcuds_ffi.a + protoc plugin
cmake -S . -B build && cmake --build build
./run_demo.sh build                         # scan round-trip -> "example: OK"
```

## Codegen (C++)

Two passes, both run by CMake — here is what it runs (`plugin` = the
`protoc-gen-grpcudspp` from `../../build.sh`):

```sh
# 1. nanopb — message structs (ble.pb.{h,c})
nanopb_generator -D gen -I proto -f proto/ble.options proto/ble.proto

# 2. protoc-gen-grpcudspp, C++ MODE (default — no --grpcudspp_opt) —
#    the service base class + typed NewStub
protoc --plugin=protoc-gen-grpcudspp="$plugin" \
       --grpcudspp_out=gen \
       --proto_path=proto proto/ble.proto
# -> gen/ble.grpc.pb.h / ble.grpc.pb.cc  (ble::BleScanner::Service base +
#                                         NewStub, over the nanopb structs)
```

The plain-C example (`../c`) runs the same two passes with
`--grpcudspp_opt=c` instead, emitting `ble.grpcuds.{h,c}`.

Cross-compiling (e.g. armv7): `cmake/armv7-linux-gnueabihf.cmake` ships next
to this file — `export SYSROOT=...; cmake -S . -B build-armv7
-DCMAKE_TOOLCHAIN_FILE=cmake/armv7-linux-gnueabihf.cmake -DGRPCUDS_FFI=<target .a>`
(see `docs/BUILDING.md`; the docker armv7 CI job runs exactly this).

What it demonstrates (each marked in the source):

- **The streaming shape.** `ScanResultStream` returns immediately and hands
  the `ServerWriter` to a radio thread — `Write()`/`Finish()` are
  thread-safe mailbox calls. This is the one structural difference from
  grpc++ (whose handlers block and loop); see `docs/THREADING.md`.
- **Typed stubs on both sides.** The server implements the generated
  `ble::BleScanner::Service` base; the client drives the generated
  `NewStub` — the stock gRPC C++ shape over nanopb structs.
- **nanopb capacities.** `proto/ble.options` pins every variable-length
  field (MAC, name, adv payload); the handlers use plain struct fields.
- **Error paths.** An unknown device returns `NOT_FOUND` through the real
  wire trailers, asserted by the client.
- **Standard health checking.** The server registers
  `grpcuds::health::HealthService` (`<grpcudspp/health.h>`); the client
  probes `grpc.health.v1/Check` — incl. the spec'd `NOT_FOUND` for unknown
  services — before driving the scanner. Stock probers work unmodified.

The wire is real gRPC over a UNIX socket: any stock client (tonic, grpc++,
grpcurl) can dial the server's socket with `ble.proto` in hand. This same
directory ships verbatim at the SDK bundle root (the CMakeLists auto-detects
which layout it sits in — there it builds from `../sdk/proto/ble.proto`).
The 3×3 interop test matrix lives under `/tests`.
