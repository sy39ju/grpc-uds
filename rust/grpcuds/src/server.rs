// SPDX-License-Identifier: MIT OR Apache-2.0
//! The server side: [`Server`], [`ServerBuilder`], [`Running`], and the
//! [`ServerWriter`] streaming handle.
//!
//! ## Threading model — and why [`ServerWriter`] is `Send + Sync`
//!
//! [`Server::serve`] runs a **single I/O thread**: it owns the listener and
//! every connection, and the underlying nghttp2 session is **not** thread-safe,
//! so it may only be touched there. To let a server stream from *other* threads
//! anyway (a worker, a timer, a device callback), [`ServerWriter`] holds only
//! `Arc`-shared, thread-safe state: `write`/`finish` copy the operation into an
//! **outbound mailbox** and poke a wakeup `eventfd`; the I/O thread drains the
//! mailbox on its next poll cycle and applies the writes on the correct
//! connection. So `Send`/`Sync` are sound — the nghttp2 session is never
//! touched off the I/O thread.
//!
//! ## Cancellation
//!
//! When the client goes away (RST_STREAM / connection drop), the I/O thread
//! reports it through the writer's shared state: the next `write`/`finish`
//! returns `Err(Closed)` and [`ServerWriter::is_cancelled`] turns `true`, so a
//! producer loop stops within one wasted message.
//!
//! ## Backpressure
//!
//! The per-stream outbound queue is unbounded by default;
//! [`ServerWriter::set_backpressure`] bounds it ([`OverflowPolicy::DropOldest`]
//! keeps the newest N, [`OverflowPolicy::Reject`] refuses the excess and sets
//! the sticky [`ServerWriter::overflowed`] flag).

use std::cell::{Cell, RefCell, UnsafeCell};
use std::collections::VecDeque;
use std::ffi::c_void;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use grpcuds_core::{Conn, ConnError, Connection, IoError, Listener, TickStatus};

/// Outbound queue bound for one stream; apply it with
/// [`ServerWriter::set_backpressure`].
pub use grpcuds_core::{Backpressure, OverflowPolicy};

use crate::{Closed, Error, Status, StatusCode};

// ---- Outbound mailbox ------------------------------------------------------

enum OpKind {
    Write(Vec<u8>),
    Finish(StatusCode, Option<Vec<u8>>),
    Policy(Backpressure),
}

struct OutOp {
    token: u64,
    call_id: i32,
    kind: OpKind,
    /// The originating writer's shared state, so the I/O thread can report an
    /// op that bounced (dead stream → `dead`, queue full → `overflow`).
    shared: Arc<CallShared>,
}

/// State shared between every clone of one call's [`ServerWriter`] and the I/O
/// thread. The writer side only ever reads `dead`/`overflow`; the I/O thread
/// sets them when an op cannot be applied.
#[derive(Default)]
struct CallShared {
    /// `finish` was called on some clone (first call wins).
    finished: AtomicBool,
    /// The stream is gone without us finishing it: client RST_STREAM,
    /// connection drop, or serve() found no live stream for an op.
    dead: AtomicBool,
    /// A write was refused by a `Bounded`/`Reject` queue (sticky).
    overflow: AtomicBool,
}

/// The cross-thread channel from any [`ServerWriter`] to the I/O thread. Holds
/// queued ops plus a wakeup `eventfd` so a producer on another thread can make
/// the poll loop return promptly. Owned by the server (via `Arc`); cloned into
/// every writer.
struct Mailbox {
    ops: Mutex<VecDeque<OutOp>>,
    wakeup: libc::c_int,
}

