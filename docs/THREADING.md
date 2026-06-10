<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
# Thread-safety design (feature/thread-safety)

Status: Phase 1 outbound mailbox **implemented in the Rust C ABI**
(`grpcuds-ffi-impl`): `grpcuds_call_write` / `_finish[_msg]` are always
thread-safe, with `grpcuds_mailbox_wakeup_fd` / `_register_io_thread` /
`_drain` wiring it into the poll loop. Plain C and C++ share the one
implementation; the C++ wrapper is a thin shim over those symbols. Verified by
the mailbox race tests (`rust/grpcuds-ffi-impl/src/mailbox.rs`), the
producer-thread C and C++ BLE examples, root ctest, and valgrind.

> The design discussion (why the mailbox lives in the FFI not the core, why a
> single always-safe API with no `_mt` naming, why two Rust mailboxes stay
> separate, why `pthread_mutex` over a spinlock, and the equivalence checklist)
> is in the [Revision section](#revision-current-direction-the-mailbox-moves-into-the-c-abi-rust)
> below. The original C++-wrapper Phase 1 (`outbound_mailbox.h`) it replaced is
> kept as narrative above it.

## Problem

grpcuds-core / grpcuds-ffi are single-threaded and lock-free by design
(`grpcuds.h`: "The runtime is single-threaded"). Every `grpcuds_*` call —
`Accept`, `tick_*`, `call_write`, `call_finish` — touches the nghttp2
session + per-stream state with no synchronization.

Real deployments are not single-threaded. A representative server topology:

- **host event loop**: drives the application's own work (e.g. an embedded
  main loop calling Bluetooth/BLE APIs).
- **dedicated grpc thread**: runs the grpcuds poll loop + handlers.
- Domain results (e.g. BLE scan/GATT) are produced **on the event-loop
  thread**, but the server-streaming response that ships them to the client
  goes through `writer->Write()` → `grpcuds_call_write` → the nghttp2
  session, which is owned by the **grpc thread**. Calling `Write()` from the
  event-loop thread is a data race on the session + the outbound queue.

So we need `Write()` / `Finish()` (and later `Read()`) to be callable from
any thread, while the nghttp2 session stays touched by exactly one thread.

## Layering decision

