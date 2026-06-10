// SPDX-License-Identifier: MIT OR Apache-2.0
#![warn(missing_docs)]
//! Wire-compatible gRPC over UNIX domain sockets — a tiny server and a
//! matching client that speak real gRPC-over-HTTP/2 to stock gRPC peers.
//!
//! ## Features
//!
//! - **`server`** (default) — the [`Server`] / [`ServerBuilder`] / [`Running`]
//!   API. Enabled by default, so `grpcuds` with no feature flags is a
//!   server-only build, unchanged.
//! - **`client`** — a blocking [`Client`] that dials a grpcuds (or any stock
//!   gRPC) server over UDS: [`Client::unary`] and
//!   [`Client::server_streaming`].
//! - **`prost`** — typed handlers/calls over prost messages.
//! - **`tokio`** — [`Server::serve_async`]: park the single-threaded I/O
//!   loop on tokio's blocking pool with future-driven shutdown. This is
//!   *coexistence* with a tokio app, **not** an async-native server — there
//!   are no `async fn` handlers and the UDS fd never joins tokio's reactor
//!   (see [Caveats](#caveats)).
//! - **`bundled`** — statically link libnghttp2 from the pinned submodule.
//!
//! These are **byte-level** handlers and calls — the smallest surface, no
//! codegen, no `prost`. For typed prost handlers and a generated typed
//! client stub, add a `build.rs` with the
//! [`grpcuds-build`](https://crates.io/crates/grpcuds-build) crate; the
//! `*_msg` methods ([`ServerBuilder::add_unary_msg`], [`Client::unary_msg`])
//! are the prost-typed equivalents the generated code calls.
//!
//! ```no_run
//! # #[cfg(all(feature = "server", feature = "client"))]
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use grpcuds::{Client, Server};
//!
//! // Server: one unary method that echoes the request bytes.
//! let running = Server::builder()
//!     .bind("/tmp/echo.sock")
//!     .add_unary("/echo.Echo/Unary", |req: &[u8]| Ok(req.to_vec()))
//!     .build()?
//!     .run()?;
//!
//! // Client (the `client` feature): dial the same socket and call it.
//! let mut client = Client::connect("/tmp/echo.sock")?;
//! let reply = client.unary("/echo.Echo/Unary", b"ping")?;
//! assert_eq!(reply, b"ping");
//!
//! # running.join()?;
//! # Ok(())
//! # }
//! # #[cfg(not(all(feature = "server", feature = "client")))]
//! # fn main() {}
//! ```
//!
//! ## Caveats
//!
//! - **Not async-native.** The core is a single-threaded poll loop; the
//!   `tokio` feature parks it on a blocking-pool thread (coexistence, not
//!   `async fn` handlers).
//! - **Handlers must not block.** They run on the I/O thread and return
//!   immediately — streaming is producer-push (`writer.write(...)` from
//!   another context) and long-running unary work defers the same way.
//! - **UDS only, no payload security.** Local IPC between cooperating
//!   processes; access control is the socket's filesystem permissions.
//! - **Sized for local IPC.** One thread serializes all connection I/O —
//!   a handful of same-device peers, not fan-out or throughput contests.
//!
//! See the [project README](https://github.com/sy39ju/grpc-uds) for
//! the full feature table and the design rationale.

use std::fmt;
use std::io;
use std::path::PathBuf;

use grpcuds_core::GrpcStatus;

/// gRPC status code. Re-exported from the core so the numeric values stay
/// identical to what stock gRPC clients/servers expect on the wire.
pub use grpcuds_core::GrpcStatus as StatusCode;

/// `include!` the module that `grpcuds-build` generated for a proto
/// package, by package name:
///
/// ```ignore
/// pub mod pb { grpcuds::include_proto!("echo"); }
/// ```
///
/// The generated messages derive `::prost::Message`, so the consuming crate
/// must depend on `prost` directly and enable this crate's `prost` feature.
/// See the `grpcuds-build` crate docs for the full setup.
#[macro_export]
macro_rules! include_proto {
    ($package:tt) => {
        include!(concat!(env!("OUT_DIR"), concat!("/", $package, ".rs")));
    };
}