impl Mailbox {
    fn new() -> io::Result<Self> {
        let fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            ops: Mutex::new(VecDeque::new()),
            wakeup: fd,
        })
    }

    /// Queue an op. Returns whether the queue was empty before — the caller
    /// uses it to elide redundant wakeups (one pending wakeup is enough: the
    /// push that made the queue non-empty already poked, and `take()` empties
    /// the queue as a whole).
    fn push(&self, op: OutOp) -> bool {
        match self.ops.lock() {
            Ok(mut q) => {
                let was_empty = q.is_empty();
                q.push_back(op);
                was_empty
            }
            Err(_) => true,
        }
    }

    /// Wake the poll loop: write a 1 to the eventfd counter.
    fn poke(&self) {
        let one: u64 = 1;
        unsafe {
            libc::write(self.wakeup, &one as *const u64 as *const c_void, 8);
        }
    }

    /// Clear the eventfd counter (called on the I/O thread after poll).
    fn drain_wakeup(&self) {
        let mut buf = [0u8; 8];
        loop {
            let n = unsafe { libc::read(self.wakeup, buf.as_mut_ptr() as *mut c_void, 8) };
            if n != 8 {
                break;
            }
        }
    }

    fn take(&self) -> VecDeque<OutOp> {
        match self.ops.lock() {
            Ok(mut q) => std::mem::take(&mut *q),
            Err(_) => VecDeque::new(),
        }
    }
}

impl Drop for Mailbox {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.wakeup);
        }
    }
}

/// Server side of a call: a cheap, `Clone`-able, `Send + Sync` handle for
/// pushing response messages and closing the stream — from the I/O thread or
/// any other thread. Operations are routed through the server's outbound
/// mailbox and applied on the I/O thread, so the nghttp2 session is never
/// touched off-thread.
#[derive(Clone)]
pub struct ServerWriter {
    token: u64,
    call_id: i32,
    mailbox: Arc<Mailbox>,
    shared: Arc<CallShared>,
    /// Expiry of the call's `grpc-timeout` budget, snapshotted at dispatch
    /// (a deadline never changes after the request headers).
    deadline_at: Option<std::time::Instant>,
}

impl ServerWriter {
    /// Remaining `grpc-timeout` budget of this call — stock gRPC's
    /// "context deadline". `None` when the client sent no deadline;
    /// `Some(ZERO)` once it is due. Use it to skip work that cannot
    /// finish in time.
    pub fn time_remaining(&self) -> Option<std::time::Duration> {
        self.deadline_at
            .map(|d| d.saturating_duration_since(std::time::Instant::now()))
    }

    fn closed(&self) -> bool {
        self.shared.finished.load(Ordering::Acquire) || self.shared.dead.load(Ordering::Acquire)
    }

    fn push(&self, kind: OpKind) {
        let was_empty = self.mailbox.push(OutOp {
            token: self.token,
            call_id: self.call_id,
            kind,
            shared: self.shared.clone(),
        });
        // The eventfd poke exists to wake a *sleeping* poll loop. Two cases
        // need no syscall at all:
        //   - we are already on the I/O thread (an inline handler write):
        //     serve() drains the mailbox later in this very iteration;
        //   - the queue was non-empty: the push that made it non-empty poked,
        //     and take() drains the whole queue at once.
        if !was_empty {
            return;
        }
        let on_io_thread = CURRENT_MAILBOX.with(|m| {
            m.borrow()
                .as_ref()
                .is_some_and(|cur| Arc::ptr_eq(cur, &self.mailbox))
        });
        if !on_io_thread {
            self.mailbox.poke();
        }
    }

    /// Queue one response message (raw, unframed bytes — the core adds the
    /// 5-byte gRPC frame). `Err(Closed)` means the stream is already finished
    /// **or the client is gone** (see [`is_cancelled`](Self::is_cancelled)) —
    /// a producer loop should stop on it.
    ///
    /// Copies `msg`; if you already own the bytes,
    /// [`write_owned`](Self::write_owned) moves them instead (one copy fewer
    /// per message).
    pub fn write(&self, msg: &[u8]) -> Result<(), Closed> {
        if self.closed() {
            return Err(Closed);
        }
        self.push(OpKind::Write(msg.to_vec()));
        Ok(())
    }

    /// Like [`write`](Self::write), but takes the message by value — the
    /// bytes travel to the wire without another copy (they are moved through
    /// the mailbox into the core's outbound queue).
    pub fn write_owned(&self, msg: Vec<u8>) -> Result<(), Closed> {
        if self.closed() {
            return Err(Closed);
        }
        self.push(OpKind::Write(msg));
        Ok(())
    }

