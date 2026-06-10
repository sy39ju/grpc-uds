// SPDX-License-Identifier: MIT OR Apache-2.0
//! End-to-end proof for the dev-only `wirelog` feature: a real
//! server⇄client exchange is captured to pcap, then this test plays
//! Wireshark — it reassembles each fake TCP stream from the records and
//! walks the bytes as HTTP/2. The frame walk only succeeds if the capture
//! is BYTE-EXACT: one missing or duplicated byte anywhere breaks the
//! length-prefixed frame tiling.

#![cfg(all(feature = "wirelog", feature = "client"))]

use std::collections::HashMap;
use std::time::Duration;

use grpcuds::{Client, Server};

mod common;
use common::unique_path;

const SERVER_PORT: u16 = 80; // the synthetic port wirelog assigns the server
                             // (80 so Wireshark's HTTP dissector auto-detects)

/// Walk a byte string as consecutive HTTP/2 frames (3B length, 1B type,
/// 1B flags, 4B stream id, payload). Returns the frame count; panics if
/// the frames do not tile the bytes exactly.
fn walk_h2_frames(mut b: &[u8]) -> usize {
    let mut frames = 0;
    while !b.is_empty() {
        assert!(b.len() >= 9, "truncated HTTP/2 frame header");
        let len = u32::from_be_bytes([0, b[0], b[1], b[2]]) as usize;
        assert!(b.len() >= 9 + len, "truncated HTTP/2 frame body");
        b = &b[9 + len..];
        frames += 1;
    }
    frames
}

#[test]
fn capture_reassembles_into_byte_exact_http2_streams() {
    let pcap = format!("/tmp/grpcuds-wirelog-e2e-{}.pcap", std::process::id());
    for f in [pcap.clone(), format!("{pcap}.1"), format!("{pcap}.2")] {
        let _ = std::fs::remove_file(f);
    }
    std::env::set_var("GRPCUDS_WIRELOG", &pcap);

    let sock = unique_path();
    let srv = Server::builder()
        .bind(&sock)
        .add_unary("/echo.Echo/Unary", |req: &[u8]| Ok(req.to_vec()))
        .build()
        .expect("build")
        .run()
        .expect("run");
    let mut client = Client::connect_wait(&sock, Duration::from_secs(3)).expect("connect");
    for i in 0..5 {
        let req = format!("payload-{i}");
        let reply = client
            .unary("/echo.Echo/Unary", req.as_bytes())
            .expect("unary");
        assert_eq!(reply, req.as_bytes());
    }
    drop(client);
    drop(srv);

    // ---- play Wireshark: parse records, reassemble per (conn, direction) --

    let data = std::fs::read(&pcap).expect("capture file");
    assert_eq!(&data[0..4], &[0xd4, 0xc3, 0xb2, 0xa1], "pcap magic (LE)");
    assert_eq!(&data[20..24], &101u32.to_le_bytes(), "LINKTYPE_RAW");

    let mut streams: HashMap<(u16, bool), Vec<u8>> = HashMap::new(); // (client port, is C→S)
    let mut off = 24;
    while off < data.len() {
        let incl =
            u32::from_le_bytes(data[off + 8..off + 12].try_into().expect("rec hdr")) as usize;
        let tcp = &data[off + 16 + 20..off + 16 + 40];
        let sport = u16::from_be_bytes([tcp[0], tcp[1]]);
        let dport = u16::from_be_bytes([tcp[2], tcp[3]]);
        let payload = &data[off + 16 + 40..off + 16 + incl];
        let (cport, c2s) = if dport == SERVER_PORT {
            (sport, true)
        } else {
            (dport, false)
        };
        streams
            .entry((cport, c2s))
            .or_default()
            .extend_from_slice(payload);
        off += 16 + incl;
    }
    assert_eq!(off, data.len(), "records tile the capture exactly");

    // In-process server + client each log their own view of the SAME
    // connection: 2 fake TCP streams × 2 directions.
    assert_eq!(
        streams.len(),
        4,
        "client view + server view, both directions"
    );

    for ((cport, c2s), bytes) in &streams {
        if *c2s {
            assert!(
                bytes.starts_with(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n"),
                "C→S stream (port {cport}) must start with the preface \
                 (this is what Wireshark's HTTP/2 heuristic locks onto)"
            );
            let frames = walk_h2_frames(&bytes[24..]);
            assert!(frames >= 7, "SETTINGS + 5×(HEADERS+DATA)…: got {frames}");
        } else {
            assert_eq!(bytes[3], 0x04, "first S→C frame is SETTINGS");
            let frames = walk_h2_frames(bytes);
            assert!(frames >= 7, "S→C frame count: got {frames}");
        }
    }

    for f in [pcap.clone(), format!("{pcap}.1"), format!("{pcap}.2")] {
        let _ = std::fs::remove_file(f);
    }
}
