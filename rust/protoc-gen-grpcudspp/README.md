<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
# protoc-gen-grpcudspp

A `protoc` plugin that emits gRPC C++-style service stubs targeting the
**grpcudspp** C++ wrapper over [grpcuds](https://crates.io/crates/grpcuds-core),
with message encode/decode delegated to [nanopb](https://github.com/nanopb/nanopb).

The plugin only generates the service scaffolding (base classes, virtual
handler signatures, dispatch trampolines, `ServerWriter<T>` specializations).
Message layouts and encoding are entirely nanopb's job, so the plugin is
syntax-neutral (proto2/proto3, oneof, nested messages all pass through).

## Install & use

```sh
cargo install protoc-gen-grpcudspp
protoc --grpcudspp_out=. -I proto proto/your_service.proto
```

`protoc` must find the binary on `PATH` (cargo installs it as
`protoc-gen-grpcudspp`). Client-streaming / bidi RPCs are rejected — the
runtime does not support them.
