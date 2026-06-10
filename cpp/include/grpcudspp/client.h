// SPDX-License-Identifier: MIT OR Apache-2.0

/// \file client.h
/// \brief Header-only C++ client over the grpcuds client C ABI.
///
/// Mirrors the gRPC C++ client shape over a UNIX domain socket. Build
/// `grpcuds-ffi` with the `client` feature — server-only builds do not export
/// the `grpcuds_client_*` symbols this header calls.
///
/// \code
/// grpcuds::Client client("/run/echo.sock");
/// if (!client) { /* connect failed */ }
///
/// echo_HelloReply reply = echo_HelloReply_init_zero;
/// grpcuds::Status s = client.Unary("/echo.Echo/Hello", request,
///                                  echo_HelloRequest_fields, &reply,
///                                  echo_HelloReply_fields);
/// \endcode
///
/// Messages are nanopb structs, encoded/decoded exactly like the server side.
/// The typed `Unary` / `ServerStreaming` overloads (and `ClientReader`) compile
/// only when nanopb is on the include path (`GRPCUDSPP_HAVE_NANOPB`); the
/// byte-level `*Raw` API needs no codec.

#ifndef GRPCUDSPP_CLIENT_H_
#define GRPCUDSPP_CLIENT_H_

#include <cstdint>
#include <string>
#include <vector>

#include "grpcuds.h"
#include "status.h"

// Typed (nanopb) methods are compiled only when nanopb is on the include
// path; the byte-level API needs no codec.
#if defined(__has_include)
#  if __has_include(<pb_decode.h>)
#    include <pb_decode.h>
#    include <pb_encode.h>
#    define GRPCUDSPP_HAVE_NANOPB 1
#  endif
#endif

/// Scratch buffer size (bytes) each call uses to encode/decode one message.
/// Override before including this header for messages larger than 1 KB.
#ifndef GRPCUDSPP_MAX_MESSAGE_SIZE
#define GRPCUDSPP_MAX_MESSAGE_SIZE 1024
#endif

namespace grpcuds {

#ifdef GRPCUDSPP_HAVE_NANOPB
/// Reader over a server-streaming response: call Read() until it returns
/// `false`, then inspect status().
///
/// Owns the underlying stream and frees it on destruction. Move-only.
/// \tparam Resp the nanopb message type each frame decodes into.
template <typename Resp>
class ClientReader {
 public:
  /// Wrap an owned `grpcuds_stream` plus the nanopb descriptor for `Resp`.
  /// Takes ownership of `stream` (freed in the destructor).
  ClientReader(grpcuds_stream* stream, const pb_msgdesc_t* fields)
      : stream_(stream), fields_(fields) {}
  /// Move-construct, transferring stream ownership; the source is left empty.
  ClientReader(ClientReader&& o) noexcept
      : stream_(o.stream_), fields_(o.fields_) {
    o.stream_ = nullptr;
  }
  ClientReader(const ClientReader&) = delete;
  ClientReader& operator=(const ClientReader&) = delete;
  ~ClientReader() {
    if (stream_) grpcuds_stream_free(stream_);
  }

  /// Decode the next message into `out`.
  /// \param out destination for the decoded message.
  /// \return `true` if a message was decoded; `false` at end of stream (then
  ///   call status()) or on a decode error.
  bool Read(Resp* out) {
    if (!stream_) return false;
    size_t len = 0;
    const uint8_t* p = grpcuds_stream_next(stream_, &len);
    if (!p) return false;
    pb_istream_t is = pb_istream_from_buffer(p, len);
    return pb_decode(&is, fields_, out);
  }

  /// The final gRPC status, valid once Read() has returned `false`.
  Status status() const {
    return Status(static_cast<StatusCode>(
        stream_ ? grpcuds_stream_status(stream_) : UNKNOWN));
  }

