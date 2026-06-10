// SPDX-License-Identifier: MIT OR Apache-2.0
//! Plumbing shared across the grpcuds example matrix.
//!
//! - [`sock`] — a unique, pre-cleaned `/tmp` socket path per test.
//! - [`wait_for_sock`] — block until a server binds (it may bind on another
//!   thread/process).
//! - [`connect_uds`] — a tonic `Channel` over a UNIX domain socket.
//! - [`cpp`] — spawn a C++ example binary and skip (not fail) when it's absent.

use std::path::Path;
use std::time::{Duration, Instant};

/// A unique, pre-cleaned socket path for one test/demo (`tag` disambiguates
/// concurrent tests in the same process).
pub fn sock(tag: &str) -> String {
    let p = format!("/tmp/grpcuds-ex-{}-{}.sock", std::process::id(), tag);
    let _ = std::fs::remove_file(&p);
    p
}

/// Block until `path` exists. Panics after ~2s.
pub fn wait_for_sock(path: impl AsRef<Path>) {
    let path = path.as_ref();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !path.exists() {
        if Instant::now() >= deadline {
            panic!("server never bound {path:?}");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A tonic `Channel` over the UDS at `path` (the `http://unix` + UnixStream
/// connector pattern every stock-gRPC-over-UDS client uses).
pub async fn connect_uds(path: String) -> tonic::transport::Channel {
    use tonic::transport::Endpoint;
    use tower::service_fn;
    Endpoint::try_from("http://unix")
        .unwrap()
        .connect_with_connector(service_fn(move |_| {
            let path = path.clone();
            async move {
                let s = tokio::net::UnixStream::connect(path).await?;
                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(s))
            }
        }))
        .await
        .expect("tonic connect")
}

/// A background tonic server (its own current-thread runtime on a dedicated
/// thread). [`stop`](TonicServer::stop) — or drop — signals graceful shutdown
/// and joins. Each domain's `spawn_tonic` builds its `Routes` and hands them
/// to [`serve_routes`].
pub struct TonicServer {
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl TonicServer {
    /// Signal shutdown and join the server thread.
    pub fn stop(mut self) {
        self.stop_inner();
    }

    fn stop_inner(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(th) = self.thread.take() {
            let _ = th.join();
        }
    }
}

impl Drop for TonicServer {
    fn drop(&mut self) {
        self.stop_inner();
    }
}

/// Serve a tonic `Routes` on a fresh UDS at `sock` until shutdown, on a
/// dedicated runtime thread.
pub fn serve_routes(sock: String, routes: tonic::service::Routes) -> TonicServer {
    use tokio::net::UnixListener;
    use tokio_stream::wrappers::UnixListenerStream;

    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let thread = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build runtime");
        rt.block_on(async move {
            let _ = std::fs::remove_file(&sock);
            let uds = UnixListener::bind(&sock).expect("bind uds");
            tonic::transport::Server::builder()
                .add_routes(routes)
                .serve_with_incoming_shutdown(UnixListenerStream::new(uds), async {
                    let _ = rx.await;
                })
                .await
                .expect("tonic serve");
        });
    });
    TonicServer {
        shutdown: Some(tx),
        thread: Some(thread),
    }
}

/// Driving the C++ example binaries from Rust cross-language tests.
pub mod cpp {
    use std::io::{BufRead, BufReader};
    use std::path::PathBuf;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    /// Locate a C++ example binary: honor `$<env_var>`, else the conventional
    /// build path. Returns `None` when neither exists — the caller should then
    /// **skip** (not fail) the test, so the suite is green before the C++ side
    /// is built.
    pub fn locate(env_var: &str, fallback: PathBuf) -> Option<PathBuf> {
        if let Ok(p) = std::env::var(env_var) {
            let pb = PathBuf::from(p);
            return pb.exists().then_some(pb);
        }
        fallback.exists().then_some(fallback)
    }

    /// A spawned child that is killed + reaped on drop.
    pub struct Guard(pub Child);
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// Spawn a C++ **server** binary as `bin <sock>` and block until it prints
    /// a line containing `READY` on stdout (bind complete). Panics on timeout.
    pub fn spawn_server(bin: &std::path::Path, sock: &str) -> Guard {
        let mut child = Command::new(bin)
            .arg(sock)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn cpp server");
        let stdout = child.stdout.take().expect("piped stdout");
        let mut reader = BufReader::new(stdout);
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                panic!("cpp server exited before READY");
            }
            if line.contains("READY") {
                break;
            }
            if Instant::now() >= deadline {
                panic!("cpp server never printed READY");
            }
        }
        // Drain the rest of stdout so the child never blocks on a full pipe.
        std::thread::spawn(move || {
            let mut sink = String::new();
            while reader.read_line(&mut sink).unwrap_or(0) > 0 {
                sink.clear();
            }
        });
        Guard(child)
    }

    /// Run a self-checking C++ **client** binary as `bin <sock>` to completion
    /// and return whether it exited 0 (success). stdout/stderr inherit so its
    /// diagnostics show on failure.
    pub fn run_client(bin: &std::path::Path, sock: &str) -> bool {
        Command::new(bin)
            .arg(sock)
            .status()
            .expect("run cpp client")
            .success()
    }
}
