# SPDX-License-Identifier: MIT OR Apache-2.0
#
# Root aggregator: one configure/build/ctest for the whole C++ side —
# the grpcudspp wrapper + its tests (cpp/) and the example matrix
# (tests/cpp/). Both also remain standalone projects.
#
# The Rust artifacts must exist first (the subprojects error with
# instructions if not):
#
#     ./build.sh            # libgrpcuds_ffi.a (server+client) + protoc plugin
#     cmake -S . -B build && cmake --build build && ctest --test-dir build
#
cmake_minimum_required(VERSION 3.16)
project(grpcuds CXX C)

enable_testing()

# The header-only C++ wrapper + its ctest suite.
add_subdirectory(cpp)

# The example matrix needs the nanopb submodule (or -DNANOPB_DIR); default the
# option to whether it's resolvable so a fresh clone still configures.
if(NANOPB_DIR OR DEFINED ENV{NANOPB_DIR}
   OR EXISTS "${CMAKE_CURRENT_SOURCE_DIR}/example/nanopb/pb_decode.h")
    set(_grpcuds_examples_default ON)
else()
    set(_grpcuds_examples_default OFF)
endif()
option(GRPCUDS_BUILD_EXAMPLES "Build the tests/cpp interop matrix"
       ${_grpcuds_examples_default})

if(GRPCUDS_BUILD_EXAMPLES)
    add_subdirectory(tests/cpp)
    # The examples (they also ship at the dist bundle root) — built here so
    # they can never rot. Same nanopb prerequisite.
    add_subdirectory(example/ble)
    add_subdirectory(example/c)
else()
    message(STATUS "tests/cpp + examples skipped — init the nanopb submodule "
                   "to enable: git submodule update --init example/nanopb")
endif()
