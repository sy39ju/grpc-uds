// SPDX-License-Identifier: MIT OR Apache-2.0
//! A blocking gRPC client over UNIX domain sockets (the `client` feature).
//!
//! Dials a grpcuds server — or any stock gRPC server that listens on a UDS —
//! and runs unary and server-streaming calls. It owns one connection and an
//! nghttp2 *client* session; calls take `&mut self`, so exactly one call is in
//! flight at a time (no concurrent multiplexing, which keeps the blocking I/O
//! model simple).
//!
//! ```no_run
//! # #[cfg(all(feature = "client", feature = "server"))]
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use grpcuds::Client;
//! let mut client = Client::connect("/run/echo.sock")?;
//! let reply = client.unary("/echo.Echo/Unary", b"ping")?;     // Vec<u8>
//! let mut stream = client.server_streaming("/scan.Scan/All", b"")?;
//! while let Some(msg) = stream.message()? { /* … */ }
//! # Ok(())
//! # }
//! # #[cfg(not(all(feature = "client", feature = "server")))] fn main() {}
//! ```

use std::collections::VecDeque;
use std::ffi::c_void;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::ptr;

use grpcuds_core::{decode_header, encode_header, FRAME_HEADER_LEN};
use grpcuds_sys::{
    nghttp2_data_provider, nghttp2_data_source, nghttp2_frame, nghttp2_nv, nghttp2_session,
    nghttp2_session_callbacks, nghttp2_session_callbacks_del, nghttp2_session_callbacks_new,
    nghttp2_session_callbacks_set_on_data_chunk_recv_callback as set_on_data,
    nghttp2_session_callbacks_set_on_header_callback as set_on_header,
    nghttp2_session_callbacks_set_on_stream_close_callback as set_on_close,
    nghttp2_session_client_new, nghttp2_session_del, nghttp2_session_get_stream_user_data,
    nghttp2_session_mem_recv, nghttp2_session_mem_send, nghttp2_session_want_read,
    nghttp2_session_want_write, nghttp2_submit_request, nghttp2_submit_rst_stream,
    nghttp2_submit_settings,
};

use crate::{Error, Status, StatusCode};

const MAX_MESSAGE_LEN: u32 = 4 * 1024 * 1024;

/// Per-call state, reachable from the nghttp2 callbacks via the stream's
/// `stream_user_data`. One instance is reused per call on the connection; its
/// `Box` address is stable for the call's lifetime.
struct CallState {
    // request body (already gRPC-framed) + how much the data provider has read
    req: Vec<u8>,
    req_off: usize,
    // response
    http_status: Option<u32>,
    grpc_status: Option<i32>,
    grpc_message: Option<String>,
    inbuf: Vec<u8>,              // raw DATA bytes not yet split into messages
    messages: VecDeque<Vec<u8>>, // fully-deframed response messages
    closed: bool,                // stream closed (END_STREAM or RST)
    close_error: u32,            // nghttp2 close error code (0 = none)
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
        self.close_error = 0;
    }

    /// Pull complete gRPC frames out of `inbuf` into `messages`.
    fn deframe(&mut self) {
        while let Ok(h) = decode_header(&self.inbuf, MAX_MESSAGE_LEN) {
            let total = FRAME_HEADER_LEN + h.payload_len as usize;
            if self.inbuf.len() < total {
                break;
            }
            let msg = self.inbuf[FRAME_HEADER_LEN..total].to_vec();
            self.inbuf.drain(..total);
            self.messages.push_back(msg);
        }
    }
}

// ---- nghttp2 callbacks ------------------------------------------------------

unsafe fn call_state<'a>(
    session: *mut nghttp2_session,
    stream_id: i32,
) -> Option<&'a mut CallState> {
    let p = nghttp2_session_get_stream_user_data(session, stream_id);
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
    let sid = (*frame).hd.stream_id;
    let Some(st) = call_state(session, sid) else {
        return 0;
    };
    let n = std::slice::from_raw_parts(name, namelen);
    let v = std::slice::from_raw_parts(value, valuelen);
    match n {
        b":status" => {
            st.http_status = std::str::from_utf8(v).ok().and_then(|s| s.parse().ok());
        }
        b"grpc-status" => {
            st.grpc_status = std::str::from_utf8(v).ok().and_then(|s| s.parse().ok());
        }
        b"grpc-message" => {
            st.grpc_message = percent_decode(v);
        }
        _ => {}
    }
    0
}

