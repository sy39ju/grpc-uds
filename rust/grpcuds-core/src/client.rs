// SPDX-License-Identifier: MIT OR Apache-2.0
//! `no_std` blocking gRPC-over-UDS client (the `client` feature).
//!
//! Owns one UDS connection + an nghttp2 *client* session and drives unary and
//! server-streaming calls with blocking libc I/O — no Rust std, so it links
//! into a C application through `grpcuds-ffi` the same way the server does.
//! One call is in flight at a time ([`ClientConn`] borrows `&mut self`).

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::ffi::c_void;
use core::ptr;

use grpcuds_sys::{
    nghttp2_data_flag_NGHTTP2_DATA_FLAG_EOF as DATA_EOF, nghttp2_data_provider,
    nghttp2_data_source, nghttp2_frame, nghttp2_nv, nghttp2_session,
    nghttp2_session_callbacks_set_on_data_chunk_recv_callback as set_on_data,
    nghttp2_session_callbacks_set_on_header_callback as set_on_header,
    nghttp2_session_callbacks_set_on_stream_close_callback as set_on_close,
    nghttp2_session_get_stream_user_data, nghttp2_session_mem_recv, nghttp2_session_mem_send,
    nghttp2_session_want_read, nghttp2_session_want_write, nghttp2_submit_request,
    nghttp2_submit_rst_stream, nghttp2_submit_settings,
};

use crate::framing::{decode_header, encode_header, FRAME_HEADER_LEN};
use crate::headers::GrpcStatus;
use crate::session::{Callbacks, Session};

const MAX_MESSAGE_LEN: u32 = 4 * 1024 * 1024;

/// Client connect / setup failure (transport-level; a non-OK gRPC *status* is
/// reported through [`ClientCall`], not as a `ClientError`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientError {
    /// `connect(2)` to the socket failed.
    Connect,
    /// nghttp2 session/callbacks setup failed.
    Session,
    /// A socket read/write failed mid-call.
    Io,
    /// nghttp2 rejected the request submission or a frame.
    Protocol,
}

/// Per-call state, reached from the nghttp2 callbacks via the stream's
/// `stream_user_data`. Reused per call; its `Box` address is stable.
struct CallState {
    req: Vec<u8>,
    req_off: usize,
    http_status: Option<u32>,
    grpc_status: Option<i32>,
    grpc_message: Option<String>,
    inbuf: Vec<u8>,
    messages: alloc::collections::VecDeque<Vec<u8>>,
    closed: bool,
}

impl CallState {
    fn reset(&mut self, req: Vec<u8>) {
        self.req = req;
        self.req_off = 0;
        self.http_status = None;
        self.grpc_status = None;
        self.grpc_message = None;
        self.inbuf.clear();
        self.messages.clear();
        self.closed = false;
    }

    fn deframe(&mut self) {
        while let Ok(h) = decode_header(&self.inbuf, MAX_MESSAGE_LEN) {
            let total = FRAME_HEADER_LEN + h.payload_len as usize;
            if self.inbuf.len() < total {
                break;
            }
            let mut msg = Vec::new();
            if msg.try_reserve_exact(h.payload_len as usize).is_err() {
                break;
            }
            msg.extend_from_slice(&self.inbuf[FRAME_HEADER_LEN..total]);
            self.inbuf.drain(..total);
            self.messages.push_back(msg);
        }
    }
}

// ---- nghttp2 callbacks ------------------------------------------------------

unsafe fn state<'a>(session: *mut nghttp2_session, sid: i32) -> Option<&'a mut CallState> {
    let p = nghttp2_session_get_stream_user_data(session, sid);
    if p.is_null() {
        None
    } else {
        Some(&mut *(p as *mut CallState))
    }
}

unsafe extern "C" fn on_header(
    session: *mut nghttp2_session,
    frame: *const nghttp2_frame,
    name: *const u8,
    namelen: usize,
    value: *const u8,
    valuelen: usize,
    _flags: u8,
    _ud: *mut c_void,
) -> i32 {
    let Some(st) = state(session, (*frame).hd.stream_id) else {
        return 0;
    };
    let n = core::slice::from_raw_parts(name, namelen);
    let v = core::slice::from_raw_parts(value, valuelen);
    match n {
        b":status" => st.http_status = parse_u32(v),
        b"grpc-status" => st.grpc_status = parse_i32(v),
        b"grpc-message" => st.grpc_message = percent_decode(v),
        _ => {}
    }
    0
}

unsafe extern "C" fn on_data(
    session: *mut nghttp2_session,
    _flags: u8,
    sid: i32,
    data: *const u8,
    len: usize,
    _ud: *mut c_void,
) -> i32 {
    if let Some(st) = state(session, sid) {
        st.inbuf
            .extend_from_slice(core::slice::from_raw_parts(data, len));
        st.deframe();
    }
    0
}

unsafe extern "C" fn on_close(
    session: *mut nghttp2_session,
    sid: i32,
    _error: u32,
    _ud: *mut c_void,
) -> i32 {
    if let Some(st) = state(session, sid) {
        st.closed = true;
    }
    0
}