    /// Close the server side of the stream with `status`. Idempotent across
    /// all clones: the first call wins; later `write`/`finish` calls return
    /// `Err(Closed)`.
    pub fn finish(&self, status: Status) -> Result<(), Closed> {
        if self.shared.dead.load(Ordering::Acquire)
            || self.shared.finished.swap(true, Ordering::AcqRel)
        {
            return Err(Closed);
        }
        let msg = match status.message {
            Some(m) if !m.is_empty() => Some(m.into_bytes()),
            _ => None,
        };
        self.push(OpKind::Finish(status.code, msg));
        Ok(())
    }

    /// Bound this stream's outbound queue (see [`Backpressure`]). Applied on
    /// the I/O thread before any later `write`; call it at the top of the
    /// handler, before producing.
    pub fn set_backpressure(&self, bp: Backpressure) -> Result<(), Closed> {
        if self.closed() {
            return Err(Closed);
        }
        self.push(OpKind::Policy(bp));
        Ok(())
    }

    /// True once `finish` has been called (on this handle or any clone).
    pub fn is_finished(&self) -> bool {
        self.shared.finished.load(Ordering::Acquire)
    }

    /// True once the I/O thread has observed that the client is gone —
    /// RST_STREAM, connection drop, or the stream vanished before an op could
    /// apply. Lazy: it flips after a queued op bounces, so a producer that
    /// never writes never sees it (check `write`'s return instead).
    pub fn is_cancelled(&self) -> bool {
        self.shared.dead.load(Ordering::Acquire)
    }

    /// True once a `Bounded { policy: Reject, .. }` queue refused a write.
    /// Sticky; the refused `write` call itself had already returned `Ok`
    /// (the rejection happens later, on the I/O thread).
    pub fn overflowed(&self) -> bool {
        self.shared.overflow.load(Ordering::Acquire)
    }
}

// ---- Method registry -------------------------------------------------------

/// Exactly one response on `Ok`, none on `Err` — the framework writes and
/// finishes; the handler cannot violate the unary wire contract.
type UnaryHandler = Box<dyn FnMut(&[u8]) -> Result<Vec<u8>, Status> + Send + 'static>;
/// Returns immediately; some producer (inline or off-thread) calls `finish`.
/// A non-OK return finishes early (error abort).
type StreamingHandler = Box<dyn FnMut(&[u8], &ServerWriter) -> Status + Send + 'static>;

enum Handler {
    Unary(UnaryHandler),
    ServerStreaming(StreamingHandler),
}

/// Heap-stable per-method record. Its address is handed to the core as the
/// handler `user_data`, so it must outlive every connection — hence `Box`.
///
/// The handler is behind an `UnsafeCell` because the core's `register_method`
/// takes a shared `user_data`, yet `FnMut` needs `&mut`. The single I/O thread
/// dispatches handlers strictly sequentially (never re-entrant, never
/// concurrent), so handing out one `&mut` per call is sound.
struct MethodEntry {
    path: Vec<u8>,
    handler: UnsafeCell<Handler>,
}

// SAFETY: a `MethodEntry` is only dispatched on the single I/O thread; the
// `UnsafeCell` is never accessed concurrently. The bounds let the owning
// `Server` move to a background thread (the `Handler` is itself `Send`).
unsafe impl Send for MethodEntry {}
unsafe impl Sync for MethodEntry {}

// The I/O thread's per-tick context, reached by the C trampoline: which
// connection is currently dispatching, and the mailbox to route writes into.
thread_local! {
    static CURRENT_TOKEN: Cell<u64> = const { Cell::new(0) };
    static CURRENT_MAILBOX: RefCell<Option<Arc<Mailbox>>> = const { RefCell::new(None) };
}

