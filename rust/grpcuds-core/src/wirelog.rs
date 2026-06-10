// SPDX-License-Identifier: MIT OR Apache-2.0
//! Dev-only wire logging (the `wirelog` build feature — OFF by default,
//! zero code in normal builds).
//!
//! Every byte that crosses a grpcuds socket is appended to a
//! **Wireshark-readable pcap**: UDS traffic has no TCP/IP framing, so each
//! chunk is wrapped in a synthetic IPv4+TCP header (one fake TCP stream
//! per connection, with a fabricated handshake and consistent seq/ack).
//! The synthetic server port is 80, so Wireshark's HTTP dissector picks
//! the stream up by default, spots the client connection preface
//! (`PRI * HTTP/2.0`) and dissects HTTP/2 → gRPC from there.
//!
//! Activation is at runtime: compiled in by the feature, **enabled only
//! when `GRPCUDS_WIRELOG=<path>.pcap` is set** in the environment. Both
//! the server and the client side log when built with the feature; in a
//! single process each connection appears as its own TCP stream.
//!
//! Rotation: the live file is capped at 1 MiB; on overflow it rolls to
//! `<path>.1` → `<path>.2` (oldest dropped) — at most 3 files / 3 MiB on
//! disk including the live one. Both knobs are environment-tunable:
//! `GRPCUDS_WIRELOG_FILE_KB` (per-file cap, KiB) and
//! `GRPCUDS_WIRELOG_FILES` (total count including the live file, 1..=10).
//! Note: a rotated file starts mid-stream (no preface), so Wireshark may
//! need "Decode As → HTTP2" for it.
//!
//! Caveats (it is a dev tool):
//! - One process per pcap path: rotation renames race across processes.
//!   Point each side at its own file when server and client are separate
//!   processes.
//! - TCP checksums are zero (Wireshark validation is off by default).
//! - The sink is a spinlock held across file I/O (write / rotation): fine
//!   for a dev capture, but RT-priority threads can priority-invert on
//!   it, and a fork() taken while another thread holds the lock leaves
//!   the child's first wirelog call spinning forever — don't fork with
//!   capture enabled in multi-threaded processes.

use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};

/// Synthetic endpoint constants. The server port is 80 ON PURPOSE: that is
/// the port Wireshark's HTTP dissector owns by default, and it is the HTTP
/// dissector that recognizes the `PRI * HTTP/2.0` preface and hands the
/// conversation to HTTP/2 → gRPC with no "Decode As" step. (Cleartext
/// HTTP/2 on an arbitrary port is NOT auto-detected by default.)
const CLIENT_IP: [u8; 4] = [10, 117, 117, 1];
const SERVER_IP: [u8; 4] = [10, 117, 117, 2];
const SERVER_PORT: u16 = 80;

/// Rotation policy defaults — overridable per process via environment:
/// `GRPCUDS_WIRELOG_FILE_KB` (per-file cap in KiB, clamped to 4..=1048576)
/// and `GRPCUDS_WIRELOG_FILES` (total file count INCLUDING the live one,
/// clamped to 1..=10). Defaults: 1 MiB × 3 (live + `.1` + `.2`).
const DEFAULT_FILE_BYTES: u64 = 1024 * 1024;
const DEFAULT_FILES: u8 = 3;
const MIN_FILE_KB: u64 = 4; // > pcap header + one max-size record
const MAX_FILE_KB: u64 = 1024 * 1024; // 1 GiB
const MAX_FILES: u8 = 10; // single-digit rotation suffixes (.1 ... .9)

/// One synthetic packet carries at most this much payload (MTU-ish — keeps
/// records realistic and the stack buffer small).
const MAX_SEGMENT: usize = 1400;

const PCAP_HEADER_LEN: usize = 24;
const REC_OVERHEAD: usize = 16 + 20 + 20; // record hdr + IPv4 + TCP

/// Which way the bytes flowed (in gRPC terms, not socket terms).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    ClientToServer,
    ServerToClient,
}

/// Per-connection logging state: the fake TCP 4-tuple + seq counters.
/// Owned by the connection object; `None` (via [`conn_open`]) when wire
/// logging is disabled, making every call site a cheap no-op.
pub struct WirelogConn {
    client_port: u16,
    /// Last octet of the synthetic client IP (wrap counter fold — see
    /// [`conn_open`]).
    ip_octet: u8,
    seq_c: u32,
    seq_s: u32,
}

