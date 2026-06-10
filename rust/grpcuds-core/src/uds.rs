// SPDX-License-Identifier: MIT OR Apache-2.0
//! AF_UNIX listener + per-connection driver.
//!
//! We are `no_std` and do not own the event loop — the host app watches
//! the fds we expose (epoll / libevent). Each `tick()` on
//! a [`Connection`] drains the socket's read buffer into nghttp2, runs any
//! pending dispatches, then pushes nghttp2's outbound bytes back out to
//! the socket. If the socket is not writable enough to absorb everything,
//! the remainder buffers on the `Connection` for the next tick.
//!
//! Sockets are non-blocking from the moment they exist. `bind` uses
//! `SOCK_NONBLOCK`; `accept` uses `accept4(..., SOCK_NONBLOCK)` so the
//! accepted fd is born non-blocking too.

use alloc::vec::Vec;
use core::ffi::c_void;
use core::mem;
use core::ptr;

use crate::conn::{Conn, ConnError};

// ---- IO error / status types ---------------------------------------------

#[cfg_attr(test, derive(Debug))]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum IoError {
    /// POSIX errno from a syscall.
    Errno(i32),
    /// Path empty or longer than `sun_path` allows.
    InvalidPath,
    /// Upstream gRPC-layer error.
    Conn(ConnError),
}

impl From<ConnError> for IoError {
    fn from(e: ConnError) -> Self {
        IoError::Conn(e)
    }
}

#[cfg_attr(test, derive(Debug))]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TickStatus {
    /// Connection is still active — caller should keep polling.
    Live,
    /// Connection is finished — caller should drop us.
    Closed,
}

// ---- Helpers --------------------------------------------------------------

#[inline]
fn errno() -> i32 {
    unsafe { *libc::__errno_location() }
}

#[inline]
fn would_block(e: i32) -> bool {
    e == libc::EAGAIN || e == libc::EWOULDBLOCK
}

/// Write up to `data.len()` bytes, returning how many made it out. Retries
/// on `EINTR`. Stops on `EAGAIN` / `EWOULDBLOCK` (caller buffers the rest).
fn write_some(fd: i32, data: &[u8]) -> Result<usize, IoError> {
    let mut written = 0;
    while written < data.len() {
        // send + MSG_NOSIGNAL, not write(2): writing to a client that died
        // raises SIGPIPE, which kills a C host daemon outright. EPIPE comes
        // back as an ordinary error instead.
        let n = unsafe {
            libc::send(
                fd,
                data.as_ptr().add(written) as *const c_void,
                data.len() - written,
                libc::MSG_NOSIGNAL,
            )
        };
        if n < 0 {
            let e = errno();
            if would_block(e) {
                break;
            }
            if e == libc::EINTR {
                continue;
            }
            crate::logging::error(c"connection write failed", e as i64);
            return Err(IoError::Errno(e));
        }
        written += n as usize;
    }
    Ok(written)
}

// ---- Listener -------------------------------------------------------------

pub struct Listener {
    fd: i32,
    /// Bound address kept so Drop can unlink the path.
    addr: libc::sockaddr_un,
}