/// Transport-level errors from building or running a [`Server`], or from
/// connecting a [`Client`] (the `Connect` / `Session` variants). A non-OK
/// gRPC *status* on a call is a [`Status`], not an `Error`.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// [`ServerBuilder::build`] was called without a [`bind`](ServerBuilder::bind).
    MissingBindPath,
    /// The bind path is empty or longer than a UDS `sun_path` allows.
    InvalidPath(PathBuf),
    /// Binding the listening socket failed (permissions, missing directory,
    /// address in use, …).
    Bind {
        /// The path that failed to bind.
        path: PathBuf,
        /// The underlying OS error.
        source: io::Error,
    },
    /// An I/O error from the running server (poll loop, eventfd, thread spawn).
    Io(io::Error),
    /// [`Client::connect`](crate::Client::connect) failed to reach the socket.
    Connect {
        /// The path that could not be connected.
        path: PathBuf,
        /// The underlying OS error.
        source: io::Error,
    },
    /// nghttp2 session setup failed on the client.
    Session,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::MissingBindPath => write!(f, "no bind() path was set on the builder"),
            Error::InvalidPath(p) => {
                write!(f, "invalid UDS path {:?} (empty or too long)", p)
            }
            Error::Bind { path, source } => write!(f, "binding {:?} failed: {source}", path),
            Error::Io(e) => write!(f, "server I/O error: {e}"),
            Error::Connect { path, source } => {
                write!(f, "connecting to {:?} failed: {source}", path)
            }
            Error::Session => write!(f, "nghttp2 client session setup failed"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Bind { source, .. } => Some(source),
            Error::Io(e) => Some(e),
            Error::Connect { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

/// The call can no longer accept this operation: it was already finished, or
/// the client is gone (RST_STREAM / connection drop). A producer loop should
/// stop when it sees this. Inspect [`ServerWriter::is_cancelled`] /
/// [`ServerWriter::is_finished`] to tell the two apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Closed;

impl fmt::Display for Closed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("call is finished or the client is gone")
    }
}

impl std::error::Error for Closed {}

/// A gRPC status: a [`StatusCode`] plus an optional message that ships as the
/// percent-encoded `grpc-message` trailer next to the numeric `grpc-status`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    code: StatusCode,
    message: Option<String>,
}

impl Status {
    /// `OK` (code 0), no message.
    pub fn ok() -> Self {
        Self {
            code: GrpcStatus::Ok,
            message: None,
        }
    }

    /// A non-OK status with a human-readable message (becomes the
    /// `grpc-message` trailer).
    pub fn new(code: StatusCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: Some(message.into()),
        }
    }

    /// A status with just a code and no message.
    pub fn code_only(code: StatusCode) -> Self {
        Self {
            code,
            message: None,
        }
    }

    /// The numeric gRPC status code.
    pub fn code(&self) -> StatusCode {
        self.code
    }

    /// The `grpc-message` payload, if any.
    pub fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }

    #[cfg_attr(not(feature = "server"), allow(dead_code))]
    fn is_ok(&self) -> bool {
        matches!(self.code, GrpcStatus::Ok)
    }

    /// Build a status from a received numeric code + optional message (client
    /// side). Out-of-range codes collapse to `Unknown`.
    #[cfg(feature = "client")]
    pub(crate) fn from_wire(code: i32, message: Option<String>) -> Self {
        Self {
            code: code_from_wire(code),
            message,
        }
    }
}

/// Map an on-the-wire `grpc-status` value to a [`StatusCode`].
#[cfg(feature = "client")]
pub(crate) fn code_from_wire(code: i32) -> StatusCode {
    use GrpcStatus::*;
    match code {
        0 => Ok,
        1 => Cancelled,
        2 => Unknown,
        3 => InvalidArgument,
        4 => DeadlineExceeded,
        5 => NotFound,
        6 => AlreadyExists,
        7 => PermissionDenied,
        8 => ResourceExhausted,
        9 => FailedPrecondition,
        10 => Aborted,
        11 => OutOfRange,
        12 => Unimplemented,
        13 => Internal,
        14 => Unavailable,
        15 => DataLoss,
        16 => Unauthenticated,
        _ => Unknown,
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "grpc status {:?}", self.code)?;
        if let Some(m) = &self.message {
            write!(f, ": {m}")?;
        }
        Ok(())
    }
}

impl std::error::Error for Status {}