 private:
  grpcuds_stream* stream_;
  const pb_msgdesc_t* fields_;
};
#endif  // GRPCUDSPP_HAVE_NANOPB

/// A blocking gRPC client over a UNIX domain socket.
///
/// Mirrors the gRPC C++ client shape; one call is in flight at a time. Connect
/// with the constructor, check operator bool, then issue unary or
/// server-streaming calls. Move-only; closes the connection on destruction.
class Client {
 public:
  /// Connect to the grpcuds (or any stock gRPC) server listening on the UDS
  /// `path`. Check operator bool to detect a failed connect.
  ///
  /// If a later call finds the connection dead (server restarted), the
  /// client makes ONE lazy reconnect attempt to the same path before that
  /// call — stock-gRPC IDLE-channel style; a failed reconnect fails the
  /// call immediately.
  explicit Client(const std::string& path)
      : conn_(grpcuds_client_connect(path.c_str())) {}

  /// Connect, retrying with exponential backoff (50 ms × 1.6 up to a 1 s
  /// cap, ±20% jitter; each attempt itself bounded at 250 ms) until
  /// `connect_timeout_ms` elapses — covers the daemon-startup race
  /// (socket file absent, or present before listen()). The rough equivalent
  /// of stock gRPC's wait_for_ready, scoped to connection establishment.
  /// `connect_timeout_ms == 0` makes exactly one attempt.
  Client(const std::string& path, uint32_t connect_timeout_ms)
      : conn_(grpcuds_client_connect_wait(path.c_str(), connect_timeout_ms)) {}
  ~Client() {
    if (conn_) grpcuds_client_free(conn_);
  }
  Client(const Client&) = delete;
  Client& operator=(const Client&) = delete;
  /// Move-construct, transferring the connection; the source is left empty.
  Client(Client&& o) noexcept : conn_(o.conn_) { o.conn_ = nullptr; }

  /// `true` if the connection was established.
  explicit operator bool() const { return conn_ != nullptr; }

  /// Per-call timeout in milliseconds; 0 clears it (default: wait forever).
  /// Covers the whole call — unary response or entire server-stream. On
  /// expiry the call fails with `DEADLINE_EXCEEDED` and the stream is
  /// cancelled (RST_STREAM), firing the server's cancel hook.
  void SetTimeout(uint32_t timeout_ms) {
    if (conn_) grpcuds_client_set_timeout_ms(conn_, timeout_ms);
  }

#ifdef GRPCUDSPP_HAVE_NANOPB
  /// Unary call: encode `req`, POST it to `path`, decode the reply.
  /// \tparam Req the nanopb request message type.
  /// \tparam Resp the nanopb reply message type.
  /// \param path the gRPC method path, e.g. `"/pkg.Service/Method"`.
  /// \param req the request message.
  /// \param req_fields nanopb descriptor for `Req` (e.g. `Foo_fields`).
  /// \param reply [out] decoded reply; left untouched on a non-OK status.
  /// \param resp_fields nanopb descriptor for `Resp`.
  /// \return OK on success, otherwise the server's status (with message) or a
  ///   transport / codec error.
  template <typename Req, typename Resp>
  Status Unary(const char* path, const Req& req, const pb_msgdesc_t* req_fields,
               Resp* reply, const pb_msgdesc_t* resp_fields) {
    std::vector<uint8_t> buf(GRPCUDSPP_MAX_MESSAGE_SIZE);
    pb_ostream_t os = pb_ostream_from_buffer(buf.data(), buf.size());
    if (!pb_encode(&os, req_fields, &req)) {
      return Status(INTERNAL, "request encode failed");
    }
    grpcuds_response* resp =
        grpcuds_client_unary(conn_, path, buf.data(), os.bytes_written);
    if (!resp) return Status(UNAVAILABLE, "transport failure");

    int code = grpcuds_response_status(resp);
    if (code != 0) {
      size_t mlen = 0;
      const uint8_t* m = grpcuds_response_message_bytes(resp, &mlen);
      std::string msg(m ? reinterpret_cast<const char*>(m) : "", mlen);
      grpcuds_response_free(resp);
      return Status(static_cast<StatusCode>(code), msg);
    }
    size_t blen = 0;
    const uint8_t* body = grpcuds_response_body(resp, &blen);
    pb_istream_t is = pb_istream_from_buffer(body, blen);
    bool ok = pb_decode(&is, resp_fields, reply);
    grpcuds_response_free(resp);
    return ok ? Status::Ok() : Status(INTERNAL, "response decode failed");
  }

