// SPDX-License-Identifier: MIT OR Apache-2.0
//! Per-stream outbound message queue + backpressure policy.
//!
//! `OutQueue` holds gRPC-framed messages (5-byte prefix already attached);
//! the per-stream `data_provider` read_callback in [`super::dispatch`] drains
//! from the front while `front_offset` tracks how many bytes of the head
//! message have already been shipped.
//!
//! [`Backpressure`] expresses bounded vs unbounded as one enum so the
//! capacity-without-policy / capacity-zero-but-policy-set misuses are
//! type-impossible. See [`super::Conn::set_stream_policy`].

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::num::NonZeroUsize;

use crate::framing::{encode_header, FRAME_HEADER_LEN};
use crate::headers::GrpcStatus;

use super::ConnError;

/// What to do when a [`Backpressure::Bounded`] queue is at capacity and the
/// application tries to enqueue another message. Mirrors
/// [DESIGN.md](https://github.com/sy39ju/grpc-uds/blob/main/DESIGN.md)
/// ("Backpressure: queue cap + policy"). There is no `Default` impl on
/// purpose — the caller must pick.
#[cfg_attr(test, derive(Debug))]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum OverflowPolicy {
    /// Refuse the new write — `write_call` returns `QueueFull`. The caller
    /// decides what to do (buffer externally, log, drop). Suitable for
    /// streams where every message matters (e.g. BLE GATT notifications).
    Reject,
    /// Discard the oldest *unstarted* message to make room. The currently
    /// in-flight message (front-of-queue, partially shipped) is never
    /// dropped. Suitable for "latest N wins" streams (e.g. BLE scan).
    DropOldest,
}

/// Backpressure configuration for an outbound stream queue.
///
/// `Unbounded` is the default and the safe initial state — nothing is
/// dropped, nothing fails, queue grows with the producer. `Bounded`
/// requires a non-zero capacity (encoded via [`NonZeroUsize`]) and an
/// explicit [`OverflowPolicy`] for what to do on overflow.
///
/// Set at registration time via
/// [`super::Conn::register_streaming_method`] or at runtime via
/// [`super::Conn::set_stream_policy`].
#[cfg_attr(test, derive(Debug))]
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum Backpressure {
    #[default]
    Unbounded,
    Bounded {
        capacity: NonZeroUsize,
        policy: OverflowPolicy,
    },
}

/// One queued response message: the 5-byte gRPC frame header kept inline
/// next to the application payload, which is taken **by ownership** so the
/// hot path (`enqueue_owned`) never copies message bytes.
pub(super) struct OutMsg {
    hdr: [u8; FRAME_HEADER_LEN],
    body: Vec<u8>,
}

impl OutMsg {
    /// Total wire length: frame header + payload.
    #[inline]
    pub(super) fn len(&self) -> usize {
        FRAME_HEADER_LEN + self.body.len()
    }

    /// The application payload (without the 5-byte frame header).
    /// Test-only observer; production code drains via byte regions.
    #[cfg(test)]
    #[inline]
    pub(super) fn body(&self) -> &[u8] {
        &self.body
    }
}

/// Per-stream outbound message queue.
///
/// Holds gRPC-framed messages (5-byte header + owned payload). The
/// data_provider read_callback drains from the front; `front_offset`
/// tracks how many bytes of the front message have already been shipped.
pub struct OutQueue {
    pub(super) messages: VecDeque<OutMsg>,
    pub(super) front_offset: usize,
    /// Bytes of the front message are promised to nghttp2 as a `NO_COPY`
    /// DATA frame (`promise_front`) but not yet consumed by the
    /// send_data_callback. While set, the front message must survive
    /// `DropOldest` even at `front_offset == 0`.
    pub(super) front_committed: bool,
    /// Application called `finish_call` — no more enqueues.
    pub(super) finished: bool,
    /// gRPC status to ship in the trailing HEADERS frame.
    pub(super) final_status: GrpcStatus,
    /// Optional `grpc-message` payload, already percent-encoded, to ship
    /// alongside `final_status`. `None` => status-only trailer.
    pub(super) final_message: Option<Vec<u8>>,
    /// `submit_response` already issued; avoid double-submit.
    pub(super) response_started: bool,
    pub(super) backpressure: Backpressure,
}

impl OutQueue {
    pub(super) fn new() -> Self {
        Self {
            messages: VecDeque::new(),
            front_offset: 0,
            front_committed: false,
            finished: false,
            final_status: GrpcStatus::Ok,
            final_message: None,
            response_started: false,
            backpressure: Backpressure::Unbounded,
        }
    }

    /// Count of queued messages whose first byte has not yet been pulled by
    /// nghttp2. The in-flight head message — partially shipped
    /// (`front_offset > 0`) or promised to a `NO_COPY` DATA frame
    /// (`front_committed`) — is excluded because we can never rewind it.
    fn unstarted_count(&self) -> usize {
        if self.front_offset > 0 || self.front_committed {
            self.messages.len().saturating_sub(1)
        } else {
            self.messages.len()
        }
    }

    /// Unshipped bytes remaining in the front message (0 if the queue is
    /// empty).
    pub(super) fn front_remaining(&self) -> usize {
        self.messages
            .front()
            .map(|m| m.len() - self.front_offset)
            .unwrap_or(0)
    }