unsafe extern "C" fn on_data(
    session: *mut nghttp2_session,
    _flags: u8,
    stream_id: i32,
    data: *const u8,
    len: usize,
    _ud: *mut c_void,
) -> i32 {
    if let Some(st) = call_state(session, stream_id) {
        st.inbuf
            .extend_from_slice(std::slice::from_raw_parts(data, len));
        st.deframe();
    }
    0
}

unsafe extern "C" fn on_close(
    session: *mut nghttp2_session,
    stream_id: i32,
    error_code: u32,
    _ud: *mut c_void,
) -> i32 {
    if let Some(st) = call_state(session, stream_id) {
        st.closed = true;
        st.close_error = error_code;
    }
    0
}

unsafe extern "C" fn data_read(
    _session: *mut nghttp2_session,
    _stream_id: i32,
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
        *data_flags |= grpcuds_sys::nghttp2_data_flag_NGHTTP2_DATA_FLAG_EOF;
    }
    n as isize
}

/// gRPC percent-decoding for `grpc-message` (best effort; invalid escapes are
/// passed through literally).
fn percent_decode(v: &[u8]) -> Option<String> {
    let mut out = Vec::with_capacity(v.len());
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

// ---- Client -----------------------------------------------------------------

/// Connect-retry policy for [`Client::connect_wait`] — the stock-gRPC
/// connection-backoff shape, rescaled for local IPC: 50 ms × 1.6 per retry,
/// capped at 1 s, ±20% jitter. Each individual attempt is additionally
/// bounded at 250 ms (a blocking UDS connect can otherwise WAIT when the
/// listener's backlog is full — see `connect_bounded`). Mirrors
/// `grpcuds-core::client`; keep the two in sync.
const CONNECT_BACKOFF_INITIAL_MS: u64 = 50;
const CONNECT_BACKOFF_MAX_MS: u64 = 1_000;
const CONNECT_ATTEMPT_TIMEOUT_MS: i32 = 250;

/// ×1.6 in integer math (×8/5), capped.
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

/// connect(2) bounded at [`CONNECT_ATTEMPT_TIMEOUT_MS`]: nonblocking
/// connect, then (for an EINPROGRESS straggler) a bounded poll + SO_ERROR
/// check. A full listener backlog comes back as EAGAIN instead of blocking
/// indefinitely; the returned stream is switched back to blocking mode.
fn connect_bounded(path: &Path) -> std::io::Result<UnixStream> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;

    let bytes = path.as_os_str().as_bytes();
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    if bytes.is_empty() || bytes.len() > addr.sun_path.len() - 1 {
        return Err(std::io::Error::from(ErrorKind::InvalidInput));
    }
    for (i, &b) in bytes.iter().enumerate() {
        addr.sun_path[i] = b as libc::c_char;
    }
    let addr_len = (std::mem::size_of::<libc::sa_family_t>() + bytes.len() + 1) as libc::socklen_t;

    let fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // From here the fd is owned: any error path drops the stream (closing it).
    let sock = unsafe { UnixStream::from_raw_fd(fd) };
    let rc = unsafe { libc::connect(fd, &addr as *const _ as *const libc::sockaddr, addr_len) };
    if rc < 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EINPROGRESS) {
            return Err(err);
        }
        let mut pfd = libc::pollfd {
            fd: sock.as_raw_fd(),
            events: libc::POLLOUT,
            revents: 0,
        };
        if unsafe { libc::poll(&mut pfd, 1, CONNECT_ATTEMPT_TIMEOUT_MS) } != 1 {
            return Err(std::io::Error::from(ErrorKind::TimedOut));
        }
        if let Some(e) = sock.take_error()? {
            return Err(e);
        }
    }
    sock.set_nonblocking(false)?;
    Ok(sock)
}