// ---- global sink -------------------------------------------------------------

const ST_UNINIT: u8 = 0;
const ST_DISABLED: u8 = 1;
const ST_ENABLED: u8 = 2;

static STATE: AtomicU8 = AtomicU8::new(ST_UNINIT);
static LOCK: AtomicBool = AtomicBool::new(false);
static NEXT_PORT: AtomicU32 = AtomicU32::new(0);

struct Globals {
    fd: i32,
    written: u64,
    /// Per-file byte cap (from `GRPCUDS_WIRELOG_FILE_KB`).
    max_file: u64,
    /// Total file count including the live one (from `GRPCUDS_WIRELOG_FILES`).
    keep_files: u8,
    /// NUL-terminated base path (room left for the ".N" suffix).
    path: [u8; 512],
    path_len: usize, // without the NUL
}

struct LockedGlobals(UnsafeCell<Globals>);
// SAFETY: the inner Globals is only ever touched inside `with_lock`.
unsafe impl Sync for LockedGlobals {}

static G: LockedGlobals = LockedGlobals(UnsafeCell::new(Globals {
    fd: -1,
    written: 0,
    max_file: DEFAULT_FILE_BYTES,
    keep_files: DEFAULT_FILES,
    path: [0; 512],
    path_len: 0,
}));

/// Parse a decimal environment variable; `None` when unset/empty/non-digit.
fn env_u64(name: &core::ffi::CStr) -> Option<u64> {
    let p = unsafe { libc::getenv(name.as_ptr()) };
    if p.is_null() {
        return None;
    }
    let mut v: u64 = 0;
    let mut i = 0;
    loop {
        let c = unsafe { *p.add(i) } as u8;
        if c == 0 {
            break;
        }
        if !c.is_ascii_digit() {
            return None;
        }
        v = v.saturating_mul(10).saturating_add(u64::from(c - b'0'));
        i += 1;
    }
    if i == 0 {
        None
    } else {
        Some(v)
    }
}

fn with_lock<R>(f: impl FnOnce(&mut Globals) -> R) -> R {
    while LOCK
        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }
    // SAFETY: the spinlock above serializes all access.
    let r = f(unsafe { &mut *G.0.get() });
    LOCK.store(false, Ordering::Release);
    r
}

/// First-use initialization: read `GRPCUDS_WIRELOG`, open/append the file.
/// Idempotent; the decision latches for the process lifetime.
fn ensure_init() -> bool {
    match STATE.load(Ordering::Acquire) {
        ST_ENABLED => return true,
        ST_DISABLED => return false,
        _ => {}
    }
    with_lock(|g| {
        match STATE.load(Ordering::Relaxed) {
            ST_ENABLED => return true,
            ST_DISABLED => return false,
            _ => {}
        }
        let env = unsafe { libc::getenv(c"GRPCUDS_WIRELOG".as_ptr()) };
        if env.is_null() {
            STATE.store(ST_DISABLED, Ordering::Release);
            return false;
        }
        let mut len = 0;
        while len < g.path.len() - 8 && unsafe { *env.add(len) } != 0 {
            g.path[len] = unsafe { *env.add(len) } as u8;
            len += 1;
        }
        if len == 0 || len >= g.path.len() - 8 {
            STATE.store(ST_DISABLED, Ordering::Release);
            return false;
        }
        g.path[len] = 0;
        g.path_len = len;
        g.max_file = env_u64(c"GRPCUDS_WIRELOG_FILE_KB")
            .map(|kb| kb.clamp(MIN_FILE_KB, MAX_FILE_KB) * 1024)
            .unwrap_or(DEFAULT_FILE_BYTES);
        g.keep_files = env_u64(c"GRPCUDS_WIRELOG_FILES")
            .map(|n| (n.min(u64::from(MAX_FILES)) as u8).max(1))
            .unwrap_or(DEFAULT_FILES);
        if !open_live(g) {
            STATE.store(ST_DISABLED, Ordering::Release);
            return false;
        }
        STATE.store(ST_ENABLED, Ordering::Release);
        true
    })
}