unsafe extern "C" fn data_read(
    _session: *mut nghttp2_session,
    _sid: i32,
    buf: *mut u8,
    length: usize,
    data_flags: *mut u32,
    source: *mut nghttp2_data_source,
    _ud: *mut c_void,
) -> isize {
    let st = &mut *((*source).ptr as *mut CallState);
    let remaining = st.req.len() - st.req_off;
    let n = remaining.min(length);
    if n > 0 {
        ptr::copy_nonoverlapping(st.req.as_ptr().add(st.req_off), buf, n);
        st.req_off += n;
    }
    if st.req_off == st.req.len() {
        *data_flags |= DATA_EOF;
    }
    n as isize
}

// ---- ClientConn -------------------------------------------------------------

/// Connect-retry policy for [`ClientConn::connect_wait`] — the stock-gRPC
/// connection-backoff shape, rescaled for local IPC where the wait target
/// is a daemon starting (ms..s), not a remote recovering (s..min):
/// 50 ms × 1.6 per retry, capped at 1 s, with ±20% jitter. Each individual
/// attempt is additionally bounded by [`CONNECT_ATTEMPT_TIMEOUT_MS`] (see
/// [`connect_uds`]).
const CONNECT_BACKOFF_INITIAL_MS: u64 = 50;
const CONNECT_BACKOFF_MAX_MS: u64 = 1_000;
const CONNECT_ATTEMPT_TIMEOUT_MS: i32 = 250;

/// ×1.6 in integer math (×8/5 — no_std core stays float-free), capped.
#[inline]
fn next_backoff_ms(cur: u64) -> u64 {
    (cur.saturating_mul(8) / 5).min(CONNECT_BACKOFF_MAX_MS)
}

/// ±20% jitter: scale `ms` to 80..=120 percent, picked from `r`.
#[inline]
fn apply_jitter(ms: u64, r: u64) -> u64 {
    ms.saturating_mul(80 + r % 41) / 100
}

/// xorshift64 — enough randomness for retry jitter, zero dependencies.
#[inline]
fn xorshift64(s: &mut u64) -> u64 {
    let mut x = *s;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *s = x;
    x
}

/// Sleep without std: poll(2) with no fds. EINTR just shortens the nap,
/// which only costs one extra connect attempt.
fn sleep_ms(ms: u64) {
    unsafe { libc::poll(ptr::null_mut(), 0, ms.min(i32::MAX as u64) as i32) };
}

/// A blocking gRPC-over-UDS client connection. See the [module docs](self).
pub struct ClientConn {
    fd: i32,
    session: *mut nghttp2_session,
    // Keep the Callbacks alive? nghttp2 copies them into the session, so they
    // can be freed after new_client — `Session` owns the session ptr.
    _session_owner: Session,
    state: Box<CallState>,
    /// Optional per-call timeout. Armed into `deadline` when a call is
    /// submitted; covers the WHOLE call (unary response or the full
    /// server-stream), matching gRPC deadline semantics.
    timeout_ms: Option<u32>,
    /// CLOCK_MONOTONIC expiry of the in-flight call, in ms.
    deadline_ms: Option<u64>,
    /// Stream id of the in-flight call (RST target on expiry).
    current_sid: i32,
    /// The connect path, kept for lazy reconnect (no trailing NUL).
    path: Vec<u8>,
    /// The transport died (EOF, I/O error, corrupt session). The next
    /// `unary`/`server_streaming` makes ONE reconnect attempt first.
    broken: bool,
    /// Dev-only wire logging stream (`None` = disabled at runtime).
    #[cfg(feature = "wirelog")]
    wl: Option<crate::wirelog::WirelogConn>,
}

impl ClientConn {
    /// Connect to a gRPC server on `path` (a NUL-terminated or raw byte path)
    /// and complete the client SETTINGS handshake. One attempt — see
    /// [`connect_wait`](Self::connect_wait) for retry-until-deadline.
    pub fn connect(path: &[u8]) -> Result<Self, ClientError> {
        let trimmed: &[u8] = match path.last() {
            Some(&0) => &path[..path.len() - 1],
            _ => path,
        };
        let mut path_copy = Vec::new();
        if path_copy.try_reserve_exact(trimmed.len()).is_err() {
            return Err(ClientError::Connect);
        }
        path_copy.extend_from_slice(trimmed);

        let fd = unsafe { connect_uds(path)? };

        let cbs = Callbacks::new().map_err(|_| {
            unsafe { libc::close(fd) };
            ClientError::Session
        })?;
        unsafe {
            set_on_header(cbs.as_ptr(), Some(on_header));
            set_on_data(cbs.as_ptr(), Some(on_data));
            set_on_close(cbs.as_ptr(), Some(on_close));
        }
        let session_owner = match unsafe { Session::new_client(&cbs, ptr::null_mut()) } {
            Ok(s) => s,
            Err(_) => {
                unsafe { libc::close(fd) };
                return Err(ClientError::Session);
            }
        };
        let session = session_owner.as_ptr();
        if unsafe { nghttp2_submit_settings(session, 0, ptr::null(), 0) } != 0 {
            unsafe { libc::close(fd) };
            return Err(ClientError::Session);
        }

        let mut conn = ClientConn {
            fd,
            session,
            _session_owner: session_owner,
            state: Box::new(CallState {
                req: Vec::new(),
                req_off: 0,
                http_status: None,
                grpc_status: None,
                grpc_message: None,
                inbuf: Vec::new(),
                messages: alloc::collections::VecDeque::new(),
                closed: false,
            }),
            timeout_ms: None,
            deadline_ms: None,
            current_sid: 0,
            path: path_copy,
            broken: false,
            // Opened before the first flush so the connection preface +
            // SETTINGS land in the capture (the HTTP/2 heuristic needs them).
            #[cfg(feature = "wirelog")]
            wl: crate::wirelog::conn_open(),
        };
        conn.flush_send().map_err(|_| ClientError::Io)?;
        crate::logging::debug(c"client connected", fd as i64);
        Ok(conn)
    }