unsafe extern "C" fn trampoline(
    _conn: *mut Conn,
    call_id: i32,
    req: *const u8,
    req_len: usize,
    user_data: *mut c_void,
) -> i32 {
    let entry = &*(user_data as *const MethodEntry);
    let handler = &mut *entry.handler.get();
    let req_slice = if req_len == 0 {
        &[][..]
    } else {
        std::slice::from_raw_parts(req, req_len)
    };

    let token = CURRENT_TOKEN.with(|c| c.get());
    let mailbox = CURRENT_MAILBOX
        .with(|m| m.borrow().clone())
        .expect("trampoline ran outside serve()");
    let deadline_at = (*_conn)
        .call_time_remaining_ms(call_id)
        .ok()
        .flatten()
        .map(|ms| std::time::Instant::now() + std::time::Duration::from_millis(ms));
    let writer = ServerWriter {
        token,
        call_id,
        mailbox,
        shared: Arc::new(CallShared::default()),
        deadline_at,
    };

    match handler {
        // Unary: the framework ships exactly one message (Ok) or none (Err)
        // and closes the call — the handler can't violate the unary contract.
        Handler::Unary(h) => match h(req_slice) {
            Ok(resp) => {
                let _ = writer.write_owned(resp);
                let _ = writer.finish(Status::ok());
            }
            Err(status) => {
                let _ = writer.finish(status);
            }
        },
        // Streaming: an OK return leaves the stream open for the producer to
        // finish; a non-OK return aborts the call now.
        Handler::ServerStreaming(h) => {
            let status = h(req_slice, &writer);
            if !status.is_ok() && !writer.is_finished() {
                let _ = writer.finish(status);
            }
        }
    }
    0
}

/// Builder for a [`Server`]: pick a UDS path, register methods, then `build`.
#[derive(Default)]
pub struct ServerBuilder {
    path: Option<PathBuf>,
    // Boxed: each entry's address is handed to the core as handler user_data.
    #[allow(clippy::vec_box)]
    methods: Vec<Box<MethodEntry>>,
}

impl ServerBuilder {
    /// An empty builder; equivalent to [`Server::builder`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind to a filesystem UDS path. Mirrors gRPC's `unix:` addresses; pass
    /// either `"/tmp/x.sock"` or `"unix:/tmp/x.sock"` (the prefix is stripped).
    pub fn bind(mut self, path: impl AsRef<Path>) -> Self {
        let p = path.as_ref();
        let stripped = p
            .to_str()
            .and_then(|s| s.strip_prefix("unix:"))
            .map(PathBuf::from)
            .unwrap_or_else(|| p.to_path_buf());
        self.path = Some(stripped);
        self
    }

    /// Register a **unary** handler for `"/package.Service/Method"`: request
    /// bytes in, `Ok(response bytes)` or `Err(status)` out. The framework
    /// writes the single response and finishes the call — exactly one message
    /// on success, none on error, per the gRPC unary contract.
    pub fn add_unary<F>(self, path: impl AsRef<str>, handler: F) -> Self
    where
        F: FnMut(&[u8]) -> Result<Vec<u8>, Status> + Send + 'static,
    {
        self.add(path, Handler::Unary(Box::new(handler)))
    }

    /// Register a **server-streaming** handler. The handler returns
    /// immediately; the stream stays open until some producer (the handler
    /// inline, or a clone of the [`ServerWriter`] on another thread) calls
    /// [`ServerWriter::finish`]. Returning a non-OK [`Status`] aborts the call.
    pub fn add_server_streaming<F>(self, path: impl AsRef<str>, handler: F) -> Self
    where
        F: FnMut(&[u8], &ServerWriter) -> Status + Send + 'static,
    {
        self.add(path, Handler::ServerStreaming(Box::new(handler)))
    }

    fn add(mut self, path: impl AsRef<str>, handler: Handler) -> Self {
        self.methods.push(Box::new(MethodEntry {
            path: path.as_ref().as_bytes().to_vec(),
            handler: UnsafeCell::new(handler),
        }));
        self
    }

    /// Bind the listener and produce a runnable [`Server`].
    pub fn build(self) -> Result<Server, Error> {
        let path = self.path.ok_or(Error::MissingBindPath)?;
        let listener =
            Listener::bind(path.as_os_str().as_encoded_bytes()).map_err(|e| match e {
                IoError::InvalidPath => Error::InvalidPath(path.clone()),
                IoError::Errno(no) => Error::Bind {
                    path: path.clone(),
                    source: io::Error::from_raw_os_error(no),
                },
                IoError::Conn(_) => Error::Bind {
                    path: path.clone(),
                    source: io::Error::other("connection setup failed"),
                },
            })?;
        Ok(Server {
            listener,
            methods: self.methods,
        })
    }
}

