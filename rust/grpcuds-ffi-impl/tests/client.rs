// SPDX-License-Identifier: MIT OR Apache-2.0
//! The client C ABI symbols (called as Rust fns) against a real grpcuds
//! server. Requires the `client` feature.
#![cfg(feature = "client")]

use std::ffi::CString;
use std::time::Duration;

use grpcuds::{Server, Status, StatusCode};
use grpcuds_ffi_impl::*;

static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
fn path() -> String {
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    format!("/tmp/grpcuds-cabi-client-{}-{}.sock", std::process::id(), n)
}

fn echo_server(sock: &str) -> grpcuds::Running {
    Server::builder()
        .bind(sock)
        .add_unary("/echo.Echo/Unary", |req: &[u8]| Ok(req.to_vec()))
        .add_unary("/echo.Echo/Bad", |_req: &[u8]| {
            Err(Status::invalid_argument("nope"))
        })
        .add_server_streaming("/echo.Echo/Stream", |req: &[u8], w| {
            for i in 0u8..3 {
                let mut m = req.to_vec();
                m.push(i);
                let _ = w.write(&m);
            }
            let _ = w.finish(Status::ok());
            Status::ok()
        })
        .build()
        .unwrap()
        .run()
        .unwrap()
}

unsafe fn connect(sock: &str) -> *mut grpcuds_client {
    let c = CString::new(sock).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        let p = grpcuds_client_connect(c.as_ptr());
        if !p.is_null() {
            return p;
        }
        assert!(std::time::Instant::now() < deadline, "connect timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn c_abi_client_unary() {
    let sock = path();
    let _srv = echo_server(&sock);
    unsafe {
        let client = connect(&sock);
        let p = CString::new("/echo.Echo/Unary").unwrap();
        let resp = grpcuds_client_unary(client, p.as_ptr(), b"hi".as_ptr(), 2);
        assert!(!resp.is_null());
        assert_eq!(grpcuds_response_status(resp), 0);
        let mut len = 0usize;
        let body = grpcuds_response_body(resp, &mut len);
        assert_eq!(std::slice::from_raw_parts(body, len), b"hi");
        grpcuds_response_free(resp);

        // error status + message
        let bad = CString::new("/echo.Echo/Bad").unwrap();
        let resp = grpcuds_client_unary(client, bad.as_ptr(), b"x".as_ptr(), 1);
        assert!(!resp.is_null());
        assert_eq!(
            grpcuds_response_status(resp),
            StatusCode::InvalidArgument as i32
        );
        let mut mlen = 0usize;
        let m = grpcuds_response_message_bytes(resp, &mut mlen);
        assert_eq!(std::slice::from_raw_parts(m, mlen), b"nope");
        grpcuds_response_free(resp);

        grpcuds_client_free(client);
    }
}

#[test]
fn c_abi_client_streaming() {
    let sock = path();
    let _srv = echo_server(&sock);
    unsafe {
        let client = connect(&sock);
        let p = CString::new("/echo.Echo/Stream").unwrap();
        let stream = grpcuds_client_server_streaming(client, p.as_ptr(), b"m".as_ptr(), 1);
        assert!(!stream.is_null());
        let mut got: Vec<Vec<u8>> = Vec::new();
        loop {
            let mut len = 0usize;
            let msg = grpcuds_stream_next(stream, &mut len);
            if msg.is_null() {
                break;
            }
            got.push(std::slice::from_raw_parts(msg, len).to_vec());
        }
        assert_eq!(grpcuds_stream_status(stream), 0);
        assert_eq!(got, vec![vec![b'm', 0], vec![b'm', 1], vec![b'm', 2]]);
        grpcuds_stream_free(stream);
        grpcuds_client_free(client);
    }
}
