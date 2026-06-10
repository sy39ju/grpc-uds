// SPDX-License-Identifier: MIT OR Apache-2.0
//! Connection / stream state and the public `Conn` surface.
//!
//! `Conn` owns the nghttp2 `Session` plus a heap-allocated `ConnState`
//! whose address is handed to nghttp2 as `user_data`. Field declaration
//! order matters: `session` is declared first so it drops first — that
//! lets nghttp2 fire `on_stream_close` while `state` is still alive.
//!
//! Cross-module invariants:
//!   * `ConnState::streams` is appended-to by [`super::callbacks::on_begin_headers`]
//!     and read by every other site through `iter`/`iter_mut`; entries are
//!     `Box`ed, so a `*mut StreamCtx` stays valid even if the `Vec`
//!     reallocates.
//!   * The handler trampoline in [`super::dispatch`] mutably re-borrows the
//!     `Conn` from a raw pointer; while it runs, the `Conn` itself is *not*
//!     accessible via Rust borrow rules — the trampoline is the sole code
//!     path.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::ffi::c_void;
use core::ptr;

use grpcuds_sys::{
    nghttp2_data_provider, nghttp2_data_source, nghttp2_error_NGHTTP2_ERR_WOULDBLOCK,
    nghttp2_session_mem_recv, nghttp2_session_mem_send, nghttp2_session_resume_data,
    nghttp2_session_want_read, nghttp2_session_want_write, nghttp2_submit_response,
    nghttp2_submit_settings,
};

use crate::framing::DEFAULT_MAX_MESSAGE_LEN;
use crate::headers::{percent_encode_message, response_headers, GrpcStatus};
use crate::session::{Callbacks, Session, SessionError};

use super::callbacks::install_callbacks;
use super::dispatch::data_provider_read;
use super::out_queue::OutQueue;
use super::Backpressure;

// ---- Public types ---------------------------------------------------------

#[cfg_attr(test, derive(Debug))]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum StreamState {
    /// HEADERS frame in flight.
    HeadersIn,
    /// HEADERS done, DATA frame(s) accumulating.
    BodyIn,
    /// END_STREAM seen — request fully received, ready for dispatch.
    Complete,
    /// Handler ran; response + status are stored on the stream.
    Dispatched,
    /// Stream closed gracefully (error_code 0).
    Closed,
    /// Stream closed with non-zero error (RST_STREAM, etc.).
    Cancelled,
}

/// Handler signature, already shaped for the C ABI exposure.
///
/// Receives a stable pointer to the owning [`Conn`] (the connection survives
/// the call) plus the stream's `call_id` (== HTTP/2 stream_id, stable for
/// the call's lifetime). The handler uses `Conn::write_call` /
/// `Conn::finish_call` for output. Saving `(conn, call_id)` lets a streaming
/// handler push messages from an async source (BLE callback) without
/// holding stack borrows.
///
/// Return value is a gRPC status code (0 = OK); out-of-range values collapse
/// to `Unknown`. If the handler returns non-zero without having called
/// `finish_call`, the dispatcher auto-finishes with that status.
pub type HandlerFn = unsafe extern "C" fn(
    conn: *mut Conn,
    call_id: i32,
    req: *const u8,
    req_len: usize,
    user_data: *mut c_void,
) -> i32;

/// Cleanup invoked from `on_stream_close` when a stream is cancelled
/// (RST_STREAM or other non-zero error). Streaming handlers install this
/// to release per-call resources (BLE scan handles, GATT subscriptions,
/// allocated state).
///
/// **`user_data` lifetime contract** — must remain valid until *either*:
///   1. the callback fires (cancel path), or
///   2. the call closes gracefully and the owning connection is dropped
///      (the hook never fires; user_data is forgotten).
///
/// The hook does NOT fire on graceful close (status:0 trailer). Handlers
/// that need a "called exactly once on either path" lifecycle should
/// also run their cleanup from the path that does the graceful finish.
///
/// **Safe pattern**: heap-allocate the state, hand the pointer in as
/// `user_data`, free from inside the callback. Stack-bound pointers
/// (locals in the streaming handler) DO NOT WORK because the handler
/// returns long before the hook can fire.
#[derive(Clone, Copy)]
pub struct CancelHook {
    pub callback: unsafe extern "C" fn(*mut c_void),
    pub user_data: *mut c_void,
}

