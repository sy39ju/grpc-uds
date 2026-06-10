// SPDX-License-Identifier: MIT OR Apache-2.0
//! Typed (prost) client against a typed server, in-process. Requires
//! `server`, `client`, and `prost`.

use std::time::Duration;

use grpcuds::{Client, MessageWriter, Server, Status};
use prost::Message;

mod common;
use common::unique_path;

#[derive(Clone, PartialEq, Message)]
struct Req {
    #[prost(string, tag = "1")]
    text: String,
}
#[derive(Clone, PartialEq, Message)]
struct Reply {
    #[prost(string, tag = "1")]
    text: String,
    #[prost(uint32, tag = "2")]
    n: u32,
}

fn server(sock: &str) -> grpcuds::Running {
    Server::builder()
        .bind(sock)
        .add_unary_msg("/t.T/Up", |r: Req| {
            Ok(Reply {
                text: r.text.to_uppercase(),
                n: 0,
            })
        })
        .add_server_streaming_msg("/t.T/Rep", |r: Req, w: &MessageWriter<Reply>| {
            for i in 0..3u32 {
                if w.send(&Reply {
                    text: r.text.clone(),
                    n: i,
                })
                .is_err()
                {
                    return Status::code_only(grpcuds::StatusCode::Aborted);
                }
            }
            let _ = w.finish(Status::ok());
            Status::ok()
        })
        .build()
        .expect("build")
        .run()
        .expect("run")
}

fn connect(sock: &str) -> Client {
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        match Client::connect(sock) {
            Ok(c) => return c,
            Err(_) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10))
            }
            Err(e) => panic!("connect: {e}"),
        }
    }
}

#[test]
fn typed_unary_and_streaming() {
    let sock = unique_path();
    let _srv = server(&sock);
    let mut client = connect(&sock);

    let reply: Reply = client
        .unary_msg("/t.T/Up", &Req { text: "hi".into() })
        .expect("unary_msg");
    assert_eq!(
        reply,
        Reply {
            text: "HI".into(),
            n: 0
        }
    );

    let mut stream = client
        .server_streaming_msg::<_, Reply>("/t.T/Rep", &Req { text: "x".into() })
        .expect("stream");
    let mut got = Vec::new();
    while let Some(m) = stream.message().expect("msg") {
        got.push(m);
    }
    assert_eq!(
        got,
        vec![
            Reply {
                text: "x".into(),
                n: 0
            },
            Reply {
                text: "x".into(),
                n: 1
            },
            Reply {
                text: "x".into(),
                n: 2
            },
        ]
    );
}