    /// Like [`connect`](Self::connect), but keeps retrying with exponential
    /// backoff (50 ms × 1.6 up to a 1 s cap, ±20% jitter — bounded CPU, no
    /// busy-spin; each attempt itself bounded at 250 ms) until `timeout_ms`
    /// elapses. Covers the daemon-startup race: on UDS the socket file may
    /// not exist yet (ENOENT) or may exist before `listen` (ECONNREFUSED);
    /// both simply retry. `timeout_ms == 0` makes exactly one attempt, same
    /// as `connect`. Returns the LAST attempt's error.
    pub fn connect_wait(path: &[u8], timeout_ms: u32) -> Result<Self, ClientError> {
        let deadline = crate::monotonic_ms() + timeout_ms as u64;
        let mut backoff = CONNECT_BACKOFF_INITIAL_MS;
        // Jitter seed: clock + pid is plenty for retry spreading.
        let mut rng = (crate::monotonic_ms() << 17) ^ ((unsafe { libc::getpid() } as u64) | 1);
        loop {
            let err = match Self::connect(path) {
                Ok(conn) => return Ok(conn),
                Err(e) => e,
            };
            let now = crate::monotonic_ms();
            if now >= deadline {
                crate::logging::error(c"client connect timed out", timeout_ms as i64);
                return Err(err);
            }
            let nap = apply_jitter(backoff, xorshift64(&mut rng));
            sleep_ms(nap.min(deadline - now));
            backoff = next_backoff_ms(backoff);
        }
    }

    /// One reconnect attempt to the original path, replacing the dead fd +
    /// session. The per-client timeout setting survives; per-call state
    /// does not (the broken call already reported its error).
    fn reconnect_once(&mut self) -> Result<(), ClientError> {
        crate::logging::info(c"client reconnecting", 0);
        let mut fresh = Self::connect(&self.path)?;
        fresh.timeout_ms = self.timeout_ms;
        *self = fresh; // the old fd/session are closed by Drop
        crate::logging::info(c"client reconnected", self.fd as i64);
        Ok(())
    }

