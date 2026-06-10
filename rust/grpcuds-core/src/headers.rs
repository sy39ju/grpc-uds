// SPDX-License-Identifier: MIT OR Apache-2.0
//! HTTP/2 + gRPC response header builders for nghttp2.
//!
//! nghttp2 takes an array of `nghttp2_nv`. By default it copies both name
//! and value into its own buffer; with `NO_COPY_*` flags the caller must
//! keep the data alive until `nghttp2_on_frame_send_callback` fires.
//!
//!  - Static names/values (`b"..."`) are `'static` and use NO_COPY flags.
//!  - Dynamic values (e.g. `grpc-status` digits rendered into a stack buffer)
//!    rely on nghttp2's default copy — caller buffer can die after submit.
//!
//! All names MUST be lowercase (HTTP/2 requires it; nghttp2 enforces it for
//! NO_COPY entries — it lowercases internally for the copy path).

use core::ffi::c_uint;

use alloc::vec::Vec;

use grpcuds_sys::{
    nghttp2_nv, nghttp2_nv_flag_NGHTTP2_NV_FLAG_NO_COPY_NAME as NV_NO_COPY_NAME,
    nghttp2_nv_flag_NGHTTP2_NV_FLAG_NO_COPY_VALUE as NV_NO_COPY_VALUE,
};

// ---- gRPC status codes (RFC) ----------------------------------------------

/// A gRPC status code — the canonical RFC values, identical to what stock
/// gRPC peers send and expect on the wire (`grpc-status` trailer). `Ok`
/// (0) means success; every other value is an error. See the
/// [gRPC status-code reference](https://grpc.io/docs/guides/status-codes/)
/// for the full guidance on when to use each.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum GrpcStatus {
    /// Success.
    Ok = 0,
    /// The operation was cancelled, typically by the caller.
    Cancelled = 1,
    /// Unknown error — e.g. a status from another address space with no
    /// known mapping, or a handler that failed without a specific code.
    Unknown = 2,
    /// The client supplied an invalid argument (independent of system
    /// state — unlike `FailedPrecondition`).
    InvalidArgument = 3,
    /// The deadline expired before the operation completed.
    DeadlineExceeded = 4,
    /// A requested entity (e.g. file or directory) was not found.
    NotFound = 5,
    /// The entity a client tried to create already exists.
    AlreadyExists = 6,
    /// The caller lacks permission to execute the operation (distinct from
    /// `Unauthenticated`, which is about identity, and `ResourceExhausted`).
    PermissionDenied = 7,
    /// A resource has been exhausted — a quota, or the whole filesystem.
    ResourceExhausted = 8,
    /// The system is not in a state required for the operation (e.g. a
    /// precondition that the client should fix before retrying).
    FailedPrecondition = 9,
    /// The operation was aborted, typically a concurrency conflict such as
    /// a transaction abort.
    Aborted = 10,
    /// The operation was attempted past the valid range (e.g. seeking past
    /// end-of-file).
    OutOfRange = 11,
    /// The operation is not implemented, or not supported/enabled here.
    Unimplemented = 12,
    /// Internal error — an invariant expected by the system was broken.
    Internal = 13,
    /// The service is currently unavailable (transient; the caller may
    /// retry with backoff).
    Unavailable = 14,
    /// Unrecoverable data loss or corruption.
    DataLoss = 15,
    /// The request lacks valid authentication credentials for the
    /// operation.
    Unauthenticated = 16,
}

/// Map a numeric grpc-status to a [`GrpcStatus`] (client side). Out-of-range
/// values collapse to `Unknown`.
#[cfg(feature = "client")]
pub fn grpc_status_from_i32(code: i32) -> GrpcStatus {
    match code {
        0 => GrpcStatus::Ok,
        1 => GrpcStatus::Cancelled,
        2 => GrpcStatus::Unknown,
        3 => GrpcStatus::InvalidArgument,
        4 => GrpcStatus::DeadlineExceeded,
        5 => GrpcStatus::NotFound,
        6 => GrpcStatus::AlreadyExists,
        7 => GrpcStatus::PermissionDenied,
        8 => GrpcStatus::ResourceExhausted,
        9 => GrpcStatus::FailedPrecondition,
        10 => GrpcStatus::Aborted,
        11 => GrpcStatus::OutOfRange,
        12 => GrpcStatus::Unimplemented,
        13 => GrpcStatus::Internal,
        14 => GrpcStatus::Unavailable,
        15 => GrpcStatus::DataLoss,
        16 => GrpcStatus::Unauthenticated,
        _ => GrpcStatus::Unknown,
    }
}