pub struct StreamCtx {
    pub id: i32,
    pub state: StreamState,
    /// Raw accumulated DATA bytes. gRPC 5B framing is left for the caller.
    pub request: Vec<u8>,
    /// `:path` value, e.g. `b"/pkg.Svc/Method"`. nghttp2 lowercases on the
    /// receive side already.
    pub path: Vec<u8>,
    /// Outbound message queue with finish/status flags. Drained by the
    /// per-stream data_provider read_callback.
    pub out: OutQueue,
    /// Final gRPC status for this call. Set when the handler / app calls
    /// `finish_call`. Meaningful once `state >= Dispatched`.
    pub status: GrpcStatus,
    /// Set by a streaming handler so we can clean up resources on cancel.
    pub cancel_hook: Option<CancelHook>,
    /// Absolute CLOCK_MONOTONIC expiry (ms) parsed from the client's
    /// `grpc-timeout` header; `None` = no deadline.
    pub deadline_ms: Option<u64>,
}

pub(super) struct MethodEntry {
    pub(super) path: Vec<u8>,
    pub(super) handler: HandlerFn,
    pub(super) user_data: *mut c_void,
    /// Optional per-method default backpressure config. Applied to each
    /// new call's OutQueue right before its handler runs. Handlers can
    /// still override via `Conn::set_stream_policy`. `None` means no
    /// method-level default (the OutQueue stays at `Backpressure::Unbounded`).
    pub(super) backpressure: Option<Backpressure>,
}

#[cfg_attr(test, derive(Debug))]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ConnError {
    /// Failure creating an nghttp2 session / callbacks block.
    Session(SessionError),
    /// `try_reserve` or `Box::new`-style allocation failed. Real OOM.
    OutOfMemory,
    /// No active stream exists for the given `call_id` on this connection.
    /// Either the id is wrong or the call already closed.
    StreamNotFound,
    /// The call was already marked finished — further `write_call` /
    /// `finish_call` is a programming error.
    StreamFinished,
    /// Outbound queue is at capacity and the active policy is `Reject`.
    QueueFull,
    /// Raw nghttp2 error code (negative). Pass-through from libnghttp2 for
    /// situations that don't map cleanly to our taxonomy.
    Nghttp2(i32),
}

impl From<SessionError> for ConnError {
    fn from(e: SessionError) -> Self {
        ConnError::Session(e)
    }
}

// ---- Conn -----------------------------------------------------------------

pub struct Conn {
    pub(super) session: Session,
    pub(super) state: Box<ConnState>,
}

pub(super) struct ConnState {
    /// Per-stream contexts, boxed individually so a `*mut StreamCtx` taken
    /// via `find_stream` survives a later push into `streams` (the `Vec`
    /// may reallocate; the boxes don't move). At these call sizes the
    /// indirection cost is negligible, and the stable-pointer guarantee
    /// keeps e.g. a handler installing a new stream mid-callback sound.
    // Box is load-bearing (stable addresses), not accidental indirection.
    #[allow(clippy::vec_box)]
    pub(super) streams: Vec<Box<StreamCtx>>,
    pub(super) methods: Vec<MethodEntry>,
    pub(super) max_message_len: u32,
    /// Socket fd for the `NO_COPY` direct-send path (send_data_callback).
    /// `-1` when the conn is driven without a socket (in-process tests,
    /// mem-interface drivers) — large messages then fall back to the copy
    /// path, which is always correct.
    pub(super) fd: i32,
    /// Tail bytes of a partially-written `NO_COPY` DATA frame (or of a
    /// `pull_send` chunk, stashed by the I/O layer). MUST fully drain to the
    /// socket before any other outbound byte, or frames would reorder.
    pub(super) out_pending: Vec<u8>,
    /// Dev-only wire logging stream (`None` = disabled at runtime).
    #[cfg(feature = "wirelog")]
    pub(super) wirelog: Option<crate::wirelog::WirelogConn>,
}

