// SPDX-License-Identifier: MIT OR Apache-2.0
//
// BLE — grpcuds server binary. Driven by the Rust tonic-client peer (the `gt`
// row). Prints READY once bound, then runs the shared poll loop.
#include <unistd.h>

#include <string>

#include "ble_service_impl.h"
#include "poll_loop.h"

int main(int argc, char** argv) {
    std::string path = argc > 1 ? argv[1] : "/tmp/grpcuds-ble-gt.sock";
    ::unlink(path.c_str());
    BleServiceImpl svc;
    return grpcuds_ex::run_poll_loop(path, &svc, []() {});
}