impl Listener {
    /// Bind the listener to a filesystem path. Any existing file at `path`
    /// is unlinked first (best-effort) so repeated server starts don't fail
    /// with `EADDRINUSE`.
    ///
    /// `path` may include or omit a trailing NUL — we copy raw bytes into
    /// `sun_path` and rely on the zeroed remainder for nul-termination.
    pub fn bind(path: &[u8]) -> Result<Self, IoError> {
        let mut addr: libc::sockaddr_un = unsafe { mem::zeroed() };
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
        // Strip a trailing NUL if the caller included one — sun_path is
        // already zeroed so we re-add it via the zero padding.
        let trimmed: &[u8] = match path.last() {
            Some(&0) => &path[..path.len() - 1],
            _ => path,
        };
        if trimmed.is_empty() || trimmed.len() > addr.sun_path.len() - 1 {
            return Err(IoError::InvalidPath);
        }
        for (i, &b) in trimmed.iter().enumerate() {
            addr.sun_path[i] = b as libc::c_char;
        }
        let addr_len = (mem::size_of::<libc::sa_family_t>() + trimmed.len() + 1) as libc::socklen_t;

        unsafe {
            // Best-effort unlink of any leftover socket file. Ignore errors.
            libc::unlink(addr.sun_path.as_ptr());

            let fd = libc::socket(
                libc::AF_UNIX,
                libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                0,
            );
            if fd < 0 {
                let e = errno();
                crate::logging::error(c"listener socket failed", e as i64);
                return Err(IoError::Errno(e));
            }
            if libc::bind(fd, &addr as *const _ as *const libc::sockaddr, addr_len) < 0 {
                let e = errno();
                libc::close(fd);
                crate::logging::error(c"listener bind failed", e as i64);
                return Err(IoError::Errno(e));
            }
            if libc::listen(fd, 128) < 0 {
                let e = errno();
                libc::close(fd);
                crate::logging::error(c"listener listen failed", e as i64);
                return Err(IoError::Errno(e));
            }
            crate::logging::info(c"server listening", fd as i64);
            Ok(Self { fd, addr })
        }
    }

    /// fd for the listening socket. Surface to the caller's event loop.
    #[inline]
    pub fn fd(&self) -> i32 {
        self.fd
    }

    /// Try to accept a pending connection. Returns `Ok(None)` if the listen
    /// queue is empty (`EAGAIN`).
    pub fn accept(&self) -> Result<Option<Connection>, IoError> {
        let fd = unsafe {
            libc::accept4(
                self.fd,
                ptr::null_mut(),
                ptr::null_mut(),
                libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            )
        };
        if fd < 0 {
            let e = errno();
            if would_block(e) {
                return Ok(None);
            }
            crate::logging::error(c"accept failed", e as i64);
            return Err(IoError::Errno(e));
        }
        crate::logging::debug(c"connection accepted", fd as i64);
        let mut conn = Conn::new_server()?;
        // Enables the NO_COPY direct-send path for large DATA frames.
        conn.set_fd(fd);
        Ok(Some(Connection { fd, conn }))
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.fd);
            // Unlink the bound socket file. Best-effort.
            libc::unlink(self.addr.sun_path.as_ptr());
        }
    }
}

// ---- Connection -----------------------------------------------------------

pub struct Connection {
    fd: i32,
    conn: Conn,
}

impl Connection {
    /// fd for this connection socket. Surface to the caller's event loop.
    #[inline]
    pub fn fd(&self) -> i32 {
        self.fd
    }

    /// Mutable access to the underlying `Conn` for handler registration,
    /// `write_call`, `finish_call`, etc.
    #[inline]
    pub fn conn(&mut self) -> &mut Conn {
        &mut self.conn
    }

    /// True iff nghttp2 currently wants to read more bytes from the peer.
    /// Use this to decide whether to keep `POLLIN` armed in the event loop.
    #[inline]
    pub fn wants_read(&self) -> bool {
        self.conn.wants_read()
    }

    /// True iff there's outbound work queued — either nghttp2 has frames to
    /// send or we have leftover bytes from a previous partial write. Use
    /// this to decide whether to arm `POLLOUT` in the event loop.
    #[inline]
    pub fn wants_write(&self) -> bool {
        self.conn.has_pending() || self.conn.wants_write()
    }

    /// Read phase: drain the socket into nghttp2, fire any newly-Complete
    /// handlers, then opportunistically push outbound bytes back. Call when
    /// `revents & (POLLIN | POLLHUP | POLLERR)` is set.
    ///
    /// Always does the write phase at the end too, so most callers that
    /// only need a single entry point can keep using this even when the
    /// socket only signalled writability — the read syscall will return
    /// EAGAIN cheaply.
    pub fn tick_read(&mut self) -> Result<TickStatus, IoError> {
        self.conn.expire_deadlines();
        if let Some(status) = self.drain_reads()? {
            return Ok(status);
        }
        self.conn.dispatch();
        self.flush_writes()
    }