/// Open (append) the live file; write the pcap global header if it is new.
/// Caller holds the lock.
fn open_live(g: &mut Globals) -> bool {
    let fd = unsafe {
        libc::open(
            g.path.as_ptr() as *const libc::c_char,
            libc::O_WRONLY | libc::O_CREAT | libc::O_APPEND | libc::O_CLOEXEC,
            0o644 as libc::c_uint,
        )
    };
    if fd < 0 {
        return false;
    }
    let mut st: libc::stat = unsafe { core::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut st) } != 0 {
        unsafe { libc::close(fd) };
        return false;
    }
    g.fd = fd;
    g.written = st.st_size as u64;
    if g.written == 0 {
        let hdr = pcap_global_header();
        write_fully(fd, &hdr);
        g.written = PCAP_HEADER_LEN as u64;
    }
    true
}

/// pcap classic, little-endian, snaplen 65535, LINKTYPE_RAW (raw IPv4).
fn pcap_global_header() -> [u8; PCAP_HEADER_LEN] {
    let mut h = [0u8; PCAP_HEADER_LEN];
    h[0..4].copy_from_slice(&0xa1b2_c3d4u32.to_le_bytes());
    h[4..6].copy_from_slice(&2u16.to_le_bytes()); // major
    h[6..8].copy_from_slice(&4u16.to_le_bytes()); // minor
                                                  // thiszone + sigfigs stay 0.
    h[16..20].copy_from_slice(&65_535u32.to_le_bytes()); // snaplen
    h[20..24].copy_from_slice(&101u32.to_le_bytes()); // LINKTYPE_RAW
    h
}

fn write_fully(fd: i32, mut data: &[u8]) {
    while !data.is_empty() {
        let n = unsafe { libc::write(fd, data.as_ptr() as *const c_void, data.len()) };
        if n <= 0 {
            if n < 0 && unsafe { *libc::__errno_location() } == libc::EINTR {
                continue;
            }
            return; // dev logging never fails the caller
        }
        data = &data[n as usize..];
    }
}

/// `<base>` + `.N` as a NUL-terminated byte path. Caller holds the lock.
fn suffixed(g: &Globals, n: u8) -> Vec<u8> {
    let mut p = Vec::new();
    if p.try_reserve_exact(g.path_len + 4).is_err() {
        return p;
    }
    p.extend_from_slice(&g.path[..g.path_len]);
    p.extend_from_slice(&[b'.', b'0' + n, 0]);
    p
}

/// Roll `<base>` → `<base>.1` → … → `<base>.N` (oldest dropped; N =
/// keep_files - 1) and start a fresh live file. With keep_files == 1 the
/// live file itself is dropped and restarted. Caller holds the lock.
fn rotate(g: &mut Globals) {
    unsafe { libc::close(g.fd) };
    g.fd = -1;
    let rotated = g.keep_files.saturating_sub(1);
    if rotated == 0 {
        unsafe { libc::unlink(g.path.as_ptr() as *const libc::c_char) };
    } else {
        let oldest = suffixed(g, rotated);
        if !oldest.is_empty() {
            unsafe { libc::unlink(oldest.as_ptr() as *const libc::c_char) };
        }
        let mut i = rotated;
        while i > 1 {
            let from = suffixed(g, i - 1);
            let to = suffixed(g, i);
            if !from.is_empty() && !to.is_empty() {
                unsafe {
                    libc::rename(
                        from.as_ptr() as *const libc::c_char,
                        to.as_ptr() as *const libc::c_char,
                    );
                }
            }
            i -= 1;
        }
        let p1 = suffixed(g, 1);
        if !p1.is_empty() {
            unsafe {
                libc::rename(
                    g.path.as_ptr() as *const libc::c_char,
                    p1.as_ptr() as *const libc::c_char,
                );
            }
        }
    }
    if !open_live(g) {
        STATE.store(ST_DISABLED, Ordering::Release);
    }
}

/// Append one finished pcap record, rotating first if it would overflow
/// the live file.
fn append_record(rec: &[u8]) {
    with_lock(|g| {
        if g.fd < 0 {
            return;
        }
        if g.written + rec.len() as u64 > g.max_file {
            rotate(g);
            if g.fd < 0 {
                return;
            }
        }
        write_fully(g.fd, rec);
        g.written += rec.len() as u64;
    });
}

// ---- synthetic packet construction --------------------------------------------

fn now() -> (u32, u32) {
    let mut ts: libc::timespec = unsafe { core::mem::zeroed() };
    unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts) };
    (ts.tv_sec as u32, (ts.tv_nsec / 1_000) as u32)
}