    /// Promise up to `max` bytes of the front message to nghttp2 as a
    /// `NO_COPY` DATA frame. Marks the front in-flight (DropOldest-proof)
    /// and returns the promised length. A promise never spans messages.
    pub(super) fn promise_front(&mut self, max: usize) -> usize {
        let n = self.front_remaining().min(max);
        if n > 0 {
            self.front_committed = true;
        }
        n
    }

    /// The byte regions of the front message covering `n` bytes starting at
    /// `front_offset`: (frame-header part, payload part). Either slice may
    /// be empty; combined they are exactly `n` bytes (clamped to what is
    /// actually available).
    pub(super) fn front_regions(&self, n: usize) -> (&[u8], &[u8]) {
        let front = match self.messages.front() {
            Some(f) => f,
            None => return (&[], &[]),
        };
        let n = n.min(front.len() - self.front_offset);
        let end = self.front_offset + n;
        let hdr_part = if self.front_offset < FRAME_HEADER_LEN {
            &front.hdr[self.front_offset..end.min(FRAME_HEADER_LEN)]
        } else {
            &[][..]
        };
        let body_start = self.front_offset.max(FRAME_HEADER_LEN) - FRAME_HEADER_LEN;
        let body_end = end.max(FRAME_HEADER_LEN) - FRAME_HEADER_LEN;
        let body_part = &front.body[body_start..body_end];
        (hdr_part, body_part)
    }

    /// Mark `n` promised bytes as shipped (the send_data_callback wrote them
    /// to the socket) and release the in-flight commit.
    pub(super) fn consume_front(&mut self, n: usize) {
        let done = match self.messages.front() {
            Some(front) => {
                self.front_offset += n.min(front.len() - self.front_offset);
                self.front_offset == front.len()
            }
            None => false,
        };
        if done {
            self.messages.pop_front();
            self.front_offset = 0;
        }
        self.front_committed = false;
    }

    /// Borrowed-payload enqueue (the C ABI path — C keeps ownership, so one
    /// copy is unavoidable here).
    pub(super) fn enqueue_framed(&mut self, payload: &[u8]) -> Result<(), ConnError> {
        let mut body: Vec<u8> = Vec::new();
        body.try_reserve_exact(payload.len())
            .map_err(|_| ConnError::OutOfMemory)?;
        body.extend_from_slice(payload);
        self.enqueue_owned(body)
    }

    /// Owned-payload enqueue — the hot path: the message bytes are moved
    /// into the queue, never copied (the 5-byte frame header lives inline
    /// in the queue entry).
    pub(super) fn enqueue_owned(&mut self, body: Vec<u8>) -> Result<(), ConnError> {
        // The gRPC frame length prefix is 4 bytes (u32). A handler trying to
        // ship a >=4 GiB single message would otherwise truncate it silently
        // into a corrupt frame; reject instead. Unreachable from the wire
        // (request side is capped by max_message_len) — guards local output.
        if body.len() > u32::MAX as usize {
            return Err(ConnError::QueueFull);
        }
        if let Backpressure::Bounded { capacity, policy } = self.backpressure {
            if self.unstarted_count() >= capacity.get() {
                match policy {
                    OverflowPolicy::Reject => {
                        crate::logging::debug(
                            c"backpressure rejected write",
                            capacity.get() as i64,
                        );
                        return Err(ConnError::QueueFull);
                    }
                    OverflowPolicy::DropOldest => {
                        crate::logging::debug(
                            c"backpressure dropped oldest message",
                            capacity.get() as i64,
                        );
                        // Drop the oldest *unstarted* message. If the head is
                        // mid-flight (front_offset > 0) or promised to a
                        // NO_COPY frame (front_committed), skip it and drop
                        // the next-oldest at index 1.
                        let drop_idx = if self.front_offset > 0 || self.front_committed {
                            1
                        } else {
                            0
                        };
                        if drop_idx < self.messages.len() {
                            self.messages.remove(drop_idx);
                        }
                    }
                }
            }
        }
        let mut hdr = [0u8; FRAME_HEADER_LEN];
        encode_header(false, body.len() as u32, &mut hdr);
        self.messages
            .try_reserve(1)
            .map_err(|_| ConnError::OutOfMemory)?;
        self.messages.push_back(OutMsg { hdr, body });
        Ok(())
    }

    /// Copy up to `dst.len()` queued bytes into `dst`. Returns bytes written.
    pub(super) fn drain_into(&mut self, dst: &mut [u8]) -> usize {
        let mut written = 0;
        while written < dst.len() {
            let front = match self.messages.front() {
                Some(f) => f,
                None => break,
            };
            // The logical message is hdr ++ body; front_offset indexes into
            // that concatenation. Copy whichever region(s) the window spans.
            if self.front_offset < FRAME_HEADER_LEN {
                let remaining = &front.hdr[self.front_offset..];
                let take = remaining.len().min(dst.len() - written);
                dst[written..written + take].copy_from_slice(&remaining[..take]);
                written += take;
                self.front_offset += take;
                if written == dst.len() {
                    break;
                }
            }
            let body_off = self.front_offset - FRAME_HEADER_LEN;
            let remaining = &front.body[body_off..];
            let take = remaining.len().min(dst.len() - written);
            dst[written..written + take].copy_from_slice(&remaining[..take]);
            written += take;
            self.front_offset += take;
            if self.front_offset == front.len() {
                self.messages.pop_front();
                self.front_offset = 0;
            }
        }
        written
    }
}