struct ConnSlot {
    token: u64,
    conn: Connection,
    dead: bool,
}

/// A bound server. Owns the listener and the registered methods; drive it with
/// [`serve`](Server::serve).
pub struct Server {
    listener: Listener,
    // Boxed: each entry's address is handed to the core as handler user_data.
    #[allow(clippy::vec_box)]
    methods: Vec<Box<MethodEntry>>,
}

impl Server {
    /// Start configuring a server: `Server::builder().bind(..).add_unary(..)`.
    pub fn builder() -> ServerBuilder {
        ServerBuilder::default()
    }

    /// Run the I/O loop on a dedicated background thread and return a
    /// [`Running`] handle. Dropping (or [`join`](Running::join)ing) the handle
    /// stops the server — ownership is the lifecycle.
    pub fn run(self) -> Result<Running, Error> {
        let shutdown = Arc::new(AtomicBool::new(false));
        let sd = shutdown.clone();
        let thread = thread::Builder::new()
            .name("grpcuds-io".into())
            .spawn(move || self.serve(&sd))
            .map_err(Error::Io)?;
        Ok(Running {
            shutdown,
            thread: Some(thread),
        })
    }

    /// Run the single-threaded poll loop **on the calling thread** until
    /// `shutdown` is set to `true` (checked every poll cycle, ≤100 ms).
    /// Accepts connections, registers every method on each, ticks them, and
    /// flushes the outbound mailbox (so off-thread `ServerWriter` calls land).
    /// This is the low-level entry point; most servers want
    /// [`run`](Server::run).
    pub fn serve(self, shutdown: &AtomicBool) -> Result<(), Error> {
        let mailbox = Arc::new(Mailbox::new()?);
        CURRENT_MAILBOX.with(|m| *m.borrow_mut() = Some(mailbox.clone()));

        let mut conns: Vec<ConnSlot> = Vec::new();
        let mut next_token: u64 = 1;
        let mut ret: Result<(), Error> = Ok(());
        // Reused across iterations — rebuilding is cheap, reallocating every
        // poll cycle is pointless.
        let mut pfds: Vec<libc::pollfd> = Vec::new();

        'main: loop {
            if shutdown.load(Ordering::Relaxed) {
                break;
            }

            // pollfds: [listener, wakeup, conn_0..]. The wakeup eventfd makes
            // the loop return as soon as an off-thread writer pokes it.
            pfds.clear();
            pfds.push(libc::pollfd {
                fd: self.listener.fd(),
                events: libc::POLLIN,
                revents: 0,
            });
            pfds.push(libc::pollfd {
                fd: mailbox.wakeup,
                events: libc::POLLIN,
                revents: 0,
            });
            for s in &conns {
                let mut events = libc::POLLIN;
                if s.conn.wants_write() {
                    events |= libc::POLLOUT;
                }
                pfds.push(libc::pollfd {
                    fd: s.conn.fd(),
                    events,
                    revents: 0,
                });
            }

            // 100ms timeout bounds shutdown latency even if nothing pokes us.
            let rc = unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, 100) };
            if rc < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                ret = Err(Error::Io(e));
                break 'main;
            }

            let n_polled = pfds.len() - 2; // conns present when pfds was built

            // New connections (appended after the existing ones).
            if pfds[0].revents & libc::POLLIN != 0 {
                loop {
                    match self.listener.accept() {
                        Ok(Some(mut connection)) => {
                            let mut ok = true;
                            for entry in &self.methods {
                                let ud = entry.as_ref() as *const MethodEntry as *mut c_void;
                                if connection
                                    .conn()
                                    .register_method(&entry.path, trampoline, ud)
                                    .is_err()
                                {
                                    ok = false;
                                    break;
                                }
                            }
                            if ok {
                                let token = next_token;
                                next_token += 1;
                                conns.push(ConnSlot {
                                    token,
                                    conn: connection,
                                    dead: false,
                                });
                            }
                        }
                        Ok(None) => break,
                        Err(_) => break,
                    }
                }
            }

            // Clear the wakeup counter; the mailbox is drained below regardless.
            if pfds[1].revents & libc::POLLIN != 0 {
                mailbox.drain_wakeup();
            }

            // Read/dispatch the connections that were polled this round.
            for i in 0..n_polled {
                let re = pfds[i + 2].revents;
                let deadline_due = re == 0 && conns[i].conn.next_deadline_ms() == Some(0);
                if re == 0 && !deadline_due {
                    continue;
                }
                // Tell the trampoline which connection is dispatching, so a
                // ServerWriter it builds routes back to the right call.
                CURRENT_TOKEN.with(|c| c.set(conns[i].token));
                // A due grpc-timeout deadline ticks like a read: tick_read
                // runs the expiry sweep and flushes the trailers. (The 100ms
                // poll cap bounds expiry latency on an idle connection.)
                let readable =
                    deadline_due || re & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0;
                let res = if readable {
                    conns[i].conn.tick_read()
                } else if re & libc::POLLOUT != 0 {
                    conns[i].conn.tick_write()
                } else {
                    Ok(TickStatus::Live)
                };
                if !matches!(res, Ok(TickStatus::Live)) {
                    conns[i].dead = true;
                }
            }

            // Apply queued writes/finishes (from inline handlers AND other
            // threads) on their connections — on this thread only. An op that
            // cannot be applied is reported back through the writer's shared
            // state so producers stop instead of spinning forever.
            for op in mailbox.take() {
                let Some(slot) = conns.iter_mut().find(|s| s.token == op.token && !s.dead) else {
                    op.shared.dead.store(true, Ordering::Release);
                    continue;
                };
                let c = slot.conn.conn();
                let res = match op.kind {
                    // The bytes were owned by the op — move them straight
                    // into the core's outbound queue, no copy.
                    OpKind::Write(bytes) => c.write_call_owned(op.call_id, bytes),
                    OpKind::Finish(code, Some(msg)) => c.finish_call_msg(op.call_id, code, &msg),
                    OpKind::Finish(code, None) => c.finish_call(op.call_id, code),
                    OpKind::Policy(bp) => c.set_stream_policy(op.call_id, bp),
                };
                match res {
                    Ok(()) => {}
                    // Bounded/Reject queue at capacity: the message was
                    // dropped, the stream itself is fine.
                    Err(ConnError::QueueFull) => {
                        op.shared.overflow.store(true, Ordering::Release);
                    }
                    // Stream gone (client RST / already closed) or the
                    // connection is unusable — either way this call is over.
                    Err(_) => {
                        op.shared.dead.store(true, Ordering::Release);
                    }
                }
            }

            // Flush any connection with outbound bytes now pending.
            for slot in conns.iter_mut() {
                if slot.dead {
                    continue;
                }
                if slot.conn.wants_write()
                    && !matches!(slot.conn.tick_write(), Ok(TickStatus::Live))
                {
                    slot.dead = true;
                }
            }

            conns.retain(|s| !s.dead);
        }

        CURRENT_MAILBOX.with(|m| *m.borrow_mut() = None);
        ret
    }
}