fn ipv4_checksum(hdr: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let mut i = 0;
    while i + 1 < hdr.len() {
        sum += u32::from(u16::from_be_bytes([hdr[i], hdr[i + 1]]));
        i += 2;
    }
    while sum > 0xFFFF {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

/// Build one pcap record (record header + IPv4 + TCP + payload) into `out`.
#[allow(clippy::too_many_arguments)]
fn build_record(
    out: &mut [u8; REC_OVERHEAD + MAX_SEGMENT],
    client_port: u16,
    ip_octet: u8,
    dir: Dir,
    seq: u32,
    ack: u32,
    flags: u8,
    payload: &[u8],
) -> usize {
    let total = REC_OVERHEAD + payload.len();
    let (sec, usec) = now();
    out[0..4].copy_from_slice(&sec.to_le_bytes());
    out[4..8].copy_from_slice(&usec.to_le_bytes());
    out[8..12].copy_from_slice(&((total - 16) as u32).to_le_bytes()); // incl_len
    out[12..16].copy_from_slice(&((total - 16) as u32).to_le_bytes()); // orig_len

    let mut client_ip = CLIENT_IP;
    client_ip[3] = ip_octet;
    let (src_ip, dst_ip, sport, dport) = match dir {
        Dir::ClientToServer => (client_ip, SERVER_IP, client_port, SERVER_PORT),
        Dir::ServerToClient => (SERVER_IP, client_ip, SERVER_PORT, client_port),
    };

    let ip = &mut out[16..36];
    ip[0] = 0x45; // v4, 20-byte header
    ip[1] = 0;
    ip[2..4].copy_from_slice(&((40 + payload.len()) as u16).to_be_bytes());
    ip[4..6].copy_from_slice(&0u16.to_be_bytes()); // id
    ip[6..8].copy_from_slice(&0x4000u16.to_be_bytes()); // DF
    ip[8] = 64; // ttl
    ip[9] = 6; // TCP
    ip[10..12].copy_from_slice(&[0, 0]);
    ip[12..16].copy_from_slice(&src_ip);
    ip[16..20].copy_from_slice(&dst_ip);
    let csum = ipv4_checksum(&out[16..36]);
    out[26..28].copy_from_slice(&csum.to_be_bytes());

    let tcp = &mut out[36..56];
    tcp[0..2].copy_from_slice(&sport.to_be_bytes());
    tcp[2..4].copy_from_slice(&dport.to_be_bytes());
    tcp[4..8].copy_from_slice(&seq.to_be_bytes());
    tcp[8..12].copy_from_slice(&ack.to_be_bytes());
    tcp[12] = 0x50; // data offset 5 words
    tcp[13] = flags;
    tcp[14..16].copy_from_slice(&0xFFFFu16.to_be_bytes()); // window
    tcp[16..18].copy_from_slice(&[0, 0]); // checksum (validation off)
    tcp[18..20].copy_from_slice(&[0, 0]);

    out[56..56 + payload.len()].copy_from_slice(payload);
    total
}

fn emit(client_port: u16, ip_octet: u8, dir: Dir, seq: u32, ack: u32, flags: u8, payload: &[u8]) {
    let mut buf = [0u8; REC_OVERHEAD + MAX_SEGMENT];
    let n = build_record(
        &mut buf,
        client_port,
        ip_octet,
        dir,
        seq,
        ack,
        flags,
        payload,
    );
    // get(..n) keeps the no-panic property provable; n ≤ buf.len() by
    // construction (payload callers chunk at MAX_SEGMENT).
    if let Some(rec) = buf.get(..n) {
        append_record(rec);
    }
}

// ---- public API ----------------------------------------------------------------

const F_SYN: u8 = 0x02;
const F_SYNACK: u8 = 0x12;
const F_ACK: u8 = 0x10;
const F_PSHACK: u8 = 0x18;

/// Start logging one connection. Returns `None` when wire logging is
/// disabled (feature compiled in but `GRPCUDS_WIRELOG` unset) — callers
/// keep the `Option` and every later call no-ops. Emits the fabricated
/// TCP handshake so Wireshark sees a well-formed stream.
pub fn conn_open() -> Option<WirelogConn> {
    if !ensure_init() {
        return None;
    }
    let seq = NEXT_PORT.fetch_add(1, Ordering::Relaxed);
    let client_port = 40_000 + (seq % 20_000) as u16;
    // Fold the wrap count into the synthetic client IP so >20k connections
    // in one process never reuse a live 4-tuple (Wireshark would merge the
    // conversations).
    let ip_octet = 1 + ((seq / 20_000) % 200) as u8;
    emit(client_port, ip_octet, Dir::ClientToServer, 0, 0, F_SYN, &[]);
    emit(
        client_port,
        ip_octet,
        Dir::ServerToClient,
        0,
        1,
        F_SYNACK,
        &[],
    );
    emit(client_port, ip_octet, Dir::ClientToServer, 1, 1, F_ACK, &[]);
    Some(WirelogConn {
        client_port,
        ip_octet,
        seq_c: 1,
        seq_s: 1,
    })
}

/// Log `bytes` as they crossed the socket in direction `dir`, advancing
/// the fake TCP stream. Chunked at `MAX_SEGMENT`.
pub fn log(wl: &mut WirelogConn, dir: Dir, bytes: &[u8]) {
    for chunk in bytes.chunks(MAX_SEGMENT) {
        let (seq, ack) = match dir {
            Dir::ClientToServer => (wl.seq_c, wl.seq_s),
            Dir::ServerToClient => (wl.seq_s, wl.seq_c),
        };
        emit(wl.client_port, wl.ip_octet, dir, seq, ack, F_PSHACK, chunk);
        match dir {
            Dir::ClientToServer => wl.seq_c = wl.seq_c.wrapping_add(chunk.len() as u32),
            Dir::ServerToClient => wl.seq_s = wl.seq_s.wrapping_add(chunk.len() as u32),
        }
    }
}

/// Test-only: drop the latched decision so a test can re-init from a fresh
/// `GRPCUDS_WIRELOG`. Other tests' connections may then also log — tests
/// must filter records by their own connection's port.
#[cfg(test)]
pub(crate) fn test_reset() {
    with_lock(|g| {
        if g.fd >= 0 {
            unsafe { libc::close(g.fd) };
            g.fd = -1;
        }
        g.written = 0;
        g.path_len = 0;
        STATE.store(ST_UNINIT, Ordering::Release);
    });
}

#[cfg(test)]
impl WirelogConn {
    pub(crate) fn client_port(&self) -> u16 {
        self.client_port
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// All global-state behavior in ONE test: the sink latches per process,
    /// so splitting into multiple #[test]s would race on init order. Other
    /// tests run in parallel and may log their own connections once the
    /// sink is enabled — every assertion filters by this test's port.
    #[test]
    fn end_to_end_pcap_sink_with_rotation() {
        let pid = unsafe { libc::getpid() };
        let base = alloc::format!("/tmp/grpcuds-wirelog-test-{pid}.pcap");
        let mut base_nul = base.clone().into_bytes();
        base_nul.push(0);
        for suffix in ["", ".1", ".2"] {
            let mut p = base.clone().into_bytes();
            p.extend_from_slice(suffix.as_bytes());
            p.push(0);
            unsafe { libc::unlink(p.as_ptr() as *const libc::c_char) };
        }
        unsafe {
            libc::setenv(
                c"GRPCUDS_WIRELOG".as_ptr(),
                base_nul.as_ptr() as *const libc::c_char,
                1,
            );
        }
        test_reset();

        let mut wl = conn_open().expect("enabled via env");
        let port = wl.client_port();
        log(
            &mut wl,
            Dir::ClientToServer,
            b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n",
        );
        log(&mut wl, Dir::ServerToClient, &[0, 0, 0, 4, 0, 0, 0, 0, 0]);

        // The live file starts with the LE pcap magic + LINKTYPE_RAW.
        let read = std::fs::read(&base).expect("live file");
        assert_eq!(&read[0..4], &[0xd4, 0xc3, 0xb2, 0xa1]);
        assert_eq!(&read[20..24], &101u32.to_le_bytes());

        // Records tile the file exactly; collect (sport, seq, payload) for
        // THIS connection only.
        let mut off = 24;
        let mut mine: Vec<(u16, u32, Vec<u8>)> = Vec::new();
        while off < read.len() {
            let incl =
                u32::from_le_bytes([read[off + 8], read[off + 9], read[off + 10], read[off + 11]])
                    as usize;
            let tcp = &read[off + 16 + 20..off + 16 + 40];
            let sport = u16::from_be_bytes([tcp[0], tcp[1]]);
            let dport = u16::from_be_bytes([tcp[2], tcp[3]]);
            if sport == port || dport == port {
                let seq = u32::from_be_bytes([tcp[4], tcp[5], tcp[6], tcp[7]]);
                mine.push((sport, seq, read[off + 16 + 40..off + 16 + incl].to_vec()));
            }
            off += 16 + incl;
        }
        assert_eq!(off, read.len(), "records tile the file exactly");
        assert_eq!(mine.len(), 5, "3 handshake + 2 data");
        assert!(mine[0].2.is_empty() && mine[1].2.is_empty() && mine[2].2.is_empty());
        assert_eq!(mine[3].2, b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");
        assert_eq!(mine[3].0, port, "preface flows client->server");
        assert_eq!(mine[3].1, 1, "first data byte is seq 1");
        assert_eq!(mine[4].0, SERVER_PORT, "reply flows server->client");

        // Rotation: pump > 2 MiB through and check the 3-file / 1 MiB cap,
        // every file starting with a valid pcap header.
        let big = alloc::vec![0xABu8; 64 * 1024];
        for _ in 0..40 {
            log(&mut wl, Dir::ServerToClient, &big);
        }
        let live = std::fs::metadata(&base).expect("live").len();
        let r1 = std::fs::metadata(alloc::format!("{base}.1"))
            .expect(".1")
            .len();
        let r2 = std::fs::metadata(alloc::format!("{base}.2"))
            .expect(".2")
            .len();
        assert!(live <= DEFAULT_FILE_BYTES, "live {live}");
        assert!(r1 <= DEFAULT_FILE_BYTES && r2 <= DEFAULT_FILE_BYTES);
        for f in [
            base.clone(),
            alloc::format!("{base}.1"),
            alloc::format!("{base}.2"),
        ] {
            let head = std::fs::read(&f).expect("file");
            assert_eq!(&head[0..4], &[0xd4, 0xc3, 0xb2, 0xa1], "{f}");
        }

        // Phase 2: the rotation knobs come from the environment — 4 KiB
        // per file, 2 files total (live + .1 only, never a .2).
        for suffix in ["", ".1", ".2"] {
            let mut p = base.clone().into_bytes();
            p.extend_from_slice(suffix.as_bytes());
            p.push(0);
            unsafe { libc::unlink(p.as_ptr() as *const libc::c_char) };
        }
        unsafe {
            libc::setenv(c"GRPCUDS_WIRELOG_FILE_KB".as_ptr(), c"4".as_ptr(), 1);
            libc::setenv(c"GRPCUDS_WIRELOG_FILES".as_ptr(), c"2".as_ptr(), 1);
        }
        test_reset();
        let mut wl2 = conn_open().expect("re-enabled");
        let blob = alloc::vec![0xCDu8; 1024];
        for _ in 0..32 {
            log(&mut wl2, Dir::ClientToServer, &blob);
        }
        assert!(std::fs::metadata(&base).expect("live").len() <= 4 * 1024);
        assert!(
            std::fs::metadata(alloc::format!("{base}.1"))
                .expect(".1")
                .len()
                <= 4 * 1024
        );
        assert!(
            std::fs::metadata(alloc::format!("{base}.2")).is_err(),
            "FILES=2 must never create .2"
        );

        unsafe {
            libc::unsetenv(c"GRPCUDS_WIRELOG_FILE_KB".as_ptr());
            libc::unsetenv(c"GRPCUDS_WIRELOG_FILES".as_ptr());
        }
        for suffix in ["", ".1", ".2"] {
            let mut p = base.clone().into_bytes();
            p.extend_from_slice(suffix.as_bytes());
            p.push(0);
            unsafe { libc::unlink(p.as_ptr() as *const libc::c_char) };
        }
    }

    /// The env parser feeding the knobs: digits only, saturating, None on
    /// junk so the defaults win.
    #[test]
    fn env_u64_parses_digits_and_rejects_junk() {
        unsafe {
            libc::setenv(c"GRPCUDS_WL_T1".as_ptr(), c"4096".as_ptr(), 1);
            libc::setenv(c"GRPCUDS_WL_T2".as_ptr(), c"12kb".as_ptr(), 1);
            libc::setenv(c"GRPCUDS_WL_T3".as_ptr(), c"".as_ptr(), 1);
        }
        assert_eq!(env_u64(c"GRPCUDS_WL_T1"), Some(4096));
        assert_eq!(env_u64(c"GRPCUDS_WL_T2"), None, "junk suffix");
        assert_eq!(env_u64(c"GRPCUDS_WL_T3"), None, "empty");
        assert_eq!(env_u64(c"GRPCUDS_WL_UNSET"), None);
    }
}