impl Conn {
    pub fn new_server() -> Result<Self, ConnError> {
        let cbs = Callbacks::new()?;
        unsafe { install_callbacks(&cbs) };
        let mut state = Box::new(ConnState {
            streams: Vec::new(),
            methods: Vec::new(),
            max_message_len: DEFAULT_MAX_MESSAGE_LEN,
            fd: -1,
            out_pending: Vec::new(),
            #[cfg(feature = "wirelog")]
            wirelog: crate::wirelog::conn_open(),
        });
        let user_data = state.as_mut() as *mut ConnState as *mut c_void;
        // SAFETY: user_data points into the Box<ConnState> owned by the
        // returned Conn; `session` is declared before `state` so it drops
        // first, keeping the pointee alive for every callback.
        let session = unsafe { Session::new_server(&cbs, user_data) }?;
        let mut this = Self { session, state };
        // Server must submit SETTINGS first (RFC 7540 §3.5).
        this.submit_initial_settings()?;
        Ok(this)
    }

    /// Register a unary/streaming handler for the given `:path` value.
    /// Later requests whose `:path` matches `path` exactly will dispatch
    /// to `handler` with `user_data` passed through.
    pub fn register_method(
        &mut self,
        path: &[u8],
        handler: HandlerFn,
        user_data: *mut c_void,
    ) -> Result<(), ConnError> {
        self.register_method_inner(path, handler, user_data, None)
    }

    /// Like `register_method` but also stores a method-level backpressure
    /// default. Every new call of this method gets its OutQueue configured
    /// with `bp` *before* its handler runs — so the handler doesn't have
    /// to remember to call `set_stream_policy`. The handler may still
    /// override at runtime via `set_stream_policy`.
    pub fn register_streaming_method(
        &mut self,
        path: &[u8],
        handler: HandlerFn,
        user_data: *mut c_void,
        bp: Backpressure,
    ) -> Result<(), ConnError> {
        self.register_method_inner(path, handler, user_data, Some(bp))
    }

    fn register_method_inner(
        &mut self,
        path: &[u8],
        handler: HandlerFn,
        user_data: *mut c_void,
        backpressure: Option<Backpressure>,
    ) -> Result<(), ConnError> {
        let mut owned = Vec::new();
        owned
            .try_reserve(path.len())
            .map_err(|_| ConnError::OutOfMemory)?;
        owned.extend_from_slice(path);
        self.state
            .methods
            .try_reserve(1)
            .map_err(|_| ConnError::OutOfMemory)?;
        self.state.methods.push(MethodEntry {
            path: owned,
            handler,
            user_data,
            backpressure,
        });
        Ok(())
    }

    /// Enqueue a single gRPC message payload (the 5-byte prefix is added
    /// internally) onto the call's outbound queue. If the call is past
    /// dispatch this also nudges nghttp2 to resume any deferred DATA frame.
    ///
    /// The payload is **copied** (the caller keeps ownership — this is the
    /// C ABI shape). Rust callers that own the bytes should prefer
    /// [`Self::write_call_owned`], which moves them instead.
    pub fn write_call(&mut self, call_id: i32, payload: &[u8]) -> Result<(), ConnError> {
        self.write_call_with(call_id, |out| out.enqueue_framed(payload))
    }

    /// Like [`Self::write_call`], but takes the payload by value — the
    /// message bytes are moved into the outbound queue without a copy.
    pub fn write_call_owned(&mut self, call_id: i32, payload: Vec<u8>) -> Result<(), ConnError> {
        self.write_call_with(call_id, move |out| out.enqueue_owned(payload))
    }

    fn write_call_with(
        &mut self,
        call_id: i32,
        enqueue: impl FnOnce(&mut OutQueue) -> Result<(), ConnError>,
    ) -> Result<(), ConnError> {
        let session_ptr = self.session.as_ptr();
        let (needs_resume, response_started) = {
            let s = self
                .state
                .streams
                .iter_mut()
                .find(|s| s.id == call_id)
                .ok_or(ConnError::StreamNotFound)?;
            if s.out.finished {
                return Err(ConnError::StreamFinished);
            }
            enqueue(&mut s.out)?;
            (
                s.state == StreamState::Dispatched && s.out.response_started,
                s.out.response_started,
            )
        };
        if needs_resume && response_started {
            unsafe {
                nghttp2_session_resume_data(session_ptr, call_id);
            }
        }
        Ok(())
    }