    /// Write phase only: flush leftover bytes then drain nghttp2's send
    /// queue to the socket. Call when `revents & POLLOUT` is set but POLLIN
    /// is not — saves the read syscall + dispatch pass entirely.
    pub fn tick_write(&mut self) -> Result<TickStatus, IoError> {
        self.conn.expire_deadlines();
        self.flush_writes()
    }

    /// Alias for [`tick_read`](Self::tick_read). Retained so callers without revents info
    /// (in-process drivers, tests) can keep a single entry point.
    #[inline]
    pub fn tick(&mut self) -> Result<TickStatus, IoError> {
        self.tick_read()
    }

    /// Remaining ms until this connection's earliest `grpc-timeout`
    /// deadline (`Some(0)` = due), `None` if no in-flight call has one.
    /// Bound your poll timeout with it so idle connections still expire.
    #[inline]
    pub fn next_deadline_ms(&self) -> Option<u64> {
        self.conn.next_deadline_ms()
    }

    /// Returns `Some(Closed)` on peer EOF; `None` once the socket would
    /// block. EINTR is retried; other errno values propagate.
    fn drain_reads(&mut self) -> Result<Option<TickStatus>, IoError> {
        let mut buf = [0u8; 4096];
        loop {
            let n = unsafe { libc::read(self.fd, buf.as_mut_ptr() as *mut c_void, buf.len()) };
            if n < 0 {
                let e = errno();
                if would_block(e) {
                    return Ok(None);
                }
                if e == libc::EINTR {
                    continue;
                }
                crate::logging::error(c"connection read failed", e as i64);
                return Err(IoError::Errno(e));
            }
            if n == 0 {
                crate::logging::debug(c"peer closed connection", self.fd as i64);
                return Ok(Some(TickStatus::Closed));
            }
            let consumed = self.conn.recv(&buf[..n as usize])?;
            debug_assert_eq!(consumed, n as usize);
        }
    }

    /// Flush the conn's pending tail, then pull from nghttp2 until the
    /// socket would block. Chunk bytes go from nghttp2's buffer straight to
    /// the socket (no intermediate copy); a partial write stashes the
    /// remainder as the conn's pending tail for the next tick. Large DATA
    /// frames bypass this entirely: nghttp2 hands them to the `NO_COPY`
    /// send_data callback during `pull_send`, which writes them fd-direct
    /// (and stashes its own tail on a partial write).
    fn flush_writes(&mut self) -> Result<TickStatus, IoError> {
        let fd = self.fd;
        loop {
            // Ordered tail first — from a previous partial chunk write OR
            // stashed by the NO_COPY send path during the last pull_send.
            while self.conn.has_pending() {
                let written = write_some(fd, self.conn.pending_bytes())?;
                if written == 0 {
                    return Ok(TickStatus::Live); // socket full
                }
                self.conn.consume_pending(written);
            }

            // Wire-log copy: the chunk borrows self.conn, so the written
            // slice is copied out and logged after the borrow ends.
            #[cfg(feature = "wirelog")]
            let mut wl_buf: Vec<u8> = Vec::new();
            let (done, stash_from) = {
                let chunk = self.conn.pull_send()?;
                if chunk.is_empty() {
                    (true, None)
                } else {
                    let written = write_some(fd, chunk)?;
                    #[cfg(feature = "wirelog")]
                    if let Some(sent) = chunk.get(..written) {
                        let _ = wl_buf.try_reserve(written);
                        wl_buf.extend_from_slice(sent);
                    }
                    if written < chunk.len() {
                        // Copy only the unsent remainder (rare path).
                        let mut rest: Vec<u8> = Vec::new();
                        if rest.try_reserve_exact(chunk.len() - written).is_err() {
                            return Err(IoError::Conn(ConnError::OutOfMemory));
                        }
                        rest.extend_from_slice(&chunk[written..]);
                        (false, Some(rest))
                    } else {
                        (false, None)
                    }
                }
            };
            #[cfg(feature = "wirelog")]
            if !wl_buf.is_empty() {
                self.conn.wl_log_out(&wl_buf);
            }
            if let Some(rest) = stash_from {
                self.conn.stash_pending(&rest).map_err(IoError::Conn)?;
                return Ok(TickStatus::Live);
            }
            if done {
                break;
            }
        }

        if self.conn.wants_read() || self.conn.wants_write() || self.conn.has_pending() {
            Ok(TickStatus::Live)
        } else {
            Ok(TickStatus::Closed)
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.fd);
        }
    }
}

