// SPDX-License-Identifier: MIT OR Apache-2.0
//! nghttp2 receive-side callbacks (5 of them).
//!
//! All callbacks are `unsafe extern "C" fn` and panic-free. On allocation
//! failure we return `NGHTTP2_ERR_CALLBACK_FAILURE`; nghttp2 translates that
//! into a session-level fatal that the caller surfaces as
//! `ConnError::Nghttp2`.
//!
//! `user_data` always points at the connection's `ConnState` box (handed in
//! at `Session::new_server` time).

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::ffi::c_void;

use grpcuds_sys::{
    nghttp2_data_source, nghttp2_error_NGHTTP2_ERR_CALLBACK_FAILURE,
    nghttp2_error_NGHTTP2_ERR_PAUSE, nghttp2_error_NGHTTP2_ERR_TEMPORAL_CALLBACK_FAILURE,
    nghttp2_error_NGHTTP2_ERR_WOULDBLOCK, nghttp2_flag_NGHTTP2_FLAG_END_STREAM, nghttp2_frame,
    nghttp2_frame_type_NGHTTP2_HEADERS, nghttp2_headers_category_NGHTTP2_HCAT_REQUEST,
    nghttp2_session, nghttp2_session_callbacks_set_on_begin_headers_callback,
    nghttp2_session_callbacks_set_on_data_chunk_recv_callback,
    nghttp2_session_callbacks_set_on_frame_recv_callback,
    nghttp2_session_callbacks_set_on_header_callback,
    nghttp2_session_callbacks_set_on_stream_close_callback,
    nghttp2_session_callbacks_set_send_data_callback,
};

use crate::headers::GrpcStatus;
use crate::session::Callbacks;

use super::out_queue::OutQueue;
use super::state::{ConnState, StreamCtx, StreamState};

pub(super) const NV_HEADERS: u8 = nghttp2_frame_type_NGHTTP2_HEADERS as u8;
pub(super) const FLAG_END_STREAM: u8 = nghttp2_flag_NGHTTP2_FLAG_END_STREAM as u8;
pub(super) const CB_FAIL: i32 = nghttp2_error_NGHTTP2_ERR_CALLBACK_FAILURE;

pub(super) unsafe fn install_callbacks(cbs: &Callbacks) {
    let p = cbs.as_ptr();
    nghttp2_session_callbacks_set_on_begin_headers_callback(p, Some(on_begin_headers));
    nghttp2_session_callbacks_set_on_header_callback(p, Some(on_header));
    nghttp2_session_callbacks_set_on_frame_recv_callback(p, Some(on_frame_recv));
    nghttp2_session_callbacks_set_on_data_chunk_recv_callback(p, Some(on_data_chunk_recv));
    nghttp2_session_callbacks_set_on_stream_close_callback(p, Some(on_stream_close));
    nghttp2_session_callbacks_set_send_data_callback(p, Some(send_data));
}

#[inline]
pub(super) fn find_stream(state: &mut ConnState, id: i32) -> Option<&mut StreamCtx> {
    state
        .streams
        .iter_mut()
        .find(|s| s.id == id)
        .map(|b| &mut **b)
}

pub(super) unsafe extern "C" fn on_begin_headers(
    _session: *mut nghttp2_session,
    frame: *const nghttp2_frame,
    user_data: *mut c_void,
) -> i32 {
    let state = &mut *(user_data as *mut ConnState);
    let hd = (*frame).hd;
    if hd.type_ != NV_HEADERS {
        return 0;
    }
    // SAFETY: type_ == HEADERS → the union member `headers` is active.
    let cat = (*frame).headers.cat;
    if cat != nghttp2_headers_category_NGHTTP2_HCAT_REQUEST {
        return 0;
    }
    if state.streams.try_reserve(1).is_err() {
        return CB_FAIL;
    }
    // `Box::try_new` is unstable on stable Rust. Bare `Box::new` aborts on
    // a real OOM under `panic = "abort"`, which matches the existing
    // `Box::new(ConnState { … })` in `Conn::new_server` and the Box-typed
    // allocations across the FFI layer. The boxed payload (`StreamCtx`)
    // is small and dominated by its own `Vec` allocations, which are
    // fallible via `try_reserve` further down the call chain.
    state.streams.push(Box::new(StreamCtx {
        id: hd.stream_id,
        state: StreamState::HeadersIn,
        request: Vec::new(),
        path: Vec::new(),
        out: OutQueue::new(),
        status: GrpcStatus::Ok,
        cancel_hook: None,
        deadline_ms: None,
    }));
    0
}

