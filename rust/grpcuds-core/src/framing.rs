// SPDX-License-Identifier: MIT OR Apache-2.0
//! gRPC message framing (5-byte length-prefix).
//!
//! Layout per gRPC HTTP/2 spec:
//!     compressed-flag (1B) | length (4B big-endian) | payload
//!
//! This crate never compresses outgoing messages and refuses incoming
//! compressed messages — there is no decompressor on the server side.

pub const FRAME_HEADER_LEN: usize = 5;

/// Per-message size cap, matching gRPC's typical 16 MiB default. A request
/// frame larger than this is rejected as a session-fatal error in
/// `on_data_chunk_recv` (it bounds per-connection request buffering). Fixed
/// for now — there is no per-server override yet.
pub const DEFAULT_MAX_MESSAGE_LEN: u32 = 16 * 1024 * 1024;

#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
#[derive(Clone, Copy)]
pub struct FrameHeader {
    pub compressed: bool,
    pub payload_len: u32,
}

#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
#[derive(Clone, Copy)]
pub enum FrameError {
    /// Fewer than `FRAME_HEADER_LEN` bytes available — caller should buffer more.
    NeedMore,
    /// `payload_len` exceeds the configured maximum.
    TooLarge,
    /// Compressed flag was set; we do not support compression.
    Compressed,
}

#[inline]
pub fn encode_header(compressed: bool, payload_len: u32, out: &mut [u8; FRAME_HEADER_LEN]) {
    out[0] = compressed as u8;
    let len_be = payload_len.to_be_bytes();
    out[1] = len_be[0];
    out[2] = len_be[1];
    out[3] = len_be[2];
    out[4] = len_be[3];
}

/// Parse a 5-byte gRPC frame header. `buf` may be longer than 5 — only the
/// first 5 bytes are consumed.
#[inline]
pub fn decode_header(buf: &[u8], max_len: u32) -> Result<FrameHeader, FrameError> {
    let head = buf.get(..FRAME_HEADER_LEN).ok_or(FrameError::NeedMore)?;
    let compressed = match head[0] {
        0 => false,
        // Spec defines 1 for "compressed"; any other value is reserved/invalid.
        // We refuse both for now.
        _ => return Err(FrameError::Compressed),
    };
    let payload_len = u32::from_be_bytes([head[1], head[2], head[3], head[4]]);
    if payload_len > max_len {
        return Err(FrameError::TooLarge);
    }
    Ok(FrameHeader {
        compressed,
        payload_len,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_roundtrip_zero() {
        let mut buf = [0u8; FRAME_HEADER_LEN];
        encode_header(false, 0, &mut buf);
        assert_eq!(buf, [0, 0, 0, 0, 0]);
        let h = decode_header(&buf, DEFAULT_MAX_MESSAGE_LEN).unwrap();
        assert_eq!(
            h,
            FrameHeader {
                compressed: false,
                payload_len: 0
            }
        );
    }

    #[test]
    fn encode_roundtrip_big() {
        let mut buf = [0u8; FRAME_HEADER_LEN];
        // 0x0102_03FF > DEFAULT_MAX_MESSAGE_LEN — use an explicit ceiling.
        encode_header(false, 0x0102_03FF, &mut buf);
        assert_eq!(buf, [0, 0x01, 0x02, 0x03, 0xFF]);
        let h = decode_header(&buf, u32::MAX).unwrap();
        assert_eq!(
            h,
            FrameHeader {
                compressed: false,
                payload_len: 0x0102_03FF
            }
        );
    }

    #[test]
    fn need_more_when_buffer_short() {
        for n in 0..FRAME_HEADER_LEN {
            let buf = [0u8; FRAME_HEADER_LEN];
            assert_eq!(
                decode_header(&buf[..n], DEFAULT_MAX_MESSAGE_LEN),
                Err(FrameError::NeedMore),
                "n={n}"
            );
        }
    }

    #[test]
    fn refuse_compressed() {
        let buf = [1u8, 0, 0, 0, 0];
        assert_eq!(
            decode_header(&buf, DEFAULT_MAX_MESSAGE_LEN),
            Err(FrameError::Compressed)
        );
        // Reserved values are also refused.
        let buf = [7u8, 0, 0, 0, 0];
        assert_eq!(
            decode_header(&buf, DEFAULT_MAX_MESSAGE_LEN),
            Err(FrameError::Compressed)
        );
    }

    #[test]
    fn refuse_too_large() {
        let mut buf = [0u8; FRAME_HEADER_LEN];
        encode_header(false, 1024, &mut buf);
        assert_eq!(decode_header(&buf, 512), Err(FrameError::TooLarge));
        assert!(decode_header(&buf, 1024).is_ok());
    }

    #[test]
    fn ignores_trailing_bytes() {
        let buf = [0, 0, 0, 0, 3, 0xAA, 0xBB, 0xCC];
        let h = decode_header(&buf, DEFAULT_MAX_MESSAGE_LEN).unwrap();
        assert_eq!(h.payload_len, 3);
    }
}