// ---- Tests ----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conn::HandlerFn;
    use crate::framing::{decode_header, encode_header, DEFAULT_MAX_MESSAGE_LEN, FRAME_HEADER_LEN};
    use crate::headers::GrpcStatus;
    use alloc::boxed::Box;
    use alloc::format;
    use alloc::vec::Vec;
    use core::sync::atomic::{AtomicU64, Ordering};
    use std::io::{ErrorKind, Read, Write};
    use std::os::unix::net::UnixStream;
    use std::thread;
    use std::time::Duration;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn unique_path() -> Vec<u8> {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        format!("/tmp/grpcuds-test-{}-{}.sock", pid, n).into_bytes()
    }

    fn path_str(path: &[u8]) -> &str {
        core::str::from_utf8(path).expect("ascii path")
    }

    #[test]
    fn bind_listen_unlink_lifecycle() {
        let path = unique_path();
        {
            let _listener = Listener::bind(&path).unwrap();
            // Path should exist now.
            assert!(std::path::Path::new(path_str(&path)).exists());
            // Re-binding to the same path before drop fails (in use).
            // (Different listener, same path.)
        }
        // After drop, the path should be unlinked.
        assert!(!std::path::Path::new(path_str(&path)).exists());
    }

    #[test]
    fn bind_unlinks_leftover_file() {
        let path = unique_path();
        // Pre-create a stale file at the path.
        std::fs::write(path_str(&path), b"stale").unwrap();
        assert!(std::path::Path::new(path_str(&path)).exists());
        let _listener = Listener::bind(&path).unwrap();
        // bind unlinks then re-creates the socket file — just verify bind
        // succeeded.
    }

    #[test]
    fn invalid_paths_rejected() {
        match Listener::bind(b"") {
            Err(IoError::InvalidPath) => {}
            other => panic!("expected InvalidPath, got {other:?}", other = other.is_ok()),
        }
        let too_long = vec![b'x'; 200];
        match Listener::bind(&too_long) {
            Err(IoError::InvalidPath) => {}
            other => panic!("expected InvalidPath, got {other:?}", other = other.is_ok()),
        }
    }

    #[test]
    fn accept_returns_none_when_no_client() {
        let path = unique_path();
        let listener = Listener::bind(&path).unwrap();
        // No connect — accept must not block.
        assert!(matches!(listener.accept(), Ok(None)));
    }

    #[test]
    fn accept_returns_connection_after_connect() {
        let path = unique_path();
        let listener = Listener::bind(&path).unwrap();
        let _client = UnixStream::connect(path_str(&path)).unwrap();
        // Give the kernel a moment to populate the listen queue.
        let mut attempts = 0;
        let conn = loop {
            match listener.accept().unwrap() {
                Some(c) => break c,
                None => {
                    attempts += 1;
                    assert!(attempts < 100, "accept never returned a connection");
                    thread::sleep(Duration::from_millis(1));
                }
            }
        };
        assert!(conn.fd() > 0);
    }

    // ---- Unary echo round-trip across a real socket ----------------------

    use grpcuds_sys::{
        nghttp2_data_flag_NGHTTP2_DATA_FLAG_EOF as DATA_EOF, nghttp2_data_provider,
        nghttp2_data_source, nghttp2_frame, nghttp2_nv, nghttp2_session, nghttp2_session_callbacks,
        nghttp2_session_callbacks_del, nghttp2_session_callbacks_new,
        nghttp2_session_callbacks_set_on_data_chunk_recv_callback as set_on_data,
        nghttp2_session_callbacks_set_on_frame_recv_callback as set_on_frame,
        nghttp2_session_callbacks_set_on_header_callback as set_on_header,
        nghttp2_session_client_new, nghttp2_session_del, nghttp2_session_mem_recv,
        nghttp2_session_mem_send, nghttp2_submit_request, nghttp2_submit_settings,
    };

    struct ClientState {
        status: Option<Vec<u8>>,
        content_type: Option<Vec<u8>>,
        grpc_status: Option<Vec<u8>>,
        data: Vec<u8>,
        end_stream_seen: bool,
    }
    impl ClientState {
        fn new() -> Self {
            Self {
                status: None,
                content_type: None,
                grpc_status: None,
                data: Vec::new(),
                end_stream_seen: false,
            }
        }
    }

    unsafe extern "C" fn cli_on_header(
        _s: *mut nghttp2_session,
        _f: *const nghttp2_frame,
        name: *const u8,
        namelen: usize,
        value: *const u8,
        valuelen: usize,
        _flags: u8,
        ud: *mut c_void,
    ) -> i32 {
        let st = &mut *(ud as *mut ClientState);
        let n = core::slice::from_raw_parts(name, namelen);
        let v = core::slice::from_raw_parts(value, valuelen).to_vec();
        if n == b":status" {
            st.status = Some(v);
        } else if n == b"content-type" {
            st.content_type = Some(v);
        } else if n == b"grpc-status" {
            st.grpc_status = Some(v);
        }
        0
    }
    unsafe extern "C" fn cli_on_data(
        _s: *mut nghttp2_session,
        _flags: u8,
        _sid: i32,
        data: *const u8,
        len: usize,
        ud: *mut c_void,
    ) -> i32 {
        let st = &mut *(ud as *mut ClientState);
        st.data
            .extend_from_slice(core::slice::from_raw_parts(data, len));
        0
    }
    unsafe extern "C" fn cli_on_frame(
        _s: *mut nghttp2_session,
        f: *const nghttp2_frame,
        ud: *mut c_void,
    ) -> i32 {
        let st = &mut *(ud as *mut ClientState);
        if (*f).hd.flags & 0x1 != 0 {
            st.end_stream_seen = true;
        }
        0
    }

    struct ClientReq {
        bytes: Vec<u8>,
        offset: usize,
    }
    unsafe extern "C" fn cli_data_read(
        _s: *mut nghttp2_session,
        _sid: i32,
        buf: *mut u8,
        length: usize,
        data_flags: *mut u32,
        source: *mut nghttp2_data_source,
        _ud: *mut c_void,
    ) -> isize {
        let src = &mut *((*source).ptr as *mut ClientReq);
        let remaining = src.bytes.len() - src.offset;
        let n = remaining.min(length);
        if n > 0 {
            ptr::copy_nonoverlapping(src.bytes.as_ptr().add(src.offset), buf, n);
            src.offset += n;
        }
        if src.offset == src.bytes.len() {
            *data_flags |= DATA_EOF;
        }
        n as isize
    }

    fn nv(name: &'static [u8], value: &'static [u8]) -> nghttp2_nv {
        nghttp2_nv {
            name: name.as_ptr() as *mut u8,
            value: value.as_ptr() as *mut u8,
            namelen: name.len(),
            valuelen: value.len(),
            flags: 0,
        }
    }

    unsafe extern "C" fn echo_handler(
        conn: *mut Conn,
        call_id: i32,
        req: *const u8,
        req_len: usize,
        _ud: *mut c_void,
    ) -> i32 {
        let c = &mut *conn;
        let req_slice = core::slice::from_raw_parts(req, req_len);
        let _ = c.write_call(call_id, req_slice);
        let _ = c.finish_call(call_id, GrpcStatus::Ok);
        0
    }

    // ---- Direct exercise of the tick_write / wants_write split ----------
    //
    // The cpp-server's poll loop relies on tick_write being able to drain
    // outbound bytes without ever calling tick_read on the same iteration
    // (POLLOUT-only branch), and on wants_write() switching from true to
    // false once the queue is empty. These tests pin both behaviours.

    #[test]
    fn tick_write_alone_drains_initial_settings() {
        let path = unique_path();
        let listener = Listener::bind(&path).unwrap();
        let mut client = UnixStream::connect(path_str(&path)).unwrap();
        client.set_nonblocking(true).unwrap();

        let mut conn = {
            let mut attempts = 0;
            loop {
                match listener.accept().unwrap() {
                    Some(c) => break c,
                    None => {
                        attempts += 1;
                        assert!(attempts < 100);
                        thread::sleep(Duration::from_millis(1));
                    }
                }
            }
        };

        // Right after accept the server has a SETTINGS frame queued in
        // nghttp2. wants_write must reflect that — the cpp-server's
        // pollfd builder reads this to decide whether to arm POLLOUT.
        assert!(
            conn.wants_write(),
            "fresh accepted conn must report wants_write=true (initial SETTINGS pending)"
        );

        // tick_write alone (no tick_read) must drain the queue to the
        // socket. This is exactly the POLLOUT-only branch of the
        // cpp-server's poll loop.
        let status = conn.tick_write().expect("tick_write must succeed");
        assert_eq!(status, TickStatus::Live, "still live, peer not yet closed");

        // Client side: the SETTINGS frame should have arrived. We don't
        // parse it — just confirm the bytes made it through to prove
        // tick_write actually wrote to the socket.
        let mut got_bytes = false;
        for _ in 0..50 {
            let mut buf = [0u8; 64];
            match client.read(&mut buf) {
                Ok(n) if n > 0 => {
                    got_bytes = true;
                    break;
                }
                Ok(_) => break,
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(1));
                }
                Err(e) => panic!("client read err: {e}"),
            }
        }
        assert!(
            got_bytes,
            "tick_write should have flushed SETTINGS to the socket"
        );
    }

    #[test]
    fn wants_write_tracks_out_pending() {
        let path = unique_path();
        let listener = Listener::bind(&path).unwrap();
        let _client = UnixStream::connect(path_str(&path)).unwrap();

        let mut conn = {
            let mut attempts = 0;
            loop {
                match listener.accept().unwrap() {
                    Some(c) => break c,
                    None => {
                        attempts += 1;
                        assert!(attempts < 100);
                        thread::sleep(Duration::from_millis(1));
                    }
                }
            }
        };

        // Pre-flush: queue is non-empty (initial SETTINGS).
        assert!(conn.wants_write());

        // Flush once via tick_write — it should clear the queue (the
        // socket buffer is empty so SETTINGS fits in one shot).
        conn.tick_write().unwrap();

        // After draining, nghttp2 has nothing to send AND out_pending is
        // empty AND no client has spoken — wants_write must report false.
        assert!(
            !conn.wants_write(),
            "after drain, no outbound work pending — POLLOUT should NOT be armed"
        );
    }

    #[test]
    fn unary_echo_over_uds() {
        let path = unique_path();
        let listener = Listener::bind(&path).unwrap();

        let mut stream = UnixStream::connect(path_str(&path)).unwrap();
        stream.set_nonblocking(true).unwrap();

        // Accept (with brief retry since accept races connect).
        let mut connection = {
            let mut attempts = 0;
            loop {
                match listener.accept().unwrap() {
                    Some(c) => break c,
                    None => {
                        attempts += 1;
                        assert!(attempts < 100);
                        thread::sleep(Duration::from_millis(1));
                    }
                }
            }
        };
        let echo: HandlerFn = echo_handler;
        connection
            .conn()
            .register_method(b"/svc/Echo", echo, ptr::null_mut())
            .unwrap();

        // Build framed request body: 5B prefix + "hello".
        let mut body = Vec::new();
        let mut hdr = [0u8; FRAME_HEADER_LEN];
        encode_header(false, 5, &mut hdr);
        body.extend_from_slice(&hdr);
        body.extend_from_slice(b"hello");
        let mut req_src = Box::new(ClientReq {
            bytes: body,
            offset: 0,
        });
        let mut cli_state = Box::new(ClientState::new());

        unsafe {
            let mut cbs: *mut nghttp2_session_callbacks = ptr::null_mut();
            assert_eq!(nghttp2_session_callbacks_new(&mut cbs), 0);
            set_on_header(cbs, Some(cli_on_header));
            set_on_data(cbs, Some(cli_on_data));
            set_on_frame(cbs, Some(cli_on_frame));
            let mut client: *mut nghttp2_session = ptr::null_mut();
            assert_eq!(
                nghttp2_session_client_new(
                    &mut client,
                    cbs,
                    cli_state.as_mut() as *mut _ as *mut c_void
                ),
                0
            );
            nghttp2_session_callbacks_del(cbs);
            assert_eq!(nghttp2_submit_settings(client, 0, ptr::null(), 0), 0);

            let nva = [
                nv(b":method", b"POST"),
                nv(b":scheme", b"http"),
                nv(b":path", b"/svc/Echo"),
                nv(b":authority", b"localhost"),
                nv(b"te", b"trailers"),
                nv(b"content-type", b"application/grpc"),
            ];
            let provider = nghttp2_data_provider {
                source: nghttp2_data_source {
                    ptr: req_src.as_mut() as *mut _ as *mut c_void,
                },
                read_callback: Some(cli_data_read),
            };
            let sid = nghttp2_submit_request(
                client,
                ptr::null(),
                nva.as_ptr(),
                nva.len(),
                &provider,
                ptr::null_mut(),
            );
            assert!(sid > 0);

            // Drive the round trip: client mem_send → UnixStream::write
            //                       server tick (reads + handles + writes)
            //                       UnixStream::read → client mem_recv
            for _ in 0..128 {
                let mut did_work = false;

                // Client → socket
                loop {
                    let mut p: *const u8 = ptr::null();
                    let n = nghttp2_session_mem_send(client, &mut p);
                    assert!(n >= 0);
                    if n == 0 || p.is_null() {
                        break;
                    }
                    let slice = core::slice::from_raw_parts(p, n as usize);
                    stream.write_all(slice).unwrap();
                    did_work = true;
                }

                // Server side: drain readable + dispatch + write back.
                match connection.tick().unwrap() {
                    TickStatus::Live => {}
                    TickStatus::Closed => {}
                }

                // Socket → client
                let mut buf = [0u8; 4096];
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            let rc = nghttp2_session_mem_recv(client, buf.as_ptr(), n);
                            assert!(rc >= 0, "client mem_recv: {rc}");
                            did_work = true;
                        }
                        Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                        Err(e) => panic!("read err: {e}"),
                    }
                }

                if !did_work && cli_state.end_stream_seen {
                    break;
                }
                // Tiny sleep to give the kernel time to surface bytes.
                if !did_work {
                    thread::sleep(Duration::from_millis(1));
                }
            }

            nghttp2_session_del(client);
        }

        // Client validation.
        assert_eq!(cli_state.status.as_deref(), Some(&b"200"[..]));
        assert_eq!(
            cli_state.content_type.as_deref(),
            Some(&b"application/grpc"[..])
        );
        assert_eq!(cli_state.grpc_status.as_deref(), Some(&b"0"[..]));
        assert!(cli_state.end_stream_seen);
        assert!(cli_state.data.len() >= FRAME_HEADER_LEN);
        let parsed = decode_header(&cli_state.data, DEFAULT_MAX_MESSAGE_LEN).unwrap();
        let pl = parsed.payload_len as usize;
        assert_eq!(
            &cli_state.data[FRAME_HEADER_LEN..FRAME_HEADER_LEN + pl],
            b"hello"
        );
    }
}