pub(super) unsafe extern "C" fn on_header(
    _session: *mut nghttp2_session,
    frame: *const nghttp2_frame,
    name: *const u8,
    namelen: usize,
    value: *const u8,
    valuelen: usize,
    _flags: u8,
    user_data: *mut c_void,
) -> i32 {
    let state = &mut *(user_data as *mut ConnState);
    let id = (*frame).hd.stream_id;
    let s = match find_stream(state, id) {
        Some(s) => s,
        None => return 0,
    };
    let name_slice = core::slice::from_raw_parts(name, namelen);
    if name_slice == b":path" {
        let value_slice = core::slice::from_raw_parts(value, valuelen);
        if s.path.try_reserve(value_slice.len()).is_err() {
            return CB_FAIL;
        }
        s.path.extend_from_slice(value_slice);
    } else if name_slice == b"grpc-timeout" {
        let value_slice = core::slice::from_raw_parts(value, valuelen);
        // Malformed values are ignored per spec (no deadline), not failed.
        s.deadline_ms =
            crate::headers::parse_grpc_timeout_ms(value_slice).map(|t| crate::monotonic_ms() + t);
    }
    0
}

pub(super) unsafe extern "C" fn on_frame_recv(
    _session: *mut nghttp2_session,
    frame: *const nghttp2_frame,
    user_data: *mut c_void,
) -> i32 {
    let state = &mut *(user_data as *mut ConnState);
    let hd = (*frame).hd;
    let s = match find_stream(state, hd.stream_id) {
        Some(s) => s,
        None => return 0,
    };
    if hd.flags & FLAG_END_STREAM != 0 {
        s.state = StreamState::Complete;
    } else if hd.type_ == NV_HEADERS {
        s.state = StreamState::BodyIn;
    }
    0
}

pub(super) unsafe extern "C" fn on_data_chunk_recv(
    _session: *mut nghttp2_session,
    _flags: u8,
    stream_id: i32,
    data: *const u8,
    len: usize,
    user_data: *mut c_void,
) -> i32 {
    let state = &mut *(user_data as *mut ConnState);
    let max = state.max_message_len;
    let s = match find_stream(state, stream_id) {
        Some(s) => s,
        None => return 0,
    };
    let chunk = core::slice::from_raw_parts(data, len);
    if (s.request.len() as u64).saturating_add(chunk.len() as u64) > max as u64 {
        return CB_FAIL;
    }
    if s.request.try_reserve(chunk.len()).is_err() {
        return CB_FAIL;
    }
    s.request.extend_from_slice(chunk);
    0
}

