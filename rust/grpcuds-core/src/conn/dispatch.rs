// SPDX-License-Identifier: MIT OR Apache-2.0
//! Handler dispatch + per-stream `data_provider` read_callback.
//!
//! These two responsibilities live together because they straddle the same
//! invariant: `submit_response` (issued from dispatch) installs the
//! `data_provider_read` callback below, and from that moment onwards
//! nghttp2 owns the transmission of DATA + trailing HEADERS for the call.

use core::ffi::c_void;

use grpcuds_sys::{
    nghttp2_data_flag_NGHTTP2_DATA_FLAG_EOF, nghttp2_data_flag_NGHTTP2_DATA_FLAG_NO_COPY,
    nghttp2_data_flag_NGHTTP2_DATA_FLAG_NO_END_STREAM, nghttp2_data_source,
    nghttp2_error_NGHTTP2_ERR_CALLBACK_FAILURE, nghttp2_error_NGHTTP2_ERR_DEFERRED,
    nghttp2_error_NGHTTP2_ERR_TEMPORAL_CALLBACK_FAILURE, nghttp2_session, nghttp2_submit_trailer,
};

/// Front-message size at which the read callback switches from "copy into
/// nghttp2's frame buffer" to a promised `NO_COPY` direct send. Below this,
/// aggregation (many small messages per DATA frame per write syscall) beats
/// zero-copy; above it, skipping two memcpys of the payload wins.
const NOCOPY_MIN: usize = 4096;

use crate::framing::{decode_header, FRAME_HEADER_LEN};
use crate::headers::{trailer, trailer_with_message, GrpcStatus};

use super::callbacks::find_stream;
use super::state::{Conn, ConnState, StreamCtx, StreamState};

impl Conn {
    /// Run dispatch over every stream in state `Complete`. For each, decode
    /// the 5-byte gRPC frame, look up the handler by `:path`, invoke it,
    /// and transition the stream to `Dispatched`. Returns the number of
    /// streams dispatched in this pass.
    ///
    pub fn dispatch(&mut self) -> usize {
        let max_len = self.state.max_message_len;
        let mut count = 0usize;
        let n = self.state.streams.len();
        for i in 0..n {
            if self.state.streams[i].state != StreamState::Complete {
                continue;
            }
            // Parse the single gRPC frame from the accumulated request bytes.
            let header = match decode_header(&self.state.streams[i].request, max_len) {
                Ok(h) => h,
                Err(_) => {
                    crate::logging::error(
                        c"malformed request frame",
                        self.state.streams[i].id as i64,
                    );
                    finalize_error(&mut self.state.streams[i], GrpcStatus::Internal);
                    let _ = self.submit_response_for(self.state.streams[i].id);
                    count += 1;
                    continue;
                }
            };
            let payload_len = header.payload_len as usize;
            if self.state.streams[i].request.len() < FRAME_HEADER_LEN + payload_len {
                finalize_error(&mut self.state.streams[i], GrpcStatus::Internal);
                let _ = self.submit_response_for(self.state.streams[i].id);
                count += 1;
                continue;
            }

            // Resolve handler (immutable borrow of methods + streams[i].path).
            let resolved = self
                .state
                .methods
                .iter()
                .find(|e| e.path.as_slice() == self.state.streams[i].path.as_slice())
                .map(|e| (e.handler, e.user_data, e.backpressure));

            let (handler, ud, bp) = match resolved {
                Some(t) => t,
                None => {
                    crate::logging::info(
                        c"unimplemented method called",
                        self.state.streams[i].id as i64,
                    );
                    finalize_error(&mut self.state.streams[i], GrpcStatus::Unimplemented);
                    let _ = self.submit_response_for(self.state.streams[i].id);
                    count += 1;
                    continue;
                }
            };

            // Apply the method-level backpressure config (if any) BEFORE
            // the handler runs, so a streaming handler can write without
            // first calling set_stream_policy.
            if let Some(bp) = bp {
                self.state.streams[i].out.backpressure = bp;
            }

            // Invoke handler. `self as *mut Conn` is a stable pointer for
            // the entire connection lifetime — safe to use inside FFI even
            // though we technically hold a `&mut self` borrow throughout
            // dispatch; the handler is the only code that runs and we
            // re-borrow when it returns.
            let call_id = self.state.streams[i].id;
            let conn_ptr: *mut Conn = self;
            // SAFETY: payload range was bounds-checked above.
            let payload_ptr =
                unsafe { self.state.streams[i].request.as_ptr().add(FRAME_HEADER_LEN) };
            let rc = unsafe { handler(conn_ptr, call_id, payload_ptr, payload_len, ud) };

            // Transition state and auto-finish on a non-OK return if the
            // handler did not explicitly finish.
            {
                let s = &mut self.state.streams[i];
                s.state = StreamState::Dispatched;
                if rc != 0 && !s.out.finished {
                    s.out.finished = true;
                    s.out.final_status = grpc_status_from_i32(rc);
                }
            }

            // Issue the response HEADERS + data_provider wiring exactly
            // once per stream. From here on nghttp2 owns transmission.
            if !self.state.streams[i].out.response_started {
                if let Err(e) = self.submit_response_for(call_id) {
                    // Submission failed (alloc, protocol error). Mark stream
                    // as cancelled and move on — we have no recovery path.
                    let s = &mut self.state.streams[i];
                    s.state = StreamState::Cancelled;
                    let _ = e;
                }
            }
            count += 1;
        }
        count
    }
}