// ---- Static name/value strings --------------------------------------------

const N_STATUS: &[u8] = b":status";
const V_200: &[u8] = b"200";
const N_CONTENT_TYPE: &[u8] = b"content-type";
const V_APP_GRPC: &[u8] = b"application/grpc";
const N_GRPC_STATUS: &[u8] = b"grpc-status";
const N_GRPC_MESSAGE: &[u8] = b"grpc-message";

// Pre-rendered digit values for every gRPC status code (0..=16).
const V_GRPC_STATUS: [&[u8]; 17] = [
    b"0", b"1", b"2", b"3", b"4", b"5", b"6", b"7", b"8", b"9", b"10", b"11", b"12", b"13", b"14",
    b"15", b"16",
];

const NO_COPY_BOTH: u8 = (NV_NO_COPY_NAME as c_uint | NV_NO_COPY_VALUE as c_uint) as u8;

#[inline]
const fn nv_static(name: &'static [u8], value: &'static [u8]) -> nghttp2_nv {
    nghttp2_nv {
        name: name.as_ptr() as *mut u8,
        value: value.as_ptr() as *mut u8,
        namelen: name.len(),
        valuelen: value.len(),
        flags: NO_COPY_BOTH,
    }
}

/// Static name + dynamic value. Only the name is NO_COPY; nghttp2 copies the
/// value into its own buffer at submit time, so `value` may be dropped once
/// the submit call returns.
#[inline]
fn nv_dyn_value(name: &'static [u8], value: &[u8]) -> nghttp2_nv {
    nghttp2_nv {
        name: name.as_ptr() as *mut u8,
        value: value.as_ptr() as *mut u8,
        namelen: name.len(),
        valuelen: value.len(),
        flags: NV_NO_COPY_NAME as u8,
    }
}

/// Percent-encode a gRPC status message per the gRPC HTTP/2 spec: bytes
/// outside the printable ASCII range `0x20..=0x7E`, and `%` itself, become
/// `%XX` (uppercase hex). Everything else passes through unchanged, so plain
/// ASCII messages (with spaces) are emitted verbatim.
///
/// Appends into `out`. Never panics; no `core::fmt`.
pub fn percent_encode_message(msg: &[u8], out: &mut Vec<u8>) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for &b in msg {
        if !(0x20..=0x7E).contains(&b) || b == b'%' {
            out.push(b'%');
            out.push(HEX[(b >> 4) as usize]);
            out.push(HEX[(b & 0x0F) as usize]);
        } else {
            out.push(b);
        }
    }
}

// ---- Public builders ------------------------------------------------------

/// `:status: 200` + `content-type: application/grpc`. Suitable for the
/// initial response HEADERS frame of any gRPC server call.
#[inline]
pub fn response_headers() -> [nghttp2_nv; 2] {
    [
        nv_static(N_STATUS, V_200),
        nv_static(N_CONTENT_TYPE, V_APP_GRPC),
    ]
}

/// Trailing HEADERS frame carrying just `grpc-status: N`. Use this when no
/// `grpc-message` is needed (the common success path and most error paths).
///
/// Returns a single-element array because nghttp2 takes an array pointer +
/// length.
#[inline]
pub fn trailer(status: GrpcStatus) -> [nghttp2_nv; 1] {
    let idx = status as usize;
    // Bounds-safe by construction: enum discriminants 0..=16.
    let val = V_GRPC_STATUS[idx];
    [nv_static(N_GRPC_STATUS, val)]
}

