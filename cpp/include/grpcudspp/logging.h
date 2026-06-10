// SPDX-License-Identifier: MIT OR Apache-2.0
//
// grpcuds logging — severity log with a host-owned sink (gpr_log's shape).
//
// The library never formats: every event is a static message + ONE numeric
// argument (errno / call id / capacity — see grpcuds.h). Unregistered, the
// library is silent. Register once at startup; the sink may fire from the
// I/O thread and from any client thread, so it must be thread-safe.
//
//     // Quickest start — built-in stderr sink:
//     grpcuds::EnableStderrLogging(grpcuds::LOG_INFO);
//
//     // Or route into your own logger:
//     grpcuds::SetLogCallback(
//         [](int lvl, const char* msg, int64_t arg, void*) {
//             my_logger(lvl, msg, arg);
//         },
//         grpcuds::LOG_DEBUG);

#ifndef GRPCUDSPP_LOGGING_H_
#define GRPCUDSPP_LOGGING_H_

#include <cstdint>
#include <cstdio>

#include <grpcuds.h>

namespace grpcuds {

enum LogLevel {
    LOG_ERROR = GRPCUDS_LOG_ERROR,
    LOG_INFO = GRPCUDS_LOG_INFO,
    LOG_DEBUG = GRPCUDS_LOG_DEBUG,
};

/// Register (or with nullptr, remove) the process-global log sink.
/// `fn` must be a plain function or capture-less lambda (C callback).
inline void SetLogCallback(grpcuds_log_fn fn, LogLevel max_level,
                           void* user_data = nullptr) {
    grpcuds_set_log_callback(fn, max_level, user_data);
}

/// The built-in sink: one line per event to stderr —
/// `grpcuds[E] accept failed (arg=13)`. fprintf is thread-safe per POSIX.
inline void EnableStderrLogging(LogLevel max_level = LOG_INFO) {
    SetLogCallback(
        [](int level, const char* msg, int64_t arg, void*) {
            const char tag = level == GRPCUDS_LOG_ERROR  ? 'E'
                             : level == GRPCUDS_LOG_INFO ? 'I'
                                                         : 'D';
            std::fprintf(stderr, "grpcuds[%c] %s (arg=%lld)\n", tag, msg,
                         static_cast<long long>(arg));
        },
        max_level);
}

}  // namespace grpcuds

#endif  // GRPCUDSPP_LOGGING_H_