    /// A unary call: one request message in, one response message out. On
    /// transport success returns the [`ClientCall`] holding the response +
    /// status (inspect [`ClientCall::status`]); transport failures are `Err`.
    pub fn unary(&mut self, path: &[u8], req: &[u8]) -> Result<ClientCall<'_>, ClientError> {
        let sid = self.submit(path, req)?;
        loop {
            if self.state.closed {
                break;
            }
            self.pump()?;
        }
        Ok(ClientCall { conn: self, sid })
    }

    /// A server-streaming call. Drive the returned [`ClientCall`] with
    /// [`recv`](ClientCall::recv).
    pub fn server_streaming(
        &mut self,
        path: &[u8],
        req: &[u8],
    ) -> Result<ClientCall<'_>, ClientError> {
        let sid = self.submit(path, req)?;
        Ok(ClientCall { conn: self, sid })
    }

    /// Connection-level message pump for the in-flight call — used by the C
    /// ABI, which cannot hold a borrowing [`ClientCall`] across the boundary.
    /// Submit with `server_streaming` (the returned handle may be dropped),
    /// then call this repeatedly; `None` ends the stream.
    pub fn recv_current(&mut self) -> Result<Option<Vec<u8>>, ClientError> {
        loop {
            if let Some(m) = self.state.messages.pop_front() {
                return Ok(Some(m));
            }
            if self.state.closed || (self.is_idle() && self.state.grpc_status.is_some()) {
                return Ok(None);
            }
            self.pump()?;
        }
    }

    /// The numeric grpc-status of the last/in-flight call, if the server sent
    /// one.
    pub fn last_status(&self) -> Option<i32> {
        self.state.grpc_status
    }

    /// The `grpc-message` of the last/in-flight call, if any (owned copy).
    pub fn last_message(&self) -> Option<String> {
        self.state.grpc_message.clone()
    }

    fn submit(&mut self, path: &[u8], req: &[u8]) -> Result<i32, ClientError> {
        // Lazy reconnect, stock-gRPC IDLE-style: a connection that died
        // under a previous call gets ONE fresh connect before this call.
        // No retry loop here — failing fast keeps the call path from
        // blocking inside an application's event loop.
        if self.broken {
            self.reconnect_once()?;
        }
        let mut framed = Vec::new();
        if framed
            .try_reserve_exact(FRAME_HEADER_LEN + req.len())
            .is_err()
        {
            return Err(ClientError::Protocol);
        }
        let mut hdr = [0u8; FRAME_HEADER_LEN];
        encode_header(false, req.len() as u32, &mut hdr);
        framed.extend_from_slice(&hdr);
        framed.extend_from_slice(req);
        self.state.reset(framed);

        // grpc-timeout rides along when a timeout is set, so a conforming
        // server (grpcuds included) can stop work server-side too. Digits
        // are written by hand — core::fmt stays out of the core.
        let mut timeout_buf = [0u8; 11];
        let timeout_len = self.timeout_ms.map(|t| fmt_ms_value(t, &mut timeout_buf));
        let mut nva = [
            nv(b":method", b"POST"),
            nv(b":scheme", b"http"),
            nv(b":path", path),
            nv(b":authority", b"localhost"),
            nv(b"te", b"trailers"),
            nv(b"content-type", b"application/grpc"),
            nv(b"grpc-timeout", b"0m"), // placeholder, only sent when armed
        ];
        let mut nva_len = nva.len() - 1;
        if let Some(len) = timeout_len {
            nva[nva_len] = nv(b"grpc-timeout", &timeout_buf[..len]);
            nva_len += 1;
        }
        let ud = &mut *self.state as *mut CallState as *mut c_void;
        let provider = nghttp2_data_provider {
            source: nghttp2_data_source { ptr: ud },
            read_callback: Some(data_read),
        };
        let sid = unsafe {
            nghttp2_submit_request(
                self.session,
                ptr::null(),
                nva.as_ptr(),
                nva_len,
                &provider,
                ud,
            )
        };
        if sid <= 0 {
            return Err(ClientError::Protocol);
        }
        self.current_sid = sid;
        self.deadline_ms = self.timeout_ms.map(|t| crate::monotonic_ms() + t as u64);
        self.flush_send().map_err(|_| ClientError::Io)?;
        Ok(sid)
    }

    /// Set a per-call timeout in milliseconds (0 clears it). Applies to
    /// every call submitted afterwards and covers the whole call — the
    /// unary response, or the entire lifetime of a server-stream. On
    /// expiry the call fails locally with `DEADLINE_EXCEEDED` (4) and the
    /// stream is cancelled with RST_STREAM, so the server's cancel hook
    /// fires and any deferred work can stop.
    pub fn set_timeout_ms(&mut self, ms: u32) {
        self.timeout_ms = if ms == 0 { None } else { Some(ms) };
    }

    /// Fail the in-flight call locally with DEADLINE_EXCEEDED and cancel
    /// the stream so the server stops working on it.
    fn expire_call(&mut self) {
        crate::logging::info(c"call deadline exceeded", self.current_sid as i64);
        self.state.grpc_status = Some(GrpcStatus::DeadlineExceeded as i32);
        self.state.grpc_message = Some(String::from("deadline exceeded (client-side)"));
        self.state.closed = true;
        self.deadline_ms = None;
        if self.current_sid > 0 {
            // 0x8 = NGHTTP2_CANCEL.
            unsafe { nghttp2_submit_rst_stream(self.session, 0, self.current_sid, 0x8) };
            let _ = self.flush_send();
        }
    }

    fn pump(&mut self) -> Result<(), ClientError> {
        self.flush_send().map_err(|_| ClientError::Io)?;
        if let Some(deadline) = self.deadline_ms {
            let now = crate::monotonic_ms();
            let remaining = deadline.saturating_sub(now);
            if remaining == 0 {
                self.expire_call();
                return Ok(());
            }
            let mut pfd = libc::pollfd {
                fd: self.fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // Cap each wait at the remaining budget; EINTR retries on the
            // next pump iteration with a recomputed remainder.
            let timeout = remaining.min(i32::MAX as u64) as i32;
            let rc = unsafe { libc::poll(&mut pfd, 1, timeout) };
            if rc == 0 {
                self.expire_call();
                return Ok(());
            }
            if rc < 0 {
                let e = unsafe { *libc::__errno_location() };
                if e == libc::EINTR {
                    return Ok(());
                }
                self.state.closed = true;
                self.broken = true;
                return Err(ClientError::Io);
            }
        }
        let mut buf = [0u8; 16 * 1024];
        let n = unsafe { libc::read(self.fd, buf.as_mut_ptr() as *mut c_void, buf.len()) };
        if n == 0 {
            // EOF: the server closed the connection. The in-flight call ends
            // (status as received, if any); the NEXT call reconnects lazily.
            crate::logging::debug(c"server closed connection", self.fd as i64);
            self.state.closed = true;
            self.broken = true;
            return Ok(());
        }
        if n < 0 {
            let e = unsafe { *libc::__errno_location() };
            if e == libc::EINTR {
                return Ok(());
            }
            crate::logging::error(c"client read failed", e as i64);
            self.state.closed = true;
            self.broken = true;
            return Err(ClientError::Io);
        }
        #[cfg(feature = "wirelog")]
        if let Some(wl) = self.wl.as_mut() {
            if let Some(bytes) = buf.get(..n as usize) {
                crate::wirelog::log(wl, crate::wirelog::Dir::ServerToClient, bytes);
            }
        }
        let rc = unsafe { nghttp2_session_mem_recv(self.session, buf.as_ptr(), n as usize) };
        if rc < 0 {
            // The session state is no longer trustworthy.
            crate::logging::error(c"nghttp2 recv failed", rc as i64);
            self.broken = true;
            return Err(ClientError::Protocol);
        }
        Ok(())
    }

    fn flush_send(&mut self) -> Result<(), ClientError> {
        loop {
            let mut p: *const u8 = ptr::null();
            let n = unsafe { nghttp2_session_mem_send(self.session, &mut p) };
            if n <= 0 || p.is_null() {
                break;
            }
            let bytes = unsafe { core::slice::from_raw_parts(p, n as usize) };
            if write_all(self.fd, bytes).is_err() {
                self.broken = true;
                return Err(ClientError::Io);
            }
            #[cfg(feature = "wirelog")]
            if let Some(wl) = self.wl.as_mut() {
                crate::wirelog::log(wl, crate::wirelog::Dir::ClientToServer, bytes);
            }
        }
        Ok(())
    }

    fn is_idle(&self) -> bool {
        unsafe {
            nghttp2_session_want_read(self.session) == 0
                && nghttp2_session_want_write(self.session) == 0
        }
    }
}