/// `NO_COPY` direct send: ship one DATA frame — 9-byte frame header from
/// nghttp2 plus `length` promised bytes from the stream's outbound queue —
/// straight to the socket with a single `writev`, no intermediate buffer.
///
/// Ordering rules (frames must hit the wire in nghttp2's emission order):
///   - nghttp2 only invokes this when its own send buffer is fully drained,
///     and the I/O layer only calls `pull_send` when `out_pending` is empty,
///     so writing here cannot overtake earlier bytes.
///   - On a partial write the unsent tail goes into `out_pending` and we
///     return `PAUSE` ("frame processed, stop emitting") so nothing further
///     is generated until the I/O layer flushes the tail.
///   - On EAGAIN with nothing written, `WOULDBLOCK` retries the whole frame
///     later.
pub(super) unsafe extern "C" fn send_data(
    _session: *mut nghttp2_session,
    frame: *mut nghttp2_frame,
    framehd: *const u8,
    length: usize,
    _source: *mut nghttp2_data_source,
    user_data: *mut c_void,
) -> i32 {
    let state = &mut *(user_data as *mut ConnState);
    let fd = state.fd;
    if fd < 0 || !state.out_pending.is_empty() {
        // No socket attached (shouldn't promise NO_COPY then), or earlier
        // bytes still queued — sending now would reorder. Retry later.
        return nghttp2_error_NGHTTP2_ERR_WOULDBLOCK;
    }
    // We never submit padded DATA frames; refuse rather than mis-frame.
    if (*frame).data.padlen != 0 {
        return nghttp2_error_NGHTTP2_ERR_CALLBACK_FAILURE;
    }
    let stream_id = (*frame).hd.stream_id;

    let hd = core::slice::from_raw_parts(framehd, 9);
    let s = match find_stream(state, stream_id) {
        Some(s) => s,
        None => return nghttp2_error_NGHTTP2_ERR_TEMPORAL_CALLBACK_FAILURE,
    };
    // Collected inside the block below (where hd/a/b are alive) and logged
    // after the stream borrow ends — a dev-feature-only copy.
    #[cfg(feature = "wirelog")]
    let mut wl_sent: Vec<u8> = Vec::new();
    // Err(()) = stash allocation failed (real OOM). The early return for
    // it happens BELOW, after wirelog logging — the n bytes already
    // reached the socket and must land in the capture either way.
    let stash: Result<Option<Vec<u8>>, ()> = {
        let (a, b) = s.out.front_regions(length);
        if a.len() + b.len() != length {
            // The promise is stale (should be impossible: the front is
            // commit-pinned). Reset the stream rather than corrupt framing.
            return nghttp2_error_NGHTTP2_ERR_TEMPORAL_CALLBACK_FAILURE;
        }
        let iov = [
            libc::iovec {
                iov_base: hd.as_ptr() as *mut c_void,
                iov_len: hd.len(),
            },
            libc::iovec {
                iov_base: a.as_ptr() as *mut c_void,
                iov_len: a.len(),
            },
            libc::iovec {
                iov_base: b.as_ptr() as *mut c_void,
                iov_len: b.len(),
            },
        ];
        let total = hd.len() + length;
        // sendmsg + MSG_NOSIGNAL, not writev(2): a dead client must surface
        // as EPIPE, not as a SIGPIPE that kills a C host daemon.
        let mut msg: libc::msghdr = core::mem::zeroed();
        msg.msg_iov = iov.as_ptr() as *mut libc::iovec;
        msg.msg_iovlen = 3;
        let n = loop {
            let rc = libc::sendmsg(fd, &msg, libc::MSG_NOSIGNAL);
            if rc >= 0 {
                break rc as usize;
            }
            let e = *libc::__errno_location();
            if e == libc::EINTR {
                continue;
            }
            if e == libc::EAGAIN || e == libc::EWOULDBLOCK {
                break 0;
            }
            return nghttp2_error_NGHTTP2_ERR_CALLBACK_FAILURE;
        };
        if n == 0 {
            // Nothing went out — safe to have nghttp2 re-offer the frame.
            return nghttp2_error_NGHTTP2_ERR_WOULDBLOCK;
        }
        #[cfg(feature = "wirelog")]
        {
            let mut take = n;
            for part in [hd, a, b] {
                let k = take.min(part.len());
                if let Some(sent) = part.get(..k) {
                    let _ = wl_sent.try_reserve(k);
                    wl_sent.extend_from_slice(sent);
                }
                take -= k;
            }
        }
        if n < total {
            // Collect the unsent tail (possibly spanning framehd/a/b) so the
            // I/O layer can finish it before any other outbound byte.
            let mut rest: Vec<u8> = Vec::new();
            if rest.try_reserve_exact(total - n).is_err() {
                Err(())
            } else {
                let mut skip = n;
                for part in [hd, a, b] {
                    if skip >= part.len() {
                        skip -= part.len();
                    } else {
                        rest.extend_from_slice(&part[skip..]);
                        skip = 0;
                    }
                }
                Ok(Some(rest))
            }
        } else {
            Ok(None)
        }
    };
    // Either way the frame is "sent" from nghttp2's perspective.
    s.out.consume_front(length);
    #[cfg(feature = "wirelog")]
    if let Some(wl) = state.wirelog.as_mut() {
        crate::wirelog::log(wl, crate::wirelog::Dir::ServerToClient, &wl_sent);
    }
    match stash {
        Ok(None) => 0,
        Ok(Some(rest)) => {
            state.out_pending = rest;
            nghttp2_error_NGHTTP2_ERR_PAUSE
        }
        // Stash OOM: the session is going down, but the capture above
        // already recorded the bytes that reached the socket.
        Err(()) => nghttp2_error_NGHTTP2_ERR_CALLBACK_FAILURE,
    }
}

pub(super) unsafe extern "C" fn on_stream_close(
    _session: *mut nghttp2_session,
    stream_id: i32,
    error_code: u32,
    user_data: *mut c_void,
) -> i32 {
    let state = &mut *(user_data as *mut ConnState);
    // `on_stream_close` is the last callback nghttp2 fires for this stream
    // id, so the per-call context is dropped HERE — keeping it would leak
    // one StreamCtx (request/path/queue buffers, ~0.6 KB) per completed
    // call on a long-lived connection. Late ops for the id (a producer
    // racing the close) resolve to StreamNotFound, which callers already
    // handle as "call over".
    if let Some(pos) = state.streams.iter().position(|s| s.id == stream_id) {
        let mut s = state.streams.swap_remove(pos);
        if error_code != 0 {
            // Cancelled (RST_STREAM, protocol error): fire the cleanup hook
            // exactly once. On a clean close the hook never fires and is
            // dropped with the stream.
            crate::logging::debug(c"stream cancelled by peer", stream_id as i64);
            if let Some(hook) = s.cancel_hook.take() {
                (hook.callback)(hook.user_data);
            }
        }
    }
    0
}