/// A blocking gRPC-over-UDS client: [`connect`](Self::connect), then
/// [`unary`](Self::unary) / [`server_streaming`](Self::server_streaming).
pub struct Client {
    sock: UnixStream,
    session: *mut nghttp2_session,
    state: Box<CallState>,
    /// Optional per-call timeout, armed into `deadline_at` at submit.
    timeout: Option<std::time::Duration>,
    /// Expiry of the in-flight call.
    deadline_at: Option<std::time::Instant>,
    /// Stream id of the in-flight call (RST target on expiry).
    current_sid: i32,
    /// The connect path, kept for lazy reconnect.
    path: std::path::PathBuf,
    /// The transport died (EOF, I/O error, corrupt session). The next
    /// `unary`/`server_streaming` makes ONE reconnect attempt first.
    broken: bool,
    /// Dev-only wire logging stream (`None` = disabled at runtime).
    #[cfg(feature = "wirelog")]
    wl: Option<grpcuds_core::wirelog::WirelogConn>,
}

impl Client {
    /// Connect to a gRPC server listening on the given UDS path and complete
    /// the HTTP/2 handshake (client SETTINGS).
    pub fn connect(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref();
        let sock = connect_bounded(path).map_err(|source| Error::Connect {
            path: path.to_path_buf(),
            source,
        })?;

        let session = unsafe {
            let mut cbs: *mut nghttp2_session_callbacks = ptr::null_mut();
            if nghttp2_session_callbacks_new(&mut cbs) != 0 {
                return Err(Error::Session);
            }
            set_on_header(cbs, Some(on_header));
            set_on_data(cbs, Some(on_data));
            set_on_close(cbs, Some(on_close));
            let mut session: *mut nghttp2_session = ptr::null_mut();
            let rc = nghttp2_session_client_new(&mut session, cbs, ptr::null_mut());
            nghttp2_session_callbacks_del(cbs);
            if rc != 0 || session.is_null() {
                return Err(Error::Session);
            }
            if nghttp2_submit_settings(session, 0, ptr::null(), 0) != 0 {
                nghttp2_session_del(session);
                return Err(Error::Session);
            }
            session
        };

        let mut client = Client {
            sock,
            session,
            state: Box::new(CallState {
                req: Vec::new(),
                req_off: 0,
                http_status: None,
                grpc_status: None,
                grpc_message: None,
                inbuf: Vec::new(),
                messages: VecDeque::new(),
                closed: false,
                close_error: 0,
            }),
            timeout: None,
            deadline_at: None,
            current_sid: 0,
            path: path.to_path_buf(),
            broken: false,
            // Opened before the first flush so the connection preface +
            // SETTINGS land in the capture.
            #[cfg(feature = "wirelog")]
            wl: grpcuds_core::wirelog::conn_open(),
        };
        // Flush the initial SETTINGS.
        client.flush_send().map_err(Error::Io)?;
        Ok(client)
    }