impl Drop for ClientConn {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
        // `_session_owner` frees the nghttp2 session.
    }
}

// ---- ClientCall (response/stream handle) ------------------------------------

/// An in-flight or completed call on a [`ClientConn`].
pub struct ClientCall<'a> {
    conn: &'a mut ClientConn,
    sid: i32,
}

impl ClientCall<'_> {
    /// The gRPC status code (0 = OK). Meaningful once the stream has closed.
    pub fn status(&self) -> GrpcStatus {
        crate::headers::grpc_status_from_i32(self.conn.state.grpc_status.unwrap_or(2))
    }

    /// The raw numeric grpc-status, or `None` if the server never sent one.
    pub fn status_code(&self) -> Option<i32> {
        self.conn.state.grpc_status
    }

    /// The `grpc-message` trailer, if any.
    pub fn message(&self) -> Option<&str> {
        self.conn.state.grpc_message.as_deref()
    }

    /// Receive the next response message; `None` once the stream is drained
    /// and closed. Blocks for more network data while the stream is open.
    pub fn recv(&mut self) -> Result<Option<Vec<u8>>, ClientError> {
        loop {
            if let Some(m) = self.conn.state.messages.pop_front() {
                return Ok(Some(m));
            }
            if self.conn.state.closed
                || (self.conn.is_idle() && self.conn.state.grpc_status.is_some())
            {
                return Ok(None);
            }
            // Suppress unused-field warning on builds that never read sid.
            let _ = self.sid;
            self.conn.pump()?;
        }
    }
}

// ---- libc helpers -----------------------------------------------------------

unsafe fn connect_uds(path: &[u8]) -> Result<i32, ClientError> {
    let mut addr: libc::sockaddr_un = core::mem::zeroed();
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let trimmed: &[u8] = match path.last() {
        Some(&0) => &path[..path.len() - 1],
        _ => path,
    };
    if trimmed.is_empty() || trimmed.len() > addr.sun_path.len() - 1 {
        return Err(ClientError::Connect);
    }
    for (i, &b) in trimmed.iter().enumerate() {
        addr.sun_path[i] = b as libc::c_char;
    }
    let addr_len =
        (core::mem::size_of::<libc::sa_family_t>() + trimmed.len() + 1) as libc::socklen_t;

    // NONBLOCK so no single attempt can hang: a blocking UDS connect WAITS
    // when the listener's backlog is full. Nonblocking, that case returns
    // EAGAIN immediately (the kernel does not pursue AF_UNIX connects in
    // the background) and the attempt fails — connect_wait's backoff
    // retries it. EINPROGRESS (not the AF_UNIX norm, but be liberal) gets
    // a poll bounded by CONNECT_ATTEMPT_TIMEOUT_MS + an SO_ERROR check.
    let fd = libc::socket(
        libc::AF_UNIX,
        libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
        0,
    );
    if fd < 0 {
        return Err(ClientError::Connect);
    }
    if libc::connect(fd, &addr as *const _ as *const libc::sockaddr, addr_len) < 0 {
        if *libc::__errno_location() != libc::EINPROGRESS {
            libc::close(fd);
            return Err(ClientError::Connect);
        }
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLOUT,
            revents: 0,
        };
        let mut soerr: libc::c_int = 0;
        let mut len = core::mem::size_of::<libc::c_int>() as libc::socklen_t;
        if libc::poll(&mut pfd, 1, CONNECT_ATTEMPT_TIMEOUT_MS) != 1
            || libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                &mut soerr as *mut _ as *mut c_void,
                &mut len,
            ) != 0
            || soerr != 0
        {
            libc::close(fd);
            return Err(ClientError::Connect);
        }
    }
    // The client's I/O is blocking by design — drop O_NONBLOCK now.
    let flags = libc::fcntl(fd, libc::F_GETFL);
    if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) < 0 {
        libc::close(fd);
        return Err(ClientError::Connect);
    }
    Ok(fd)
}