    /// Mark the call as finished. Sets the trailer status and resumes any
    /// deferred DATA frame so the empty-queue + finished branch in the
    /// read_callback fires and ships the trailing HEADERS.
    pub fn finish_call(&mut self, call_id: i32, status: GrpcStatus) -> Result<(), ConnError> {
        self.finish_call_inner(call_id, status, None)
    }

    /// Like [`Self::finish_call`], but also ships a `grpc-message` trailer.
    /// `msg` is the raw (un-encoded) message; percent-encoding happens here.
    /// An empty `msg` is treated as no message (status-only trailer).
    pub fn finish_call_msg(
        &mut self,
        call_id: i32,
        status: GrpcStatus,
        msg: &[u8],
    ) -> Result<(), ConnError> {
        self.finish_call_inner(call_id, status, Some(msg))
    }

    fn finish_call_inner(
        &mut self,
        call_id: i32,
        status: GrpcStatus,
        msg: Option<&[u8]>,
    ) -> Result<(), ConnError> {
        let session_ptr = self.session.as_ptr();
        let (needs_resume, response_started) = {
            let s = self
                .state
                .streams
                .iter_mut()
                .find(|s| s.id == call_id)
                .ok_or(ConnError::StreamNotFound)?;
            if !s.out.finished {
                s.out.finished = true;
                s.out.final_status = status;
                if let Some(m) = msg {
                    if !m.is_empty() {
                        let mut enc = Vec::new();
                        percent_encode_message(m, &mut enc);
                        s.out.final_message = Some(enc);
                    }
                }
            }
            (
                s.state == StreamState::Dispatched && s.out.response_started,
                s.out.response_started,
            )
        };
        if needs_resume && response_started {
            unsafe {
                nghttp2_session_resume_data(session_ptr, call_id);
            }
        }
        Ok(())
    }

    /// Enforce `grpc-timeout` deadlines: every dispatched, unfinished
    /// stream whose deadline has passed fires its cancel hook (so a
    /// deferred producer stops) and finishes with `DEADLINE_EXCEEDED` —
    /// what a stock gRPC server does when a deadline expires server-side.
    /// Called from every connection tick; hosts bound their poll timeout
    /// with [`Self::next_deadline_ms`] so idle connections expire too.
    pub fn expire_deadlines(&mut self) {
        let now = crate::monotonic_ms();
        let mut expired: [i32; 8] = [0; 8];
        let mut n = 0;
        for s in self.state.streams.iter_mut() {
            if n == expired.len() {
                break; // the rest expire on the next tick
            }
            if s.state == StreamState::Dispatched && !s.out.finished {
                if let Some(d) = s.deadline_ms {
                    if d <= now {
                        s.deadline_ms = None;
                        if let Some(hook) = s.cancel_hook.take() {
                            unsafe { (hook.callback)(hook.user_data) };
                        }
                        expired[n] = s.id;
                        n += 1;
                    }
                }
            }
        }
        for &id in &expired[..n] {
            crate::logging::info(c"call deadline expired", id as i64);
            let _ = self.finish_call_msg(id, GrpcStatus::DeadlineExceeded, b"deadline exceeded");
        }
    }

    /// Remaining milliseconds of one call's `grpc-timeout` budget:
    /// `Ok(None)` when the client sent no deadline, `Ok(Some(0))` when it
    /// is already due. Handlers use this to skip work that cannot finish
    /// in time (stock gRPC's `context.deadline()`).
    pub fn call_time_remaining_ms(&self, call_id: i32) -> Result<Option<u64>, ConnError> {
        let s = self
            .state
            .streams
            .iter()
            .find(|s| s.id == call_id)
            .ok_or(ConnError::StreamNotFound)?;
        let now = crate::monotonic_ms();
        Ok(s.deadline_ms.map(|d| d.saturating_sub(now)))
    }

