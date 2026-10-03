//! The egress route broker: TLS ended in the daemon for hosts listed under
//! `sandbox.egress.routes`, request rules, and credentials added on this
//! computer.
//!
//! For a route host the decision point answers the sidecar's `CONNECT` with
//! a relay instead of a tunnel, and the sidecar carries the client's TLS
//! bytes to the daemon. [`terminate::SessionBroker::serve`] takes that byte
//! stream (any `AsyncRead + AsyncWrite`):
//!
//! 1. It accepts TLS with a certificate for the route host from the
//!    Session's own authority ([`ca::SessionCa`]), only when the client's
//!    server name is that host, and only for `http/1.1`.
//! 2. It reads HTTP/1.1 requests from a process kind the route serves. Each
//!    must name the route host in `Host`, have a canonical path, ask for no
//!    upgrade, carry no header that names another method, path or host (such
//!    as `X-HTTP-Method-Override` or `X-Forwarded-Host`), and match one of
//!    the route's rules ([`rules`]); anything else gets a JSON refusal saying
//!    why.
//! 3. It writes a `request` event to the Session's network record and waits
//!    for it before anything goes upstream; a request that cannot be
//!    recorded is refused.
//! 4. On a credentialed route it reads the credential now (from the
//!    daemon's environment or an owner-only file), removes the client's own
//!    `Authorization`, `Proxy-Authorization` and configured header, and adds
//!    the credential. The value never enters a container, the sidecar, the
//!    record or the logs.
//! 5. It sends the request upstream ([`upstream`]) to an address the
//!    decision point already resolved, over TLS checked with this computer's
//!    trust settings, and streams the response back, refusing compressed
//!    responses on credentialed routes and stopping any response whose
//!    status line, headers, body or trailers carry the credential ([`scan`]).
//!    A `response` event records how it ended.
//!
//! [`trust`] has the files and environment that make containers trust the
//! Session's authority. Wiring the broker to the relay is done where the
//! decision point lives.

use std::sync::Arc;

use async_trait::async_trait;
use axocoatl_session::network_record::NetworkEvent;
use rustls::crypto::CryptoProvider;

pub mod ca;
pub mod rules;
pub mod scan;
pub mod terminate;
pub mod trust;
pub mod upstream;
mod x509;

pub use ca::SessionCa;
pub use rules::{CredentialSource, Route, RouteTable, RuleDecision};
pub use terminate::{serve, BrokerOutcome, RelayContext, SessionBroker, WorkspaceRoots};
pub use trust::TrustMaterial;
pub use upstream::UpstreamConnector;

/// Errors carried through bodies.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// A broker setup failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BrokerError {
    #[error("certificate: {0}")]
    Certificate(String),
}

/// Where the broker writes `request` and `response` events: the Session's
/// network record.
#[async_trait]
pub trait BrokerRecordSink: Send + Sync {
    /// Append one event. Returns once the event is in the record, or an
    /// error when it could not be written.
    async fn append(&self, event: NetworkEvent) -> Result<(), String>;
}

/// The TLS implementation the broker uses for both sides.
pub(crate) fn crypto_provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}