fn write_all(fd: i32, mut data: &[u8]) -> Result<(), ()> {
    while !data.is_empty() {
        // send + MSG_NOSIGNAL, not write(2): a write to a server that died
        // raises SIGPIPE, which kills a C host process outright (there is
        // no Rust std runtime ignoring it). EPIPE comes back as an error
        // instead, and the lazy-reconnect path can do its job.
        let n = unsafe {
            libc::send(
                fd,
                data.as_ptr() as *const c_void,
                data.len(),
                libc::MSG_NOSIGNAL,
            )
        };
        if n < 0 {
            let e = unsafe { *libc::__errno_location() };
            if e == libc::EINTR {
                continue;
            }
            crate::logging::error(c"client write failed", e as i64);
            return Err(());
        }
        if n == 0 {
            return Err(());
        }
        data = &data[n as usize..];
    }
    Ok(())
}

fn nv(name: &[u8], value: &[u8]) -> nghttp2_nv {
    nghttp2_nv {
        name: name.as_ptr() as *mut u8,
        value: value.as_ptr() as *mut u8,
        namelen: name.len(),
        valuelen: value.len(),
        flags: 0,
    }
}

/// Render `ms` as a `grpc-timeout` value (`<digits>m`) without core::fmt —
/// and without indexing panics (panic paths drag the fmt machinery into the
/// no_std staticlib). `buf` holds 10 digits of u32 + the unit. Returns the
/// length, 0 only if the buffer types were ever shrunk (impossible today).
fn fmt_ms_value(mut v: u32, buf: &mut [u8; 11]) -> usize {
    let mut tmp = [0u8; 10];
    let mut i = 0;
    for slot in tmp.iter_mut() {
        *slot = b'0' + (v % 10) as u8;
        v /= 10;
        i += 1;
        if v == 0 {
            break;
        }
    }
    let mut len = 0;
    for slot in buf.iter_mut() {
        if i == 0 {
            *slot = b'm';
            return len + 1;
        }
        i -= 1;
        // i < 10 by construction; `get` keeps the no-panic property provable.
        *slot = tmp.get(i).copied().unwrap_or(b'0');
        len += 1;
    }
    len
}

fn parse_u32(v: &[u8]) -> Option<u32> {
    core::str::from_utf8(v).ok().and_then(|s| s.parse().ok())
}
fn parse_i32(v: &[u8]) -> Option<i32> {
    core::str::from_utf8(v).ok().and_then(|s| s.parse().ok())
}