    /// Like [`connect`](Self::connect), but keeps retrying with exponential
    /// backoff (50 ms × 1.6 up to a 1 s cap, ±20% jitter — bounded CPU, no
    /// busy-spin; each attempt itself bounded at 250 ms) until `timeout`
    /// elapses. Covers the daemon-startup race: on UDS the socket file may
    /// not exist yet, or may exist before the server's `listen` — both
    /// simply retry. The rough equivalent of stock gRPC's wait_for_ready,
    /// scoped to connection establishment. A zero `timeout` makes exactly
    /// one attempt. Returns the LAST attempt's error.
    pub fn connect_wait(
        path: impl AsRef<Path>,
        timeout: std::time::Duration,
    ) -> Result<Self, Error> {
        let path = path.as_ref();
        let deadline = std::time::Instant::now() + timeout;
        let mut backoff = CONNECT_BACKOFF_INITIAL_MS;
        let mut rng = std::time::UNIX_EPOCH
            .elapsed()
            .map(|d| d.subsec_nanos() as u64)
            .unwrap_or(1)
            .wrapping_shl(17)
            ^ ((std::process::id() as u64) | 1);
        loop {
            let err = match Self::connect(path) {
                Ok(client) => return Ok(client),
                Err(e) => e,
            };
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Err(err);
            }
            let nap = std::time::Duration::from_millis(apply_jitter(backoff, xorshift64(&mut rng)));
            std::thread::sleep(nap.min(remaining));
            backoff = next_backoff_ms(backoff);
        }
    }

    /// One reconnect attempt to the original path, replacing the dead
    /// socket + session. The per-client timeout setting survives; per-call
    /// state does not (the broken call already reported its error).
    fn reconnect_once(&mut self) -> Result<(), Status> {
        grpcuds_core::logging::info(c"client reconnecting", 0);
        match Self::connect(&self.path) {
            Ok(mut fresh) => {
                fresh.timeout = self.timeout;
                *self = fresh; // the old socket/session are closed by Drop
                grpcuds_core::logging::info(c"client reconnected", 0);
                Ok(())
            }
            Err(e) => Err(Status::new(StatusCode::Unavailable, e.to_string())),
        }
    }

    /// A unary call: one request message in, exactly one response message out.
    /// A non-OK gRPC status (or a transport failure) is returned as `Err`.
    pub fn unary(&mut self, path: &str, request: &[u8]) -> Result<Vec<u8>, Status> {
        let sid = self.submit(path, request)?;
        // Drive until the stream closes.
        loop {
            if self.state.closed {
                break;
            }
            if !self.state.messages.is_empty() && self.state.grpc_status.is_some() {
                break;
            }
            self.pump(sid)?;
        }
        self.final_status()?;
        self.state
            .messages
            .pop_front()
            .ok_or_else(|| Status::new(StatusCode::Internal, "unary response had no message"))
    }

    /// A server-streaming call: drive the returned [`ServerStreaming`] with
    /// [`message`](ServerStreaming::message).
    pub fn server_streaming(
        &mut self,
        path: &str,
        request: &[u8],
    ) -> Result<ServerStreaming<'_>, Status> {
        let sid = self.submit(path, request)?;
        Ok(ServerStreaming {
            client: self,
            stream_id: sid,
        })
    }

    fn submit(&mut self, path: &str, request: &[u8]) -> Result<i32, Status> {
        // Lazy reconnect, stock-gRPC IDLE-style: a connection that died
        // under a previous call gets ONE fresh connect before this call.
        // No retry loop — failing fast keeps the call path non-blocking.
        if self.broken {
            self.reconnect_once()?;
        }
        // Frame the request (single message) and arm the per-call state.
        let mut framed = Vec::with_capacity(FRAME_HEADER_LEN + request.len());
        let mut hdr = [0u8; FRAME_HEADER_LEN];
        encode_header(false, request.len() as u32, &mut hdr);
        framed.extend_from_slice(&hdr);
        framed.extend_from_slice(request);
        self.state.reset(framed);

        let path_b = path.as_bytes();
        // grpc-timeout rides along when a timeout is set, so a conforming
        // server (grpcuds included) can stop work server-side too.
        let timeout_value = self
            .timeout
            .map(|t| format!("{}m", t.as_millis().clamp(1, u32::MAX as u128)));
        let mut nva = vec![
            nv(b":method", b"POST"),
            nv(b":scheme", b"http"),
            nv_dyn(b":path", path_b),
            nv(b":authority", b"localhost"),
            nv(b"te", b"trailers"),
            nv(b"content-type", b"application/grpc"),
        ];
        if let Some(v) = timeout_value.as_deref() {
            nva.push(nv_dyn(b"grpc-timeout", v.as_bytes()));
        }
        let provider = nghttp2_data_provider {
            source: nghttp2_data_source {
                ptr: &mut *self.state as *mut CallState as *mut c_void,
            },
            read_callback: Some(data_read),
        };
        let ud = &mut *self.state as *mut CallState as *mut c_void;
        let sid = unsafe {
            nghttp2_submit_request(
                self.session,
                ptr::null(),
                nva.as_ptr(),
                nva.len(),
                &provider,
                ud,
            )
        };
        if sid <= 0 {
            return Err(Status::new(
                StatusCode::Internal,
                "nghttp2 submit_request failed",
            ));
        }
        self.current_sid = sid;
        self.deadline_at = self.timeout.map(|t| std::time::Instant::now() + t);
        self.flush_send().map_err(|e| {
            self.broken = true;
            Status::new(StatusCode::Unavailable, e.to_string())
        })?;
        Ok(sid)
    }

    /// One network round: flush queued output, then block for one read and
    /// feed it to the session. Returns once progress was made.
    /// Set a per-call timeout (`None` clears it; default: wait forever).
    /// Applies to every call made afterwards and covers the whole call —
    /// the unary response, or the entire lifetime of a server-stream
    /// (gRPC deadline semantics, enforced client-side). On expiry the call
    /// fails with [`StatusCode::DeadlineExceeded`] and the stream is
    /// cancelled with RST_STREAM, so the server's cancel hook fires and any
    /// deferred work can stop.
    pub fn set_timeout(&mut self, timeout: Option<std::time::Duration>) {
        self.timeout = timeout;
    }

    /// Fail the in-flight call locally with DEADLINE_EXCEEDED and cancel
    /// the stream so the server stops working on it.
    fn expire_call(&mut self) {
        grpcuds_core::logging::info(c"call deadline exceeded", self.current_sid as i64);
        self.state.grpc_status = Some(StatusCode::DeadlineExceeded as i32);
        self.state.grpc_message = Some("deadline exceeded (client-side)".to_string());
        self.state.closed = true;
        self.deadline_at = None;
        if self.current_sid > 0 {
            // 0x8 = NGHTTP2_CANCEL.
            unsafe { nghttp2_submit_rst_stream(self.session, 0, self.current_sid, 0x8) };
            let _ = self.flush_send();
        }
    }

    fn pump(&mut self, _sid: i32) -> Result<(), Status> {
        self.flush_send().map_err(|e| {
            self.broken = true;
            Status::new(StatusCode::Unavailable, e.to_string())
        })?;
        if let Some(deadline) = self.deadline_at {
            use std::os::fd::AsRawFd;
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                self.expire_call();
                return Ok(());
            }
            let mut pfd = libc::pollfd {
                fd: self.sock.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let timeout_ms = remaining.as_millis().min(i32::MAX as u128) as i32;
            let rc = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
            if rc == 0 {
                self.expire_call();
                return Ok(());
            }
            if rc < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() == ErrorKind::Interrupted {
                    return Ok(());
                }
                self.state.closed = true;
                self.broken = true;
                return Err(Status::new(StatusCode::Unavailable, e.to_string()));
            }
        }
        let mut buf = [0u8; 16 * 1024];
        match self.sock.read(&mut buf) {
            Ok(0) => {
                // EOF: the server closed the connection. The in-flight call
                // ends; the NEXT call reconnects lazily.
                grpcuds_core::logging::debug(c"server closed connection", 0);
                self.state.closed = true;
                self.broken = true;
                Ok(())
            }
            Ok(n) => {
                #[cfg(feature = "wirelog")]
                if let Some(wl) = self.wl.as_mut() {
                    if let Some(bytes) = buf.get(..n) {
                        grpcuds_core::wirelog::log(
                            wl,
                            grpcuds_core::wirelog::Dir::ServerToClient,
                            bytes,
                        );
                    }
                }
                let rc = unsafe { nghttp2_session_mem_recv(self.session, buf.as_ptr(), n) };
                if rc < 0 {
                    // The session state is no longer trustworthy.
                    self.broken = true;
                    return Err(Status::new(StatusCode::Internal, "nghttp2 mem_recv failed"));
                }
                Ok(())
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => Ok(()),
            Err(e) => {
                self.state.closed = true;
                self.broken = true;
                Err(Status::new(StatusCode::Unavailable, e.to_string()))
            }
        }
    }

    /// Drain everything nghttp2 wants to send to the socket.
    fn flush_send(&mut self) -> std::io::Result<()> {
        loop {
            let mut p: *const u8 = ptr::null();
            let n = unsafe { nghttp2_session_mem_send(self.session, &mut p) };
            if n <= 0 || p.is_null() {
                break;
            }
            let bytes = unsafe { std::slice::from_raw_parts(p, n as usize) };
            self.sock.write_all(bytes)?;
            #[cfg(feature = "wirelog")]
            if let Some(wl) = self.wl.as_mut() {
                grpcuds_core::wirelog::log(wl, grpcuds_core::wirelog::Dir::ClientToServer, bytes);
            }
        }
        Ok(())
    }

    /// Turn the received trailers into a `Result`: Ok if grpc-status is 0 (or
    /// absent with HTTP 200), Err(Status) otherwise.
    fn final_status(&self) -> Result<(), Status> {
        match self.state.grpc_status {
            Some(0) => Ok(()),
            Some(code) => Err(Status::from_wire(code, self.state.grpc_message.clone())),
            None => {
                if self.state.http_status == Some(200) {
                    // Server closed cleanly without an explicit grpc-status —
                    // treat as OK (some servers omit a 0 trailer).
                    Ok(())
                } else {
                    Err(Status::new(
                        StatusCode::Unavailable,
                        format!(
                            "stream closed without grpc-status (http :status {:?}, nghttp2 err {})",
                            self.state.http_status, self.state.close_error
                        ),
                    ))
                }
            }
        }
    }

    fn is_idle(&self) -> bool {
        unsafe {
            nghttp2_session_want_read(self.session) == 0
                && nghttp2_session_want_write(self.session) == 0
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        unsafe { nghttp2_session_del(self.session) };
    }
}