> **Superseded for the mailbox** by the [Revision section](#revision-current-direction-the-mailbox-moves-into-the-c-abi-rust):
> the outbound mailbox moves into the Rust C ABI (`grpcuds-ffi-impl`), not the
> C++ wrapper, so plain C gets it too. The *core* still stays single-threaded
> and lock-free; what moves is the cross-thread boundary, one layer out in the
> FFI shim. The rest of this section's reasoning still holds.

**All thread-safety lives in the C++ wrapper (`grpcudspp/`). The Rust core
and FFI stay single-threaded and lock-free — zero changes in Phase 1.**

Rationale:
- `grpcuds-core` / `grpcuds-ffi-impl` are `#![no_std]` + `panic="abort"`;
  `std::sync` is unavailable and pulling pthread through FFI into the core
  would bloat the size budget for no benefit.
- The C++ wrapper is header/source compiled **into the server binary**,
  which already links `libstdc++` + `pthread`. Threading machinery there
  costs the server's `.text`, not the Rust `.a` — exactly the budget split
  the project wants (library lightweight; server code may be heavier).
- It keeps the one hard invariant trivially true: the nghttp2 session is
  only ever touched on the I/O thread.

### Roles

- **I/O thread** = the caller's grpc poll loop. The *only* thread that
  calls into the FFI/core (`Accept`, `tick_*`, and the drained
  `call_write`/`call_finish`). One, consistent thread.
- **Producer / worker threads** = anywhere a handler or async producer
  runs (an event-loop callback, a worker pool). They never touch the core;
  they talk to a thread-safe mailbox.

### Mechanism: outbound mailbox + wakeup fd

1. The wrapper's `Server` owns a wakeup fd (`eventfd`, or a self-pipe) and
   a per-call **outbound mailbox** (`std::mutex` + a queue of
   `{bytes}` / `{finish, status}` items).
2. `RawWriter::Write` / `Finish`, when invoked, push to the mailbox and
   write one byte to the wakeup fd — they do **not** call
   `grpcuds_call_write` directly anymore.
3. The caller adds the wakeup fd to its `poll(2)` set. When it's readable,
   the caller drains it (`Server::DrainOutbound()`), which — **on the I/O
   thread** — pops every queued item and calls the real
   `grpcuds_call_write` / `grpcuds_call_finish`.

The nghttp2 session is thus only ever touched on the I/O thread; cross-
thread handoff is just the mutex-guarded queue + a 1-byte wakeup. This is
the inverse of a control-message queue a server might use to hand work from
the grpc thread back to the event loop; here it's producer→grpc.

### Connection teardown: the tombstone registry

A producer thread may still be enqueueing for a connection the I/O thread
is about to free — without help, the drained item would replay
`grpcuds_call_write` against a dangling pointer. The mailbox therefore
keeps a tombstone registry:

- `Mailbox().UnregisterCall(handle)` — call right before
  `grpcuds_conn_free` (handle = `grpcuds_conn_call_handle(conn)`). Items
  already queued for that connection are scrubbed; later enqueues for the
  same handle are dropped at drain time, never dereferenced.
- `Mailbox().RegisterCall(handle)` — call on accept. Clears a tombstone
  left by a previous connection whose address the allocator re-used.

The bundled loops (`ServerThread`, the `tests/cpp` reference loop) do both
automatically. **Thread contract:** `UnregisterCall` + `grpcuds_conn_free`
must run on the I/O thread — Drain's liveness check and the core replay
that follows are not one atomic step, so teardown racing in from another
thread could land between them. Producer threads only ever enqueue; that
side is fully thread-safe. Custom loops that skip the registry keep the
historical contract: stop every producer before freeing the connection.

## Revision (current direction): the mailbox moves into the C ABI (Rust)

The Phase 1 mailbox above ships in the **C++ wrapper**, which means
**plain-C servers have no thread-safe write path at all** — only C++ gets
off-thread `Write()`/`Finish()`. To close that gap without copying the
mailbox into both C and C++, the mailbox moves **into the Rust C ABI
(`grpcuds-ffi-impl`)** as the single implementation: plain C calls it
directly, and the C++ wrapper's `outbound_mailbox.h` becomes a thin shim
that delegates to the same symbols (its `std::mutex` / `std::deque` /
`eventfd` machinery is deleted). Net effect: three mailbox shapes → two
(see "Why two Rust mailboxes" below); C and C++ share one Rust copy.

The existing `grpcuds_call_write` / `grpcuds_call_finish` / `_finish_msg`
become **always thread-safe** (option X — a single API, exactly like the C++
wrapper's `Write()`): each checks internally whether it is on the registered
I/O thread and either calls the core directly or enqueues + pokes the wakeup
fd. **No `_mt` variant, no naming convention** (see "Single always-safe API"
below). The only *new* symbols are the mailbox lifecycle:

    grpcuds_mailbox_wakeup_fd(server) -> int      // add to the poll set
    grpcuds_mailbox_drain(server)                 // I/O thread, on wakeup
    grpcuds_mailbox_register_io_thread(server)    // marks the direct-path thread

Tombstones reuse the existing `grpcuds_conn_call_handle` hook. The mechanism
(mailbox + wakeup fd + tombstone registry, described above) is unchanged —
only its home moves.

### What thread-safe *server* write buys — and why client write is out of scope

**What it buys (server).** Making `Write()` / `Finish()` callable from any
thread lets the response-producing code run on whatever thread the app's domain
logic already lives on, without (a) blocking the single I/O thread or (b) the
app hand-rolling its own cross-thread queue. The motivating pattern (server-
streaming, and deferred unary — the handler may return without finishing and
complete later, per `grpcuds.h`):

    I/O thread: Read -> handler called
    handler   : save (call, call_id), delegate to a worker / the app's main
                thread, return immediately
    worker    : do the work; when done, call grpcuds_call_write / _finish
    mailbox   : carries that call to the I/O thread, which does the real write

The handler returns right away; the result is delivered by *calling write/finish
from the worker thread* (not a function return), and that call is exactly what
thread-safety makes sound. Without it, the same pattern forces either blocking
the I/O thread until the result is ready (stalls every other connection) or
rebuilding the mailbox by hand. So the app keeps its own threading — a BLE radio
callback, a worker pool, the main loop — and just calls `Write`.

**Why only the server, not the client.** The grpcuds client is a *blocking*
client — one call in flight per connection; `grpcuds_client_*` blocks until the
response. There the calling thread *is* the producer and waits for the reply, so
there is no producer≠I/O-thread handoff inside a call to make safe. Client
concurrency is therefore served by **multiple connections** (cheap, ~15 KB each;
shared-nothing), not by sharing one connection across threads — blocking makes
that pointless on a single connection (the same nghttp2 serialization point as
the server). The async-client architecture that *would* need a client-side
mailbox — an I/O thread demultiplexing replies back to the requesting threads
(gRPC's CompletionQueue / tonic) — is a separate, much larger runtime grpcuds
deliberately does not build. So the mailbox is **server-outbound only**, and
that asymmetry is by design, not an omission.

### Single always-safe API (no `_mt` naming) — and why

We first considered the C convention of encoding the contract in the *name* —
`_r` (reentrant) for the safe variant (`strtok_r`, `gmtime_r`, `rand_r`),
`_unlocked` for the fast caller-must-serialize variant (`getc_unlocked`) — i.e.
shipping a fast `grpcuds_call_write` alongside a safe `grpcuds_call_write_mt`.
**We rejected it** (option Y), in favor of a single always-safe call (option X).

Why: the C++ wrapper already exposes a **single** `Write()` that is *always*
thread-safe — it checks `OnIoThread()` internally and branches direct-vs-mailbox;
there is no `_mt` because there is only one variant, the choice is
*encapsulated*. gRPC C++ likewise expresses thread-safety through documentation
and type design, not name markers. Introducing a `_mt`/`_unlocked` naming
convention on the C side that **C++ would not share** is itself an
inconsistency — the very "weird shape" the naming was meant to avoid. So the C
ABI matches C++: `grpcuds_call_write` / `_finish` are themselves always
thread-safe, branching internally. One symbol, one contract, both languages.

Consequences of "single always-safe":

- The contract is expressed by **documentation** (like gRPC C++ / our C++
  wrapper), not by name — there is no variant to distinguish.
- The mailbox is therefore **always linked** into the `.a` (an always-safe
  `write` always references it), so it is **not** an opt-out feature — see
  [Size-impact](#size-impact--revised) below.
- The fast path costs one extra atomic-bool load (is an I/O thread registered?)
  in the common single-threaded case — negligible, and exactly what the C++
  wrapper already pays.

The trade we accepted: a few KB of always-present mailbox code in every server
`.a`, in exchange for one coherent API shape across C and C++ with no naming
convention to keep consistent. (Option Y — feature-gated `_mt` — would have
shaved those KB off single-threaded builds but split the C and C++ surfaces.)

### Why not just make the core thread-safe?

The recurring alternative is "wrap the core in `Arc<Mutex<…>>` so any thread
can use it." Rejected, for four independent reasons:

- **No parallelism to gain.** An nghttp2 session is itself serial — two RPCs
  on one connection cannot proceed concurrently. A per-connection lock adds
  contention and buys *zero* parallelism; the session serializes regardless.
- **`no_std` has no `std::sync::Mutex`.** The real, futex-backed Mutex lives
  in `std` (the ~278 KB floor the `.a` refuses). The core would be stuck with
  a spinlock or pthread — and a spinlock is *wrong* on the single-core armv7
  target (a preempted lock-holder makes spinners waste their whole timeslice).
- **The compile-time guarantee doesn't cross the FFI.** Rust's `Send`/`Sync`
  checking protects *Rust* callers. Our consumers hold `void*` across the C
  ABI, where there is no borrow checker — `Arc<Mutex<Conn>>` cannot stop a C
  thread. The mailbox enforces the discipline by *API shape* (the off-thread
  symbol physically enqueues instead of touching the core), which survives the
  boundary.
- **Size + invariant.** Locking the hot path adds code to a tens-of-KB library
  and breaks the single-threaded-core invariant for no real benefit.

So the core stays single-threaded with no locks; the **mailbox is the
thread-safe *interface* layered on top of it.** A writer handle is
`Send + Sync` precisely because it points at the *mailbox* (whose `Sync`
comes from the lock), never at the core — the writer's thread-safety is not
the core's.

### Why two Rust mailboxes, not one

The safe-Rust `grpcuds` crate already has its own mailbox
(`server.rs::Mailbox` = `std::sync::Mutex<VecDeque<…>>` + `eventfd`), behind
its `Send + Sync` `ServerWriter`. It is deliberately **not** merged with the
new `no_std` one:

- What's shareable is trivial — `Mutex<VecDeque<T>>` + `eventfd` + drain
  plumbing (~30 lines). What differs is essential: the `grpcuds` mailbox
  queues *typed* ops (`OutOp { token, call_id, kind, shared: Arc<CallShared> }`)
  and drains via the safe `Conn`; the C-ABI one queues *raw*
  `{ call: void*, call_id, bytes, finish, status }` and drains via
  `grpcuds_call_write` / `_finish_msg`.
- Unifying would force the shared mailbox to be `no_std`, **downgrading the
  `grpcuds` crate's `std::sync::Mutex` to a spinlock/pthread** — for which its
  std users (who already pay the std floor, so `std::Mutex` is free) gain
  nothing. A C-only need would make the Rust path worse.

So the `grpcuds` (std) mailbox keeps `std::sync::Mutex`; `grpcuds-ffi-impl`
gets a `no_std` sibling. Different consumers, different constraints.

### Lock choice for the no_std mailbox: `pthread_mutex`, not a spinlock

The `no_std` mailbox can't use `std::sync::Mutex`, and a **spinlock is
rejected** for the single-core reason above: on uniprocessor armv7 a spinner
burns its timeslice while the preempted holder can't run to release the lock.
The mailbox uses **`pthread_mutex` via `libc`** instead:

- futex-backed — sleeps on contention and yields to the holder (no busy-wait),
  safe on a single core;
- `libc` is already a real dependency of the core, and
  `pthread_mutex_lock`/`unlock` live in the C library the server already links
  → **~0 static bytes**;
- wrapped in `unsafe` with a hand-written `unsafe impl Sync` — we own the lock,
  so we vouch for the soundness `std::sync::Mutex` would otherwise grant.

The critical section is an O(1), **syscall-free** `push` / `swap`; the
`eventfd` poke and the core replay happen *outside* the lock.

### Equivalence checklist: no behavioral change when the mailbox moves

The C++ `Write()` / `Finish()` / `WakeupFd()` / `DrainOutbound()` /
`RegisterIoThread()` public API and behavior must stay unchanged — only the
implementation relocates to Rust, and C++ delegates to it. The relocation is
semantics-preserving **iff** the Rust port replicates these four, re-verified by
the existing C++ ctest + `tests/bench/memcheck.sh`:

1. **Thread-identity check stays cheap.** The on-I/O-thread test runs on every
   write (the streaming hot path). Use `pthread_self()` + `pthread_equal()`
   (userspace), **not** `gettid()` (a syscall). Short-circuit on an
   `io_thread_registered` atomic-bool first, so the common single-threaded case
   is one atomic load with no id compare — matching the C++ cost.
2. **"Unregistered ⇒ every thread is the I/O thread" default.** Before any
   `register_io_thread`, the direct path is taken, so zero-setup single-threaded
   servers keep working — same as the C++ `OnIoThread()` returning `true`
   pre-registration.
3. **Mailbox stays process-global** (one Server per process — already the C++
   assumption), so the always-safe `grpcuds_call_write(call, …)` reaches it
   without adding a server back-pointer to the `call` handle.
4. **Tombstone registry + FIFO drain replicated exactly** — freed-connection
   scrubbing keyed by `grpcuds_conn_call_handle`, items replayed in order. This
   is the data-race surface `memcheck.sh` must re-confirm.

### Size-impact — revised

Moving the mailbox into the `.a` changes the Phase 1 row of the table below:
the outbound mailbox now lands in the Rust `.a`, so it **is** budget-relevant.
Under option X (a single always-safe `grpcuds_call_write`) the mailbox is
**always linked** — not an opt-out feature — because the always-safe write
always references it. The cost is a few KB (pthread_mutex wrapper + queue +
eventfd + tombstone set) present in every server `.a`, the accepted price of the
symmetric, name-free API. Measure it against the 40–60 KB target / 100 KB
ceiling like the Phase 2 Rust rows.

## Phasing

### Phase 1 — thread-safe outbound for the existing model (unary + server-streaming)

- Makes `Write()` / `Finish()` safe to call from any thread via the
  outbound mailbox + wakeup fd above.
- Directly fixes the event-loop→`Write` data race for server-streaming
  producers (e.g. BLE).
- Handlers still run **synchronously on the I/O thread** during `tick`
  (unary fills the response and returns; server-streaming returns
  immediately and the producer writes later). **No worker pool needed.**
- **Rust side: zero changes.** Pure C++ wrapper addition + one new fd that
  the caller's poll loop watches. Zero Rust size impact.
- Backpressure (`SetBackpressure`) currently configures the **core
  `OutQueue`**, which the on-I/O-thread direct path honors. The
  off-thread path (`RawWriter::Write` → `OutboundMailbox::EnqueueWrite`)
  does **not** yet apply a bound: the mailbox `queue_` is unbounded and
  `Write()` returns `true` regardless. A producer thread that outpaces the
  drain therefore grows the mailbox without limit — the caller must rate-
  limit its producers (the same "cooperating local peer" assumption the
  rest of the design rests on). Applying the bound at mailbox enqueue (so
  `Write()` returns the true/false contract off-thread too) is the planned
  refinement; until then this is a documented limitation, surfaced in the
  README's Security §resource-limits note.

### Phase 2 — client-streaming (`ServerReader<T>`) — gated on CC-1 size

This is the heavy one and the only part that touches Rust.

- Requires a worker thread pool (wrapper-owned, configurable count) so a
  blocking `reader->Read()` suspends a worker thread, not the I/O thread.
- Requires **core changes**: an inbound gRPC length-prefix parser that
  delivers each request message as it completes (today the core buffers
  the whole request and dispatches once at END_STREAM), dispatch moved to
  HEADERS for client-streaming methods, and new FFI hooks
  (`on_message` / `on_half_close`) plus a per-call **inbound mailbox**
  (`std::mutex` + condvar) that `Read()` blocks on.
- Plugin: stop decoding the request upfront for client-streaming methods;
  generate a `ServerReader<T>*` handler parameter; remove the
  `client_streaming` rejection in `protoc-gen-grpcudspp`.
- Hard part is **not** LOC — it's the cancel race: a worker blocked in
  `Read()` while the I/O thread tears the stream down on RST_STREAM. Needs
  refcounted call lifetime + waking the blocked reader. `set_cancel_hook`
  (fires on the I/O thread during `tick`) is the starting point.

## Size-impact split (what CC-1 must confirm)

| Cost | Lands in | Budget-relevant? |
| --- | --- | --- |
| Phase 1 mailbox + wakeup + worker glue | C++ wrapper → server `.text` | No (server-side) |
| Phase 2 inbound parser + new FFI hooks | Rust core/ffi → `.a` | **Yes** — measure |
| Phase 2 worker pool + condvar mailbox | C++ wrapper → server `.text` | No (server-side) |

> Revised: the [current direction](#revision-current-direction-the-mailbox-moves-into-the-c-abi-rust)
> moves the Phase 1 outbound mailbox into the Rust `.a`, so that row becomes
> **budget-relevant**. Under option X (single always-safe `grpcuds_call_write`)
> the mailbox is **always linked** — not an opt-out feature — and measured like
> the Phase 2 Rust rows.

Expectation: Phase 1 adds ~0 to the Rust budget. Phase 2's Rust delta is
the inbound parser + a few FFI entry points — single-digit KB — but must
be measured against the 40–60KB target / 100KB ceiling before committing.

## Open questions + recommended resolutions

1. **Worker pool ownership (Phase 2)** — wrapper-managed pool (gRPC sync-
   server style), configurable thread count, small default. Friendlier for
   usability; consistent with the memory that the single-thread constraint
   is the I/O loop only, handlers may run on worker threads.
2. **Unary that needs a BT round-trip** — the current plugin trampoline
   auto-finishes synchronously, so a unary handler can't await an
   event-loop result without blocking the I/O thread. Resolution: with Phase 2 worker
   threads, a unary handler may block on the worker thread; the response is
   written + finished via the outbound mailbox. Until Phase 2, model such
   calls as server-streaming or accept I/O-thread blocking (stalls all
   conns — only acceptable with no concurrency).
3. **Cancel race** — refcount the call handle across threads; the cancel
   hook signals the inbound condvar so a blocked `Read()` returns false and
   the worker unwinds before teardown completes.

## API additions (summary)

- `grpcuds::Server::WakeupFd() -> int` — add to the caller's poll set.
- `grpcuds::Server::DrainOutbound()` — call on the I/O thread when the
  wakeup fd is readable; flushes mailboxes into the core.
- Phase 2: `grpcuds::ServerReader<T>` + `grpcuds_call_*` inbound hooks +
  wrapper worker-pool config on `ServerBuilder`.
- No `grpcuds_*` removals, and Phase 1 adds no new C ABI symbols.
- The `tests/cpp` poll loop gains the wakeup fd + drain call as the
  reference integration.