/// Trailing HEADERS frame carrying `grpc-status: N` + `grpc-message: <msg>`.
/// `msg` must already be percent-encoded (see [`percent_encode_message`]).
/// The returned array borrows `msg`, which must stay alive until the
/// `nghttp2_submit_trailer` call returns (nghttp2 copies the value then).
#[inline]
pub fn trailer_with_message(status: GrpcStatus, msg: &[u8]) -> [nghttp2_nv; 2] {
    let val = V_GRPC_STATUS[status as usize];
    [
        nv_static(N_GRPC_STATUS, val),
        nv_dyn_value(N_GRPC_MESSAGE, msg),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(nv: &nghttp2_nv) -> (&[u8], &[u8]) {
        // SAFETY: we built these from &[u8] references that are still alive
        // for the duration of the test.
        unsafe {
            (
                core::slice::from_raw_parts(nv.name, nv.namelen),
                core::slice::from_raw_parts(nv.value, nv.valuelen),
            )
        }
    }

    #[test]
    fn response_headers_match_grpc_wire() {
        let nvs = response_headers();
        assert_eq!(bytes(&nvs[0]), (&b":status"[..], &b"200"[..]));
        assert_eq!(
            bytes(&nvs[1]),
            (&b"content-type"[..], &b"application/grpc"[..])
        );
        assert_eq!(nvs[0].flags, NO_COPY_BOTH);
    }

    #[test]
    fn trailer_ok_is_zero() {
        let nvs = trailer(GrpcStatus::Ok);
        assert_eq!(bytes(&nvs[0]), (&b"grpc-status"[..], &b"0"[..]));
    }

    #[test]
    fn trailer_unimplemented_is_twelve() {
        let nvs = trailer(GrpcStatus::Unimplemented);
        assert_eq!(bytes(&nvs[0]), (&b"grpc-status"[..], &b"12"[..]));
    }

    #[test]
    fn trailer_unauthenticated_is_sixteen() {
        let nvs = trailer(GrpcStatus::Unauthenticated);
        assert_eq!(bytes(&nvs[0]), (&b"grpc-status"[..], &b"16"[..]));
    }

    /// The grpc-message value is a caller temporary (percent-encode buffer):
    /// nghttp2 must COPY it at submit time, so NO_COPY_VALUE on that nv
    /// would be a use-after-free. The name stays NO_COPY (it's 'static).
    #[test]
    fn trailer_with_message_copies_the_value() {
        let msg = b"not%20found";
        let nvs = trailer_with_message(GrpcStatus::NotFound, msg);
        assert_eq!(bytes(&nvs[0]), (&b"grpc-status"[..], &b"5"[..]));
        assert_eq!(bytes(&nvs[1]), (&b"grpc-message"[..], &msg[..]));
        assert_eq!(nvs[0].flags, NO_COPY_BOTH);
        assert_eq!(nvs[1].flags, NV_NO_COPY_NAME as u8);
    }

    #[test]
    fn percent_encode_passes_printable_ascii_through() {
        let mut out = alloc::vec::Vec::new();
        percent_encode_message(b"scan failed: adapter hci0 (code 3)", &mut out);
        assert_eq!(out, b"scan failed: adapter hci0 (code 3)");
    }

    /// The gRPC HTTP/2 spec: bytes outside 0x20..=0x7E and '%' itself
    /// become %XX with uppercase hex. The boundaries are the contract.
    #[test]
    fn percent_encode_escapes_the_spec_byte_set() {
        let mut out = alloc::vec::Vec::new();
        percent_encode_message(&[0x1F, 0x20, 0x7E, 0x7F, b'%'], &mut out);
        assert_eq!(out, b"%1F ~%7F%25");
    }

    #[test]
    fn percent_encode_escapes_utf8_and_control_bytes() {
        let mut out = alloc::vec::Vec::new();
        // "é" = 0xC3 0xA9; newline + NUL are control bytes.
        percent_encode_message("é\n\0".as_bytes(), &mut out);
        assert_eq!(out, b"%C3%A9%0A%00");
    }

    /// Appends — callers build trailers into a reused buffer.
    #[test]
    fn percent_encode_appends_without_clearing() {
        let mut out = alloc::vec::Vec::from(&b"x"[..]);
        percent_encode_message(b"", &mut out);
        assert_eq!(out, b"x");
        percent_encode_message(b"y", &mut out);
        assert_eq!(out, b"xy");
    }
}

#[cfg(all(test, feature = "client"))]
mod client_status_tests {
    use super::*;

    /// Every wire integer maps to its enum twin; anything out of range —
    /// negative, 17, huge — collapses to Unknown (never panics, never
    /// transmutes).
    #[test]
    fn grpc_status_from_i32_full_table_and_out_of_range() {
        for code in 0..=16 {
            assert_eq!(grpc_status_from_i32(code) as i32, code);
        }
        assert_eq!(grpc_status_from_i32(-1), GrpcStatus::Unknown);
        assert_eq!(grpc_status_from_i32(17), GrpcStatus::Unknown);
        assert_eq!(grpc_status_from_i32(i32::MAX), GrpcStatus::Unknown);
        assert_eq!(grpc_status_from_i32(i32::MIN), GrpcStatus::Unknown);
    }
}

/// Parse a `grpc-timeout` header value (`<1-8 digits><unit>`, units
/// `H`/`M`/`S`/`m`/`u`/`n`) into milliseconds. Sub-millisecond values
/// round UP to 1 ms (a 0 would expire a call before its handler runs).
/// Malformed values yield `None` — the spec says ignore, don't fail.
pub fn parse_grpc_timeout_ms(v: &[u8]) -> Option<u64> {
    if v.len() < 2 || v.len() > 9 {
        return None;
    }
    let (digits, unit) = v.split_at(v.len() - 1);
    let mut n: u64 = 0;
    for &d in digits {
        if !d.is_ascii_digit() {
            return None;
        }
        n = n * 10 + (d - b'0') as u64;
    }
    let ms = match unit[0] {
        b'H' => n.saturating_mul(3_600_000),
        b'M' => n.saturating_mul(60_000),
        b'S' => n.saturating_mul(1_000),
        b'm' => n,
        b'u' => n.div_ceil(1_000),
        b'n' => n.div_ceil(1_000_000),
        _ => return None,
    };
    Some(if ms == 0 { 1 } else { ms })
}

#[cfg(test)]
mod grpc_timeout_tests {
    use super::parse_grpc_timeout_ms;

    #[test]
    fn parses_every_unit() {
        assert_eq!(parse_grpc_timeout_ms(b"2H"), Some(7_200_000));
        assert_eq!(parse_grpc_timeout_ms(b"3M"), Some(180_000));
        assert_eq!(parse_grpc_timeout_ms(b"5S"), Some(5_000));
        assert_eq!(parse_grpc_timeout_ms(b"250m"), Some(250));
        assert_eq!(parse_grpc_timeout_ms(b"1500u"), Some(2), "rounds up");
        assert_eq!(parse_grpc_timeout_ms(b"999999n"), Some(1), "rounds up");
    }

    #[test]
    fn sub_millisecond_rounds_up_to_one() {
        assert_eq!(parse_grpc_timeout_ms(b"1n"), Some(1));
        assert_eq!(parse_grpc_timeout_ms(b"1u"), Some(1));
        assert_eq!(
            parse_grpc_timeout_ms(b"0m"),
            Some(1),
            "0 would insta-expire"
        );
    }

    #[test]
    fn rejects_malformed_values() {
        assert_eq!(parse_grpc_timeout_ms(b""), None);
        assert_eq!(parse_grpc_timeout_ms(b"S"), None);
        assert_eq!(parse_grpc_timeout_ms(b"12"), None, "missing unit");
        assert_eq!(parse_grpc_timeout_ms(b"1x"), None, "bad unit");
        assert_eq!(parse_grpc_timeout_ms(b"123456789S"), None, "9 digits");
        assert_eq!(parse_grpc_timeout_ms(b"-1S"), None);
    }

    /// 8 digits is the spec maximum — must parse, including at the largest
    /// unit without overflowing (99,999,999 H fits u64 comfortably).
    #[test]
    fn accepts_the_eight_digit_maximum() {
        assert_eq!(parse_grpc_timeout_ms(b"12345678m"), Some(12_345_678));
        assert_eq!(
            parse_grpc_timeout_ms(b"99999999H"),
            Some(99_999_999u64 * 3_600_000)
        );
    }
}
