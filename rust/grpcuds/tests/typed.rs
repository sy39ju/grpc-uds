// SPDX-License-Identifier: MIT OR Apache-2.0
//! Typed (prost) handler layer, end to end over the real wire:
//! decode-in/encode-out unary, MessageWriter streaming from another thread,
//! and the INTERNAL answer for an undecodable request.
//!
//! Runs only with `--features prost` (see `[[test]]` in Cargo.toml).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use grpcuds::{MessageWriter, Server, ServerBuilder, Status, StatusCode};
use grpcuds_core::{decode_header, FRAME_HEADER_LEN};
use prost::Message;

mod common;
use common::{call, unique_path};

// Hand-derived messages — no protoc needed for the test.
#[derive(Clone, PartialEq, Message)]
struct EchoRequest {
    #[prost(string, tag = "1")]
    text: String,
}

#[derive(Clone, PartialEq, Message)]
struct EchoReply {
    #[prost(string, tag = "1")]
    text: String,
    #[prost(uint32, tag = "2")]
    len: u32,
}

struct ServerHarness {
    sock: String,
    shutdown: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl ServerHarness {
    fn start(build: impl FnOnce(ServerBuilder) -> ServerBuilder) -> Self {
        let sock = unique_path();
        let server = build(Server::builder().bind(&sock)).build().expect("build");
        let shutdown = Arc::new(AtomicBool::new(false));
        let sd = shutdown.clone();
        let handle = thread::spawn(move || {
            server.serve(&sd).expect("serve");
        });
        ServerHarness {
            sock,
            shutdown,
            handle: Some(handle),
        }
    }
}

impl Drop for ServerHarness {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn messages(mut data: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    while !data.is_empty() {
        let h = decode_header(data, 4 * 1024 * 1024)
            .ok()
            .expect("decode_header");
        let pl = h.payload_len as usize;
        out.push(data[FRAME_HEADER_LEN..FRAME_HEADER_LEN + pl].to_vec());
        data = &data[FRAME_HEADER_LEN + pl..];
    }
    out
}

#[test]
fn typed_unary_decodes_and_encodes() {
    let srv = ServerHarness::start(|b| {
        b.add_unary_msg("/echo.Echo/Upper", |req: EchoRequest| {
            if req.text.is_empty() {
                return Err(Status::new(StatusCode::InvalidArgument, "empty text"));
            }
            let len = req.text.len() as u32;
            Ok(EchoReply {
                text: req.text.to_uppercase(),
                len,
            })
        })
    });

    let req = EchoRequest {
        text: "hello".into(),
    }
    .encode_to_vec();
    let st = call(&srv.sock, b"/echo.Echo/Upper", &req);
    assert_eq!(st.grpc_status.as_deref(), Some(&b"0"[..]));

    let msgs = messages(&st.data);
    assert_eq!(msgs.len(), 1, "unary must produce exactly one message");
    let reply = EchoReply::decode(&msgs[0][..]).expect("decode reply");
    assert_eq!(
        reply,
        EchoReply {
            text: "HELLO".into(),
            len: 5
        }
    );

    // The typed Err path ships code + grpc-message like the raw one.
    let empty = EchoRequest {
        text: String::new(),
    }
    .encode_to_vec();
    let st = call(&srv.sock, b"/echo.Echo/Upper", &empty);
    assert_eq!(st.grpc_status.as_deref(), Some(&b"3"[..]));
    assert_eq!(st.grpc_message.as_deref(), Some(&b"empty text"[..]));
}

#[test]
fn typed_streaming_from_producer_thread() {
    let srv = ServerHarness::start(|b| {
        b.add_server_streaming_msg(
            "/echo.Echo/Count",
            |req: EchoRequest, w: &MessageWriter<EchoReply>| {
                let w = w.clone();
                thread::spawn(move || {
                    for i in 0..3u32 {
                        let reply = EchoReply {
                            text: req.text.clone(),
                            len: i,
                        };
                        if w.send(&reply).is_err() {
                            return;
                        }
                    }
                    let _ = w.finish(Status::ok());
                });
                Status::ok()
            },
        )
    });

    let req = EchoRequest {
        text: "tick".into(),
    }
    .encode_to_vec();
    let st = call(&srv.sock, b"/echo.Echo/Count", &req);
    assert_eq!(st.grpc_status.as_deref(), Some(&b"0"[..]));

    let msgs = messages(&st.data);
    assert_eq!(msgs.len(), 3);
    for (i, m) in msgs.iter().enumerate() {
        let reply = EchoReply::decode(&m[..]).expect("decode reply");
        assert_eq!(
            reply,
            EchoReply {
                text: "tick".into(),
                len: i as u32
            }
        );
    }
}

#[test]
fn undecodable_request_answers_internal() {
    let handler_ran = Arc::new(AtomicBool::new(false));
    let ran = handler_ran.clone();
    let srv = ServerHarness::start(move |b| {
        b.add_unary_msg("/echo.Echo/Strict", move |req: EchoRequest| {
            ran.store(true, Ordering::Relaxed);
            Ok(EchoReply {
                text: req.text,
                len: 0,
            })
        })
    });

    // 0xFF opens a field header whose tag/wire-type is truncated garbage.
    let st = call(&srv.sock, b"/echo.Echo/Strict", &[0xFF]);
    assert_eq!(
        st.grpc_status.as_deref(),
        Some(&b"13"[..]),
        "decode failure must answer INTERNAL (13)"
    );
    let msg = st.grpc_message.expect("grpc-message present");
    assert!(
        msg.starts_with(b"failed to decode request"),
        "unexpected grpc-message: {:?}",
        String::from_utf8_lossy(&msg)
    );
    assert!(
        !handler_ran.load(Ordering::Relaxed),
        "handler must not run on decode failure"
    );
}