// `Client` owns its session exclusively and is driven from one thread; the raw
// pointer does not make it shareable, but it can move between threads.
unsafe impl Send for Client {}

/// A server-streaming response. Each [`message`](Self::message) returns the
/// next message, `None` at a clean end, or `Err(Status)` on a non-OK trailer
/// or transport failure.
pub struct ServerStreaming<'a> {
    client: &'a mut Client,
    stream_id: i32,
}

impl ServerStreaming<'_> {
    /// Next response message, or `None` once the stream ends with `grpc-status:
    /// 0`. A non-OK status (or transport error) is returned as `Err`.
    pub fn message(&mut self) -> Result<Option<Vec<u8>>, Status> {
        loop {
            if let Some(msg) = self.client.state.messages.pop_front() {
                return Ok(Some(msg));
            }
            if self.client.state.closed
                || self.client.is_idle() && self.client.state.grpc_status.is_some()
            {
                self.client.final_status()?;
                return Ok(None);
            }
            self.client.pump(self.stream_id)?;
        }
    }
}

// ---- helpers ----------------------------------------------------------------

fn nv(name: &'static [u8], value: &'static [u8]) -> nghttp2_nv {
    nghttp2_nv {
        name: name.as_ptr() as *mut u8,
        value: value.as_ptr() as *mut u8,
        namelen: name.len(),
        valuelen: value.len(),
        flags: 0,
    }
}