#[cfg(feature = "tokio")]
impl Server {
    /// Run the I/O loop on tokio's blocking pool until `shutdown` resolves
    /// (then stop gracefully) or the loop fails — a future-driven graceful
    /// shutdown:
    ///
    /// ```ignore
    /// let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    /// tokio::spawn(server.serve_async(async { let _ = stop_rx.await; }));
    /// // ... later: let _ = stop_tx.send(());
    /// ```
    ///
    /// Handlers stay synchronous and must not block, but producers may be
    /// ordinary tokio tasks: [`ServerWriter`] is `Send + Sync` and its
    /// operations are non-blocking, so a cloned writer works from `async`
    /// code as-is.
    pub async fn serve_async(
        self,
        shutdown: impl std::future::Future<Output = ()>,
    ) -> Result<(), Error> {
        let flag = Arc::new(AtomicBool::new(false));
        let io_flag = flag.clone();
        let mut io = tokio::task::spawn_blocking(move || self.serve(&io_flag));
        let joined = tokio::select! {
            res = &mut io => Some(res),
            _ = shutdown => {
                flag.store(true, Ordering::Relaxed);
                None
            }
        };
        let res = match joined {
            Some(r) => r,
            None => io.await, // loop notices the flag within one poll cycle
        };
        res.map_err(|_| Error::Io(io::Error::other("grpcuds I/O task panicked")))?
    }
}

