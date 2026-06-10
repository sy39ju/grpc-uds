// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Compile-strictness probe for the C-mode generator: edge.proto's generated
// stubs must build as C11 under -Wall -Wextra -Werror, with a small
// GRPCUDSC_MAX_MESSAGE_SIZE override (set by CMake). References every
// generated entry point so nothing is dead-stripped before it compiles.
#include <stdio.h>

#include "edge.grpcuds.h"

static int echo_h(grpcuds_call_ref ref, const Ping* req, Ping* resp, void* ud) {
    (void)ref;
    (void)ud;
    resp->n = req->n;
    return GRPCUDS_OK;
}

int main(void) {
    First_service first;
    Second_service second;
    first.user_data = NULL;
    first.Echo = echo_h;
    first.Nested = NULL;
    first.Watch = NULL;
    second.user_data = NULL;
    second.Echo = NULL;

    // Reference (not run) the full generated surface so every static
    // inline is instantiated and every extern symbol must link.
    void (*volatile fns[])(void) = {
        (void (*)(void))First_register,
        (void (*)(void))Second_register,
        (void (*)(void))First_Echo,
        (void (*)(void))First_Nested,
        (void (*)(void))First_Echo_respond,
        (void (*)(void))First_Nested_respond,
        (void (*)(void))First_Watch_send,
        (void (*)(void))First_Watch_finish,
        (void (*)(void))First_Watch_start,
        (void (*)(void))First_Watch_next,
        (void (*)(void))Second_Echo,
        (void (*)(void))Second_Echo_respond,
    };
    (void)fns;
    (void)first;
    (void)second;
    printf("edge-cgen-compile: OK (scratch=%d)\n", GRPCUDSC_MAX_MESSAGE_SIZE);
    return 0;
}