    /// Remaining milliseconds until the earliest armed deadline on this
    /// connection (`Some(0)` = due now), or `None` when no in-flight call
    /// carries one. Use it to bound a poll timeout.
    pub fn next_deadline_ms(&self) -> Option<u64> {
        let now = crate::monotonic_ms();
        self.state
            .streams
            .iter()
            .filter(|s| s.state == StreamState::Dispatched && !s.out.finished)
            .filter_map(|s| s.deadline_ms)
            .min()
            .map(|d| d.saturating_sub(now))
    }

    /// Install a cancel-cleanup hook on an active call. The runtime fires
    /// the callback exactly once when the stream is closed with a non-zero
    /// error code (RST_STREAM, protocol error, etc.) — typically the
    /// streaming handler's chance to stop a backing producer (e.g. cancel
    /// a BLE scan) and free per-call resources.
    ///
    /// The `user_data` pointer must remain valid until either:
    ///   - the hook fires (cancel path), or
    ///   - the call closes gracefully (`grpc-status:0` trailer; the hook
    ///     never fires and is dropped together with the stream context).
    ///
    /// On graceful close the hook does NOT fire — handlers that need a
    /// "called once on either path" lifecycle should additionally call
    /// their cleanup from the streaming handler's exit path.
    pub fn set_cancel_hook(
        &mut self,
        call_id: i32,
        callback: unsafe extern "C" fn(*mut c_void),
        user_data: *mut c_void,
    ) -> Result<(), ConnError> {
        let s = self
            .state
            .streams
            .iter_mut()
            .find(|s| s.id == call_id)
            .ok_or(ConnError::StreamNotFound)?;
        s.cancel_hook = Some(CancelHook {
            callback,
            user_data,
        });
        Ok(())
    }

    /// Configure outbound backpressure for an active call. Pass
    /// [`Backpressure::Unbounded`] to disable. Typically called from inside
    /// a streaming handler before kicking off the async producer:
    ///
    /// ```ignore
    /// use core::num::NonZeroUsize;
    /// use grpcuds_core::{Backpressure, OverflowPolicy};
    ///
    /// // BLE scan: keep latest 4 results, drop older ones.
    /// conn.set_stream_policy(call_id, Backpressure::Bounded {
    ///     capacity: NonZeroUsize::new(4).unwrap(),
    ///     policy: OverflowPolicy::DropOldest,
    /// })?;
    ///
    /// // BLE GATT notifications: never drop; producer must handle QueueFull.
    /// conn.set_stream_policy(call_id, Backpressure::Bounded {
    ///     capacity: NonZeroUsize::new(16).unwrap(),
    ///     policy: OverflowPolicy::Reject,
    /// })?;
    /// ```
    pub fn set_stream_policy(&mut self, call_id: i32, bp: Backpressure) -> Result<(), ConnError> {
        let s = self
            .state
            .streams
            .iter_mut()
            .find(|s| s.id == call_id)
            .ok_or(ConnError::StreamNotFound)?;
        s.out.backpressure = bp;
        Ok(())
    }

    pub(super) fn submit_response_for(&mut self, call_id: i32) -> Result<(), ConnError> {
        let nva = response_headers();
        let provider = nghttp2_data_provider {
            source: nghttp2_data_source {
                ptr: ptr::null_mut(),
            },
            read_callback: Some(data_provider_read),
        };
        let rc = unsafe {
            nghttp2_submit_response(
                self.session.as_ptr(),
                call_id,
                nva.as_ptr(),
                nva.len(),
                &provider,
            )
        };
        if rc != 0 {
            return Err(ConnError::Nghttp2(rc));
        }
        if let Some(s) = self.state.streams.iter_mut().find(|s| s.id == call_id) {
            s.out.response_started = true;
        }
        Ok(())
    }

    fn submit_initial_settings(&mut self) -> Result<(), ConnError> {
        let rc = unsafe { nghttp2_submit_settings(self.session.as_ptr(), 0, ptr::null(), 0) };
        if rc != 0 {
            return Err(ConnError::Nghttp2(rc));
        }
        Ok(())
    }