/// A server running on its own background I/O thread, returned by
/// [`Server::run`]. **Ownership is the lifecycle**: dropping the handle (or
/// calling [`join`](Running::join)) signals shutdown and joins the thread.
pub struct Running {
    shutdown: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<Result<(), Error>>>,
}

impl Running {
    /// Signal the I/O thread to stop after its current poll cycle (≤100 ms).
    /// Non-blocking; use [`join`](Running::join) (or drop) to wait.
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    /// True while the I/O thread is still running.
    pub fn is_running(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| !t.is_finished())
    }

    /// Stop the server and wait for the I/O thread, returning its result.
    /// Must not be called from a handler (that would self-join the thread).
    pub fn join(mut self) -> Result<(), Error> {
        self.shutdown();
        match self.thread.take().map(|t| t.join()) {
            Some(Ok(r)) => r,
            Some(Err(_)) => Err(Error::Io(io::Error::other("grpcuds I/O thread panicked"))),
            None => Ok(()),
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.shutdown();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

// ---- Typed (prost) layer ----------------------------------------------------

/// Typed handlers over [prost](https://docs.rs/prost) messages (the `prost`
/// feature). Decode/encode move into the framework; handlers see structs:
///
/// ```ignore
/// builder.add_unary_msg("/echo.Echo/Hello", |req: HelloRequest| {
///     Ok(HelloReply { text: req.text.to_uppercase() })
/// });
/// builder.add_server_streaming_msg("/scan.Scan/Start", |_req: StartScan, w| {
///     let w = w.clone();                       // MessageWriter<ScanResult>
///     std::thread::spawn(move || {
///         while let Some(r) = next_result() {
///             if !w.send(&r) { return; }       // typed write; false = client gone
///         }
///         w.finish(Status::ok());
///     });
///     Status::ok()
/// });
/// ```
///
/// A request that fails to decode is answered with `INTERNAL` + a
/// `grpc-message` describing the decode error (the standard gRPC
/// convention), without invoking the handler.
#[cfg(feature = "prost")]
mod typed {
    use super::{Backpressure, Closed, ServerBuilder, ServerWriter, Status, StatusCode};
    use std::marker::PhantomData;

    /// Typed face of a [`ServerWriter`]: [`send`](Self::send) encodes one
    /// `Resp` per call. Same threading contract as the raw writer — `Clone` +
    /// `Send + Sync`, ops cross to the I/O thread via the mailbox.
    pub struct MessageWriter<Resp> {
        pub(super) inner: ServerWriter,
        pub(super) _resp: PhantomData<fn(&Resp)>,
    }

    impl<Resp> Clone for MessageWriter<Resp> {
        fn clone(&self) -> Self {
            Self {
                inner: self.inner.clone(),
                _resp: PhantomData,
            }
        }
    }

    impl<Resp: prost::Message> MessageWriter<Resp> {
        /// Encode and queue one response message. `Err(Closed)` once the
        /// stream is finished or the client is gone (see
        /// [`ServerWriter::write`]).
        pub fn send(&self, msg: &Resp) -> Result<(), Closed> {
            self.inner.write_owned(msg.encode_to_vec())
        }
    }

    impl<Resp> MessageWriter<Resp> {
        /// See [`ServerWriter::finish`].
        pub fn finish(&self, status: Status) -> Result<(), Closed> {
            self.inner.finish(status)
        }

        /// See [`ServerWriter::set_backpressure`].
        pub fn set_backpressure(&self, bp: Backpressure) -> Result<(), Closed> {
            self.inner.set_backpressure(bp)
        }

        /// See [`ServerWriter::is_finished`].
        pub fn is_finished(&self) -> bool {
            self.inner.is_finished()
        }

        /// See [`ServerWriter::is_cancelled`].
        pub fn is_cancelled(&self) -> bool {
            self.inner.is_cancelled()
        }

        /// See [`ServerWriter::overflowed`].
        pub fn overflowed(&self) -> bool {
            self.inner.overflowed()
        }

        /// See [`ServerWriter::time_remaining`].
        pub fn time_remaining(&self) -> Option<std::time::Duration> {
            self.inner.time_remaining()
        }

        /// The underlying byte-level writer.
        pub fn raw(&self) -> &ServerWriter {
            &self.inner
        }
    }

    fn decode_status(e: prost::DecodeError) -> Status {
        Status::new(
            StatusCode::Internal,
            format!("failed to decode request: {e}"),
        )
    }

    impl ServerBuilder {
        /// Typed **unary** handler: `Req` in, `Ok(Resp)` or `Err(status)` out.
        /// The framework decodes the request, encodes the response, and
        /// finishes the call.
        pub fn add_unary_msg<Req, Resp, F>(self, path: impl AsRef<str>, mut handler: F) -> Self
        where
            Req: prost::Message + Default,
            Resp: prost::Message,
            F: FnMut(Req) -> Result<Resp, Status> + Send + 'static,
        {
            self.add_unary(path, move |bytes: &[u8]| {
                let req = Req::decode(bytes).map_err(decode_status)?;
                handler(req).map(|resp| resp.encode_to_vec())
            })
        }

        /// Typed **server-streaming** handler: same contract as
        /// [`add_server_streaming`](ServerBuilder::add_server_streaming), with
        /// a [`MessageWriter<Resp>`] in place of the byte writer.
        pub fn add_server_streaming_msg<Req, Resp, F>(
            self,
            path: impl AsRef<str>,
            mut handler: F,
        ) -> Self
        where
            Req: prost::Message + Default,
            Resp: prost::Message,
            F: FnMut(Req, &MessageWriter<Resp>) -> Status + Send + 'static,
        {
            self.add_server_streaming(path, move |bytes: &[u8], w: &ServerWriter| {
                let req = match Req::decode(bytes) {
                    Ok(r) => r,
                    Err(e) => return decode_status(e),
                };
                let tw = MessageWriter {
                    inner: w.clone(),
                    _resp: PhantomData,
                };
                handler(req, &tw)
            })
        }
    }
}

#[cfg(feature = "prost")]
pub use typed::MessageWriter;

#[cfg(test)]
mod builder_tests {
    use super::*;

    #[test]
    fn bind_strips_the_unix_prefix() {
        let b = Server::builder().bind("unix:/tmp/svc.sock");
        assert_eq!(b.path.as_deref(), Some(Path::new("/tmp/svc.sock")));

        let b = Server::builder().bind("/tmp/plain.sock");
        assert_eq!(b.path.as_deref(), Some(Path::new("/tmp/plain.sock")));

        // Only the scheme prefix is special — a later "unix:" is untouched.
        let b = Server::builder().bind("/tmp/unix:odd.sock");
        assert_eq!(b.path.as_deref(), Some(Path::new("/tmp/unix:odd.sock")));
    }

    #[test]
    fn build_without_bind_is_missing_bind_path() {
        assert!(matches!(
            Server::builder().build(),
            Err(crate::Error::MissingBindPath)
        ));
    }

    #[test]
    fn build_rejects_empty_and_oversized_paths() {
        // "unix:" alone strips to an empty path.
        assert!(matches!(
            Server::builder().bind("unix:").build(),
            Err(crate::Error::InvalidPath(_))
        ));
        // sun_path caps UDS paths at ~107 bytes.
        let long = format!("/tmp/{}.sock", "x".repeat(200));
        assert!(matches!(
            Server::builder().bind(&long).build(),
            Err(crate::Error::InvalidPath(_))
        ));
    }
}