fn nv_dyn(name: &'static [u8], value: &[u8]) -> nghttp2_nv {
    nghttp2_nv {
        name: name.as_ptr() as *mut u8,
        value: value.as_ptr() as *mut u8,
        namelen: name.len(),
        valuelen: value.len(),
        flags: 0,
    }
}

// ---- typed (prost) client ---------------------------------------------------

#[cfg(feature = "prost")]
impl Client {
    /// Typed unary call: encode `req`, decode the response.
    pub fn unary_msg<Req: prost::Message, Resp: prost::Message + Default>(
        &mut self,
        path: &str,
        req: &Req,
    ) -> Result<Resp, Status> {
        let bytes = self.unary(path, &req.encode_to_vec())?;
        Resp::decode(&bytes[..])
            .map_err(|e| Status::new(StatusCode::Internal, format!("decode response: {e}")))
    }

    /// Typed server-streaming call: encode `req`, decode each response.
    pub fn server_streaming_msg<Req: prost::Message, Resp: prost::Message + Default>(
        &mut self,
        path: &str,
        req: &Req,
    ) -> Result<TypedStreaming<'_, Resp>, Status> {
        let stream = self.server_streaming(path, &req.encode_to_vec())?;
        Ok(TypedStreaming {
            inner: stream,
            _resp: std::marker::PhantomData,
        })
    }
}

/// A typed server-streaming response (the `prost` feature). Each
/// [`message`](Self::message) decodes the next `Resp`.
#[cfg(feature = "prost")]
pub struct TypedStreaming<'a, Resp> {
    inner: ServerStreaming<'a>,
    _resp: std::marker::PhantomData<fn() -> Resp>,
}

#[cfg(feature = "prost")]
impl<Resp: prost::Message + Default> TypedStreaming<'_, Resp> {
    /// Next decoded message, or `None` at a clean end.
    pub fn message(&mut self) -> Result<Option<Resp>, Status> {
        match self.inner.message()? {
            Some(bytes) => Resp::decode(&bytes[..])
                .map(Some)
                .map_err(|e| Status::new(StatusCode::Internal, format!("decode response: {e}"))),
            None => Ok(None),
        }
    }
}