  /// Start a server-streaming call: encode `req`, then read the returned
  /// ClientReader until it yields `false`. On a request-encode failure the
  /// reader is empty (its Read() returns `false` immediately).
  /// \tparam Req the nanopb request message type.
  /// \tparam Resp the nanopb response message type.
  /// \param path the gRPC method path.
  /// \param req the request message.
  /// \param req_fields nanopb descriptor for `Req`.
  /// \param resp_fields nanopb descriptor for `Resp`.
  /// \return a ClientReader over the response stream.
  template <typename Req, typename Resp>
  ClientReader<Resp> ServerStreaming(const char* path, const Req& req,
                                     const pb_msgdesc_t* req_fields,
                                     const pb_msgdesc_t* resp_fields) {
    std::vector<uint8_t> buf(GRPCUDSPP_MAX_MESSAGE_SIZE);
    pb_ostream_t os = pb_ostream_from_buffer(buf.data(), buf.size());
    grpcuds_stream* s = nullptr;
    if (pb_encode(&os, req_fields, &req)) {
      s = grpcuds_client_server_streaming(conn_, path, buf.data(),
                                          os.bytes_written);
    }
    return ClientReader<Resp>(s, resp_fields);
  }
#endif  // GRPCUDSPP_HAVE_NANOPB

  // ---- byte-level API (no nanopb) ---------------------------------------

  /// Unary call with a raw, pre-encoded request body (no nanopb).
  /// \param path the gRPC method path.
  /// \param req pointer to the request body.
  /// \param req_len request body length in bytes.
  /// \param out [out] receives the response body on an OK status; may be null.
  /// \return OK on success, otherwise the server's status or a transport error.
  Status UnaryRaw(const char* path, const uint8_t* req, size_t req_len,
                  std::vector<uint8_t>* out) {
    grpcuds_response* resp = grpcuds_client_unary(conn_, path, req, req_len);
    if (!resp) return Status(UNAVAILABLE, "transport failure");
    int code = grpcuds_response_status(resp);
    if (code != 0) {
      size_t mlen = 0;
      const uint8_t* m = grpcuds_response_message_bytes(resp, &mlen);
      std::string msg(m ? reinterpret_cast<const char*>(m) : "", mlen);
      grpcuds_response_free(resp);
      return Status(static_cast<StatusCode>(code), msg);
    }
    if (out) {
      size_t blen = 0;
      const uint8_t* body = grpcuds_response_body(resp, &blen);
      out->assign(body, body + blen);
    }
    grpcuds_response_free(resp);
    return Status::Ok();
  }

  /// Start a server-streaming call with a raw request body, returning the
  /// opaque `grpcuds_stream`. Read it with the C ABI `grpcuds_stream_next` (or
  /// prefer the typed ServerStreaming).
  /// \param path the gRPC method path.
  /// \param req pointer to the request body.
  /// \param req_len request body length in bytes.
  /// \return the opaque stream handle, or null on failure.
  grpcuds_stream* ServerStreamingRaw(const char* path, const uint8_t* req,
                                     size_t req_len) {
    return grpcuds_client_server_streaming(conn_, path, req, req_len);
  }

 private:
  grpcuds_client* conn_;
};

}  // namespace grpcuds

#endif  // GRPCUDSPP_CLIENT_H_