/// Canonical per-code constructors, one per status code:
/// `Status::invalid_argument("scan_mode is required")`.
macro_rules! status_ctor {
    ($($(#[$doc:meta])* $name:ident => $code:ident),+ $(,)?) => {
        impl Status {
            $($(#[$doc])*
            pub fn $name(message: impl Into<String>) -> Self {
                Self::new(StatusCode::$code, message)
            })+
        }
    };
}

status_ctor! {
    /// `CANCELLED` (1) with a message.
    cancelled => Cancelled,
    /// `UNKNOWN` (2) with a message.
    unknown => Unknown,
    /// `INVALID_ARGUMENT` (3) with a message.
    invalid_argument => InvalidArgument,
    /// `DEADLINE_EXCEEDED` (4) with a message.
    deadline_exceeded => DeadlineExceeded,
    /// `NOT_FOUND` (5) with a message.
    not_found => NotFound,
    /// `ALREADY_EXISTS` (6) with a message.
    already_exists => AlreadyExists,
    /// `PERMISSION_DENIED` (7) with a message.
    permission_denied => PermissionDenied,
    /// `RESOURCE_EXHAUSTED` (8) with a message.
    resource_exhausted => ResourceExhausted,
    /// `FAILED_PRECONDITION` (9) with a message.
    failed_precondition => FailedPrecondition,
    /// `ABORTED` (10) with a message.
    aborted => Aborted,
    /// `OUT_OF_RANGE` (11) with a message.
    out_of_range => OutOfRange,
    /// `UNIMPLEMENTED` (12) with a message.
    unimplemented => Unimplemented,
    /// `INTERNAL` (13) with a message.
    internal => Internal,
    /// `UNAVAILABLE` (14) with a message.
    unavailable => Unavailable,
    /// `DATA_LOSS` (15) with a message.
    data_loss => DataLoss,
    /// `UNAUTHENTICATED` (16) with a message.
    unauthenticated => Unauthenticated,
}

// Logging is half-independent (grpcuds-core::logging is always compiled).
mod logging;
pub use logging::{set_logger, LogLevel};

#[cfg(feature = "server")]
mod server;
#[cfg(feature = "server")]
pub use server::*;

#[cfg(feature = "health")]
pub mod health;

#[cfg(feature = "client")]
mod client;
#[cfg(feature = "client")]
pub use client::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_basic_constructors() {
        let ok = Status::ok();
        assert_eq!(ok.code(), StatusCode::Ok);
        assert_eq!(ok.message(), None);

        let s = Status::new(StatusCode::Internal, "boom");
        assert_eq!(s.code(), StatusCode::Internal);
        assert_eq!(s.message(), Some("boom"));

        let c = Status::code_only(StatusCode::Aborted);
        assert_eq!(c.code(), StatusCode::Aborted);
        assert_eq!(c.message(), None);
    }

    /// The status_ctor! table maps every canonical name to its code — a
    /// transposed pair would compile fine, so pin each one.
    #[test]
    fn canonical_constructors_map_to_their_codes() {
        let cases: [(Status, StatusCode); 15] = [
            (Status::cancelled("m"), StatusCode::Cancelled),
            (Status::unknown("m"), StatusCode::Unknown),
            (Status::invalid_argument("m"), StatusCode::InvalidArgument),
            (Status::deadline_exceeded("m"), StatusCode::DeadlineExceeded),
            (Status::not_found("m"), StatusCode::NotFound),
            (Status::already_exists("m"), StatusCode::AlreadyExists),
            (Status::permission_denied("m"), StatusCode::PermissionDenied),
            (
                Status::resource_exhausted("m"),
                StatusCode::ResourceExhausted,
            ),
            (
                Status::failed_precondition("m"),
                StatusCode::FailedPrecondition,
            ),
            (Status::aborted("m"), StatusCode::Aborted),
            (Status::out_of_range("m"), StatusCode::OutOfRange),
            (Status::unimplemented("m"), StatusCode::Unimplemented),
            (Status::internal("m"), StatusCode::Internal),
            (Status::unavailable("m"), StatusCode::Unavailable),
            (Status::unauthenticated("m"), StatusCode::Unauthenticated),
        ];
        for (status, code) in cases {
            assert_eq!(status.code(), code);
            assert_eq!(status.message(), Some("m"));
        }
    }

    #[test]
    fn status_display_includes_code_and_message() {
        assert_eq!(
            Status::not_found("no such device").to_string(),
            "grpc status NotFound: no such device"
        );
        assert_eq!(Status::ok().to_string(), "grpc status Ok");
    }

    #[cfg(feature = "client")]
    #[test]
    fn from_wire_collapses_out_of_range_codes_to_unknown() {
        assert_eq!(Status::from_wire(5, None).code(), StatusCode::NotFound);
        assert_eq!(Status::from_wire(0, None).code(), StatusCode::Ok);
        assert_eq!(Status::from_wire(99, None).code(), StatusCode::Unknown);
        assert_eq!(Status::from_wire(-3, None).code(), StatusCode::Unknown);
        let s = Status::from_wire(3, Some("bad".into()));
        assert_eq!(s.message(), Some("bad"));
    }

    #[test]
    fn error_display_and_source_chain() {
        assert_eq!(
            Error::MissingBindPath.to_string(),
            "no bind() path was set on the builder"
        );
        assert!(Error::InvalidPath(PathBuf::from("/x"))
            .to_string()
            .contains("invalid UDS path"));

        let io = io::Error::from_raw_os_error(13); // EACCES
        let bind = Error::Bind {
            path: PathBuf::from("/run/x.sock"),
            source: io,
        };
        assert!(bind.to_string().contains("/run/x.sock"));
        assert!(std::error::Error::source(&bind).is_some());
        assert!(std::error::Error::source(&Error::MissingBindPath).is_none());
        assert!(std::error::Error::source(&Error::Session).is_none());

        // From<io::Error> lands in the Io variant (with the source wired).
        let e: Error = io::Error::from_raw_os_error(32).into();
        assert!(matches!(e, Error::Io(_)));
        assert!(std::error::Error::source(&e).is_some());
    }

    #[test]
    fn closed_is_a_real_error_type() {
        assert_eq!(Closed.to_string(), "call is finished or the client is gone");
        let _: &dyn std::error::Error = &Closed;
        assert_eq!(Closed, Closed);
    }
}