fn percent_decode(v: &[u8]) -> Option<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < v.len() {
        if v[i] == b'%' && i + 2 < v.len() {
            let hi = (v[i + 1] as char).to_digit(16);
            let lo = (v[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(v[i]);
        i += 1;
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framing::encode_header;

    #[test]
    fn fmt_ms_value_renders_grpc_timeout_values() {
        let mut buf = [0u8; 11];
        for (v, want) in [
            (0u32, &b"0m"[..]),
            (7, b"7m"),
            (200, b"200m"),
            (4_999, b"4999m"),
            (u32::MAX, b"4294967295m"),
        ] {
            let len = fmt_ms_value(v, &mut buf);
            assert_eq!(&buf[..len], want, "value {v}");
        }
    }

    fn call_state() -> CallState {
        CallState {
            req: Vec::new(),
            req_off: 0,
            http_status: None,
            grpc_status: None,
            grpc_message: None,
            inbuf: Vec::new(),
            messages: alloc::collections::VecDeque::new(),
            closed: false,
        }
    }

    fn frame(payload: &[u8]) -> Vec<u8> {
        let mut h = [0u8; FRAME_HEADER_LEN];
        encode_header(false, payload.len() as u32, &mut h);
        let mut out = h.to_vec();
        out.extend_from_slice(payload);
        out
    }

    // ---- deframe ------------------------------------------------------------

    #[test]
    fn deframe_extracts_a_single_message() {
        let mut st = call_state();
        st.inbuf.extend_from_slice(&frame(b"hello"));
        st.deframe();
        assert_eq!(st.messages.pop_front().as_deref(), Some(&b"hello"[..]));
        assert!(st.messages.is_empty());
        assert!(st.inbuf.is_empty(), "consumed bytes must leave the buffer");
    }

    #[test]
    fn deframe_handles_an_empty_payload() {
        let mut st = call_state();
        st.inbuf.extend_from_slice(&frame(b""));
        st.deframe();
        assert_eq!(st.messages.pop_front().as_deref(), Some(&b""[..]));
        assert!(st.inbuf.is_empty());
    }

    #[test]
    fn deframe_waits_for_a_partial_header() {
        let mut st = call_state();
        st.inbuf.extend_from_slice(&frame(b"hello")[..3]);
        st.deframe();
        assert!(st.messages.is_empty());
        assert_eq!(st.inbuf.len(), 3, "partial header must stay buffered");
    }

    #[test]
    fn deframe_waits_for_a_partial_payload_then_completes() {
        let full = frame(b"split across reads");
        let mut st = call_state();
        st.inbuf.extend_from_slice(&full[..FRAME_HEADER_LEN + 4]);
        st.deframe();
        assert!(
            st.messages.is_empty(),
            "payload incomplete — no message yet"
        );

        st.inbuf.extend_from_slice(&full[FRAME_HEADER_LEN + 4..]);
        st.deframe();
        assert_eq!(
            st.messages.pop_front().as_deref(),
            Some(&b"split across reads"[..])
        );
        assert!(st.inbuf.is_empty());
    }

    #[test]
    fn deframe_splits_back_to_back_messages_in_order() {
        let mut st = call_state();
        st.inbuf.extend_from_slice(&frame(b"first"));
        st.inbuf.extend_from_slice(&frame(b"second"));
        st.inbuf.extend_from_slice(&frame(b"third"));
        st.deframe();
        assert_eq!(st.messages.pop_front().as_deref(), Some(&b"first"[..]));
        assert_eq!(st.messages.pop_front().as_deref(), Some(&b"second"[..]));
        assert_eq!(st.messages.pop_front().as_deref(), Some(&b"third"[..]));
        assert!(st.inbuf.is_empty());
    }

    #[test]
    fn deframe_stops_at_a_compressed_flag() {
        // grpcuds never negotiates compression, so a compressed flag is a
        // peer bug; deframe must not consume or mis-parse the frame.
        let mut st = call_state();
        let mut h = [0u8; FRAME_HEADER_LEN];
        encode_header(true, 3, &mut h);
        st.inbuf.extend_from_slice(&h);
        st.inbuf.extend_from_slice(b"abc");
        st.deframe();
        assert!(st.messages.is_empty());
        assert_eq!(st.inbuf.len(), FRAME_HEADER_LEN + 3, "nothing consumed");
    }

    #[test]
    fn deframe_stops_at_an_oversized_length() {
        let mut st = call_state();
        let mut h = [0u8; FRAME_HEADER_LEN];
        encode_header(false, MAX_MESSAGE_LEN + 1, &mut h);
        st.inbuf.extend_from_slice(&h);
        st.deframe();
        assert!(st.messages.is_empty());
        assert_eq!(st.inbuf.len(), FRAME_HEADER_LEN, "nothing consumed");
    }

    // ---- per-call reset -------------------------------------------------------

    #[test]
    fn reset_clears_per_call_state() {
        let mut st = call_state();
        st.req_off = 7;
        st.http_status = Some(200);
        st.grpc_status = Some(5);
        st.grpc_message = Some("nope".into());
        st.inbuf.extend_from_slice(b"junk");
        st.messages.push_back(b"stale".to_vec());
        st.closed = true;

        st.reset(b"new-request".to_vec());
        assert_eq!(st.req, b"new-request");
        assert_eq!(st.req_off, 0);
        assert_eq!(st.http_status, None);
        assert_eq!(st.grpc_status, None);
        assert_eq!(st.grpc_message, None);
        assert!(st.inbuf.is_empty());
        assert!(st.messages.is_empty());
        assert!(!st.closed);
    }

    // ---- trailer-value helpers --------------------------------------------------

    #[test]
    fn parse_helpers_accept_digits_and_reject_garbage() {
        assert_eq!(parse_u32(b"200"), Some(200));
        assert_eq!(parse_i32(b"-1"), Some(-1));
        assert_eq!(parse_i32(b"12"), Some(12));
        assert_eq!(parse_u32(b""), None);
        assert_eq!(parse_u32(b"12a"), None);
        assert_eq!(parse_i32(b"\xff\xff"), None, "non-utf8 is rejected");
    }

    // ---- connect_wait + lazy reconnect ----------------------------------------

    #[test]
    fn next_backoff_follows_the_1_6_curve_to_the_cap() {
        let mut b = CONNECT_BACKOFF_INITIAL_MS;
        let mut seq = Vec::new();
        for _ in 0..9 {
            seq.push(b);
            b = next_backoff_ms(b);
        }
        // 50 × 1.6 (integer ×8/5), capped at 1000.
        assert_eq!(seq, [50, 80, 128, 204, 326, 521, 833, 1000, 1000]);
    }

    #[test]
    fn jitter_stays_within_20_percent_and_varies() {
        let mut s = 0x1234_5678_9abc_def0u64;
        let mut seen = Vec::new();
        for _ in 0..1_000 {
            let j = apply_jitter(1_000, xorshift64(&mut s));
            assert!((800..=1_200).contains(&j), "out of band: {j}");
            seen.push(j);
        }
        seen.dedup();
        assert!(seen.len() > 10, "jitter must actually vary");
    }

    fn test_sock_path(tag: &str) -> Vec<u8> {
        let pid = unsafe { libc::getpid() };
        let mut p = alloc::format!("/tmp/grpcuds-client-{tag}-{pid}.sock").into_bytes();
        p.push(0);
        p
    }

    fn bind_listener(path_nul: &[u8]) -> i32 {
        unsafe {
            libc::unlink(path_nul.as_ptr() as *const libc::c_char);
            let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
            assert!(fd >= 0);
            let mut addr: libc::sockaddr_un = core::mem::zeroed();
            addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
            let trimmed = &path_nul[..path_nul.len() - 1];
            for (i, &b) in trimmed.iter().enumerate() {
                addr.sun_path[i] = b as libc::c_char;
            }
            let len =
                (core::mem::size_of::<libc::sa_family_t>() + trimmed.len() + 1) as libc::socklen_t;
            assert_eq!(
                libc::bind(fd, &addr as *const _ as *const libc::sockaddr, len),
                0
            );
            assert_eq!(libc::listen(fd, 8), 0);
            fd
        }
    }

    fn drop_listener(fd: i32, path_nul: &[u8]) {
        unsafe {
            libc::close(fd);
            libc::unlink(path_nul.as_ptr() as *const libc::c_char);
        }
    }

    #[test]
    fn connect_wait_zero_timeout_is_one_fast_attempt() {
        let t0 = crate::monotonic_ms();
        let r = ClientConn::connect_wait(b"/tmp/grpcuds-no-such-dir/x.sock\0", 0);
        assert!(matches!(r, Err(ClientError::Connect)));
        assert!(crate::monotonic_ms() - t0 < 100, "0 must not wait");
    }

    #[test]
    fn connect_wait_retries_until_the_deadline_then_fails() {
        let t0 = crate::monotonic_ms();
        let r = ClientConn::connect_wait(b"/tmp/grpcuds-no-such-dir/x.sock\0", 120);
        let dt = crate::monotonic_ms() - t0;
        assert!(matches!(r, Err(ClientError::Connect)));
        assert!(dt >= 120, "gave up early: {dt}ms");
        assert!(dt < 2_000, "overshot the budget: {dt}ms");
    }

    #[test]
    fn connect_wait_succeeds_against_a_live_listener() {
        let path = test_sock_path("wait-ok");
        let lfd = bind_listener(&path);
        assert!(ClientConn::connect_wait(&path, 1_000).is_ok());
        drop_listener(lfd, &path);
    }

    #[test]
    fn broken_client_reconnects_on_the_next_call() {
        let path = test_sock_path("reconn");
        let lfd = bind_listener(&path);
        let mut conn = ClientConn::connect(&path).expect("first connect");

        // The daemon "restarts": old listener gone, a new one on the path.
        drop_listener(lfd, &path);
        conn.broken = true; // what pump()/flush_send() record on EOF/EPIPE
        let lfd2 = bind_listener(&path);

        // submit() must reconnect first, then succeed on the fresh socket.
        assert!(conn.server_streaming(b"/svc/Method", b"req").is_ok());
        assert!(!conn.broken, "fresh connection");
        drop_listener(lfd2, &path);
    }

    #[test]
    fn broken_client_without_a_server_fails_fast_with_connect() {
        let path = test_sock_path("reconn-fail");
        let lfd = bind_listener(&path);
        let mut conn = ClientConn::connect(&path).expect("first connect");
        drop_listener(lfd, &path);
        conn.broken = true;
        assert!(matches!(
            conn.server_streaming(b"/svc/M", b""),
            Err(ClientError::Connect)
        ));
        assert!(conn.broken, "still broken — the next call retries again");
    }

    #[test]
    fn server_death_marks_the_connection_broken() {
        let path = test_sock_path("eof");
        let lfd = bind_listener(&path);
        let mut conn = ClientConn::connect(&path).expect("connect");
        drop_listener(lfd, &path);
        // The call sees EPIPE on write (as an error, NOT a SIGPIPE — that
        // is the MSG_NOSIGNAL contract) or EOF on read; either way the
        // transport gets flagged for lazy reconnect.
        let _ = conn.unary(b"/svc/M", b"x");
        assert!(conn.broken);
    }

    #[test]
    fn percent_decode_handles_the_grpc_message_encoding() {
        // Plain text passes through.
        assert_eq!(percent_decode(b"not found").as_deref(), Some("not found"));
        // %XX escapes decode (the gRPC spec percent-encodes grpc-message).
        assert_eq!(
            percent_decode(b"no%20such%20device").as_deref(),
            Some("no such device")
        );
        assert_eq!(percent_decode(b"%41%42").as_deref(), Some("AB"));
        // Truncated or non-hex escapes pass through as literal bytes.
        assert_eq!(percent_decode(b"100%").as_deref(), Some("100%"));
        assert_eq!(percent_decode(b"%2").as_deref(), Some("%2"));
        assert_eq!(percent_decode(b"50%zz").as_deref(), Some("50%zz"));
        // Decoding that produces invalid UTF-8 is rejected, not garbled.
        assert_eq!(percent_decode(b"%ff"), None);
    }
}