/// Finalize a stream as an error without invoking a handler. Stamps the
/// status, marks the queue finished, and transitions to Dispatched so the
/// trailing HEADERS gets shipped from the data_provider's EOF branch.
pub(super) fn finalize_error(s: &mut StreamCtx, status: GrpcStatus) {
    s.status = status;
    s.out.finished = true;
    s.out.final_status = status;
    s.state = StreamState::Dispatched;
}

/// Map a handler's i32 return value to the typed `GrpcStatus`. Out-of-range
/// values collapse to `Unknown` so a misbehaving handler can't produce an
/// undefined `repr(u8)` discriminant.
fn grpc_status_from_i32(rc: i32) -> GrpcStatus {
    if (0..=16).contains(&rc) {
        // SAFETY: `GrpcStatus` is `#[repr(u8)]` with discriminants 0..=16.
        unsafe { core::mem::transmute::<u8, GrpcStatus>(rc as u8) }
    } else {
        GrpcStatus::Unknown
    }
}

// ---- Per-stream data_provider read_callback -------------------------------
//
// Pull-based source for response DATA frames. Branches:
//   - queue has bytes  → copy into `buf`, return count
//   - queue empty + finished → set EOF + NO_END_STREAM, submit trailer
//   - queue empty + open → return NGHTTP2_ERR_DEFERRED so nghttp2 pauses
//     this stream's DATA frame; later `write_call` / `finish_call` calls
//     resume_data to wake us up
pub(super) unsafe extern "C" fn data_provider_read(
    session: *mut nghttp2_session,
    stream_id: i32,
    buf: *mut u8,
    length: usize,
    data_flags: *mut u32,
    _source: *mut nghttp2_data_source,
    user_data: *mut c_void,
) -> isize {
    let state = &mut *(user_data as *mut ConnState);
    let direct_fd = state.fd;
    let s = match find_stream(state, stream_id) {
        Some(s) => s,
        None => return nghttp2_error_NGHTTP2_ERR_TEMPORAL_CALLBACK_FAILURE as isize,
    };

    // Large front message + a real socket attached → promise the bytes as a
    // NO_COPY DATA frame; `send_data` ships them fd-direct, skipping both
    // the copy into nghttp2's frame buffer and its internal send buffer.
    // Small messages keep the copy path: nghttp2 then aggregates many of
    // them per write syscall, which is the better trade below the threshold.
    if direct_fd >= 0 && s.out.front_remaining() >= NOCOPY_MIN {
        let n = s.out.promise_front(length);
        *data_flags |= nghttp2_data_flag_NGHTTP2_DATA_FLAG_NO_COPY;
        return n as isize;
    }

    let dst = core::slice::from_raw_parts_mut(buf, length);
    let n = s.out.drain_into(dst);
    if n > 0 {
        return n as isize;
    }
    if s.out.finished {
        *data_flags |= nghttp2_data_flag_NGHTTP2_DATA_FLAG_EOF;
        // Keep the stream open so we can ship the trailing HEADERS.
        *data_flags |= nghttp2_data_flag_NGHTTP2_DATA_FLAG_NO_END_STREAM;
        let rc = match s.out.final_message.as_deref() {
            Some(msg) if !msg.is_empty() => {
                let tr = trailer_with_message(s.out.final_status, msg);
                nghttp2_submit_trailer(session, stream_id, tr.as_ptr(), tr.len())
            }
            _ => {
                let tr = trailer(s.out.final_status);
                nghttp2_submit_trailer(session, stream_id, tr.as_ptr(), tr.len())
            }
        };
        if rc != 0 {
            return nghttp2_error_NGHTTP2_ERR_CALLBACK_FAILURE as isize;
        }
        return 0;
    }
    // Queue empty, more messages coming — pause.
    nghttp2_error_NGHTTP2_ERR_DEFERRED as isize
}