    /// Feed bytes received from the peer. Returns bytes consumed.
    pub fn recv(&mut self, data: &[u8]) -> Result<usize, ConnError> {
        if data.is_empty() {
            return Ok(0);
        }
        #[cfg(feature = "wirelog")]
        if let Some(wl) = self.state.wirelog.as_mut() {
            crate::wirelog::log(wl, crate::wirelog::Dir::ClientToServer, data);
        }
        let rc =
            unsafe { nghttp2_session_mem_recv(self.session.as_ptr(), data.as_ptr(), data.len()) };
        if rc < 0 {
            crate::logging::error(c"nghttp2 recv failed", rc as i64);
            return Err(ConnError::Nghttp2(rc as i32));
        }
        Ok(rc as usize)
    }

    /// Borrow the next outgoing chunk from nghttp2. Slice is valid only
    /// until the next `pull_send` or `recv` on this `Conn`.
    ///
    /// `NO_COPY` DATA frames never appear here — they go straight to the
    /// socket via the send_data_callback (which may run *during* this call).
    /// A `WOULDBLOCK` bubbled up from that callback means "socket full, try
    /// again later" and is mapped to an empty chunk.
    pub fn pull_send(&mut self) -> Result<&[u8], ConnError> {
        let mut p: *const u8 = ptr::null();
        let rc = unsafe { nghttp2_session_mem_send(self.session.as_ptr(), &mut p) };
        if rc < 0 {
            if rc as i32 == nghttp2_error_NGHTTP2_ERR_WOULDBLOCK {
                return Ok(&[]);
            }
            crate::logging::error(c"nghttp2 send failed", rc as i64);
            return Err(ConnError::Nghttp2(rc as i32));
        }
        if rc == 0 || p.is_null() {
            return Ok(&[]);
        }
        Ok(unsafe { core::slice::from_raw_parts(p, rc as usize) })
    }

    /// Attach the socket fd that the `NO_COPY` direct-send path writes to.
    /// Without it (fd < 0, e.g. in-process drivers) every message takes the
    /// copy path.
    pub fn set_fd(&mut self, fd: i32) {
        self.state.fd = fd;
    }

    /// True if ordered tail bytes are waiting to reach the socket. They MUST
    /// be flushed (`pending_bytes` + `consume_pending`) before writing any
    /// new `pull_send` chunk.
    pub fn has_pending(&self) -> bool {
        !self.state.out_pending.is_empty()
    }

    pub fn pending_bytes(&self) -> &[u8] {
        &self.state.out_pending
    }

    /// Drop the first `n` pending bytes (they reached the socket).
    pub fn consume_pending(&mut self, n: usize) {
        #[cfg(feature = "wirelog")]
        {
            // Disjoint field borrows: wirelog (mut) + out_pending (shared).
            let st = &mut *self.state;
            if let Some(wl) = st.wirelog.as_mut() {
                let upto = n.min(st.out_pending.len());
                if let Some(bytes) = st.out_pending.get(..upto) {
                    crate::wirelog::log(wl, crate::wirelog::Dir::ServerToClient, bytes);
                }
            }
        }
        if n >= self.state.out_pending.len() {
            self.state.out_pending.clear();
        } else {
            self.state.out_pending.drain(..n);
        }
    }

    /// Wire-log bytes that left through the I/O layer's `pull_send` path
    /// (the I/O layer copies the written slice because the chunk borrows
    /// this `Conn` — a dev-feature-only copy).
    #[cfg(feature = "wirelog")]
    pub fn wl_log_out(&mut self, bytes: &[u8]) {
        if let Some(wl) = self.state.wirelog.as_mut() {
            crate::wirelog::log(wl, crate::wirelog::Dir::ServerToClient, bytes);
        }
    }

    /// Stash ordered tail bytes that did not fit on the socket.
    pub fn stash_pending(&mut self, bytes: &[u8]) -> Result<(), ConnError> {
        self.state
            .out_pending
            .try_reserve(bytes.len())
            .map_err(|_| ConnError::OutOfMemory)?;
        self.state.out_pending.extend_from_slice(bytes);
        Ok(())
    }

    pub fn wants_read(&self) -> bool {
        unsafe { nghttp2_session_want_read(self.session.as_ptr()) != 0 }
    }
    pub fn wants_write(&self) -> bool {
        unsafe { nghttp2_session_want_write(self.session.as_ptr()) != 0 }
    }

    pub fn streams(&self) -> &[Box<StreamCtx>] {
        &self.state.streams
    }
}
