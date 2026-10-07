//! Connections from the broker to a route's upstream.
//!
//! The broker connects only to the addresses the decision point already
//! resolved and classified for the connection, never resolving the name
//! again, and refuses any that is this computer
//! ([`axocoatl_tools::fetch_guard::is_local_destination`]). TLS sends the
//! route host as the server name and offers only `http/1.1`. The upstream's
//! certificate is checked with this computer's own trust settings
//! (`rustls-platform-verifier`), plus the route's `upstream_ca` when set.
//!
//! The one exception is the explicit `sandbox.egress.host_ollama` route:
//! plain HTTP to `127.0.0.1:<port>` on this computer, its configured port
//! and nothing else ([`UpstreamConnector::connect_host_loopback`]).

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use hyper::client::conn::http1::SendRequest;
use hyper_util::rt::TokioIo;
use rustls::client::danger::ServerCertVerifier;
use rustls::crypto::CryptoProvider;
use rustls::pki_types::ServerName;
use rustls::ClientConfig;
use tokio::net::TcpStream;

use super::rules::Route;
use super::BoxError;

/// The body type the broker sends upstream.
pub type UpstreamBody = BoxBody<Bytes, BoxError>;

/// Whether an address is this computer and must not be connected to.
pub type LocalCheck = Arc<dyn Fn(IpAddr) -> bool + Send + Sync>;

/// How long one TCP connect and one TLS handshake may take.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Why the upstream could not be reached.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct UpstreamError(pub String);

enum Verification {
    /// This computer's trust settings, plus a route's `upstream_ca`.
    Platform,
    /// One fixed verifier (tests).
    #[cfg_attr(not(test), allow(dead_code))]
    Fixed(Arc<dyn ServerCertVerifier>),
}

/// Opens TLS connections to route upstreams. One per daemon or Session.
pub struct UpstreamConnector {
    provider: Arc<CryptoProvider>,
    verification: Verification,
    local: LocalCheck,
    connect_timeout: Duration,
    configs: Mutex<HashMap<Option<[u8; 32]>, Arc<ClientConfig>>>,
}

impl std::fmt::Debug for UpstreamConnector {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UpstreamConnector")
            .field("connect_timeout", &self.connect_timeout)
            .finish_non_exhaustive()
    }
}

impl Default for UpstreamConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl UpstreamConnector {
    /// The production connector: platform trust, and this computer's own
    /// addresses refused.
    pub fn new() -> Self {
        Self::with_local_check(Arc::new(axocoatl_tools::fetch_guard::is_local_destination))
    }

    /// Platform trust with another own-address check. Tests use it to reach
    /// a local upstream through the real platform verifier.
    pub fn with_local_check(local: LocalCheck) -> Self {
        Self {
            provider: super::crypto_provider(),
            verification: Verification::Platform,
            local,
            connect_timeout: CONNECT_TIMEOUT,
            configs: Mutex::new(HashMap::new()),
        }
    }

    /// A connector that checks upstreams with `verifier` alone (tests).
    #[cfg(test)]
    pub(crate) fn with_verifier(verifier: Arc<dyn ServerCertVerifier>, local: LocalCheck) -> Self {
        Self {
            verification: Verification::Fixed(verifier),
            ..Self::with_local_check(local)
        }
    }

    fn client_config(&self, route: &Route) -> Result<Arc<ClientConfig>, UpstreamError> {
        let mut configs = self
            .configs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let key = match self.verification {
            Verification::Platform => route.upstream_roots_id,
            Verification::Fixed(_) => None,
        };
        if let Some(config) = configs.get(&key) {
            return Ok(config.clone());
        }
        let verifier: Arc<dyn ServerCertVerifier> = match &self.verification {
            Verification::Fixed(verifier) => verifier.clone(),
            Verification::Platform if route.upstream_roots.is_empty() => Arc::new(
                rustls_platform_verifier::Verifier::new(self.provider.clone()).map_err(
                    |error| UpstreamError(format!("this computer's trust settings: {error}")),
                )?,
            ),
            Verification::Platform => Arc::new(
                rustls_platform_verifier::Verifier::new_with_extra_roots(
                    route.upstream_roots.iter().cloned(),
                    self.provider.clone(),
                )
                .map_err(|error| UpstreamError(format!("the route's upstream_ca: {error}")))?,
            ),
        };
        let mut config = ClientConfig::builder_with_provider(self.provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
            .map_err(|error| UpstreamError(format!("TLS settings: {error}")))?
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let config = Arc::new(config);
        configs.insert(key, config.clone());
        Ok(config)
    }

    /// Open a plain HTTP/1.1 connection to `127.0.0.1:port` for a
    /// `host_ollama` route. Only the route's own configured port is passed
    /// here; the own-address check that refuses this computer for every
    /// other route does not apply to it, by the person's explicit setting.
    pub async fn connect_host_loopback(
        &self,
        port: u16,
    ) -> Result<SendRequest<UpstreamBody>, UpstreamError> {
        let target = SocketAddr::from(([127, 0, 0, 1], port));
        let tcp = match tokio::time::timeout(self.connect_timeout, TcpStream::connect(target)).await
        {
            Ok(Ok(tcp)) => tcp,
            Ok(Err(error)) => {
                return Err(UpstreamError(format!(
                    "Ollama on this computer ({target}) could not be reached: {error}"
                )))
            }
            Err(_) => {
                return Err(UpstreamError(format!(
                    "Ollama on this computer ({target}) could not be reached: connect timed out"
                )))
            }
        };
        let _ = tcp.set_nodelay(true);
        let (sender, connection) =
            hyper::client::conn::http1::handshake::<_, UpstreamBody>(TokioIo::new(tcp))
                .await
                .map_err(|error| UpstreamError(format!("{target}: HTTP: {error}")))?;
        tokio::spawn(async move {
            let _ = connection.await;
        });
        Ok(sender)
    }

    /// Open an HTTP/1.1 connection over TLS to `route` at the first of
    /// `addrs` that answers on `port`.
    pub async fn connect(
        &self,
        route: &Route,
        addrs: &[IpAddr],
        port: u16,
    ) -> Result<SendRequest<UpstreamBody>, UpstreamError> {
        let config = self.client_config(route)?;
        let server_name = ServerName::try_from(route.host.clone())
            .map_err(|error| UpstreamError(format!("{}: {error}", route.host)))?;
        if addrs.is_empty() {
            return Err(UpstreamError(format!(
                "no address was resolved for {}",
                route.host
            )));
        }
        let mut failures = Vec::new();
        for address in addrs {
            if (self.local)(*address) {
                failures.push(format!("{address} is this computer"));
                continue;
            }
            let target = SocketAddr::new(*address, port);
            let tcp = match tokio::time::timeout(self.connect_timeout, TcpStream::connect(target))
                .await
            {
                Ok(Ok(tcp)) => tcp,
                Ok(Err(error)) => {
                    failures.push(format!("{target}: {error}"));
                    continue;
                }
                Err(_) => {
                    failures.push(format!("{target}: connect timed out"));
                    continue;
                }
            };
            let _ = tcp.set_nodelay(true);
            let connector = tokio_rustls::TlsConnector::from(config.clone());
            let tls = match tokio::time::timeout(
                self.connect_timeout,
                connector.connect(server_name.clone(), tcp),
            )
            .await
            {
                Ok(Ok(tls)) => tls,
                Ok(Err(error)) => {
                    failures.push(format!("{target}: TLS: {error}"));
                    continue;
                }
                Err(_) => {
                    failures.push(format!("{target}: TLS handshake timed out"));
                    continue;
                }
            };
            if let Some(protocol) = tls.get_ref().1.alpn_protocol() {
                if protocol != b"http/1.1" {
                    failures.push(format!(
                        "{target}: the upstream chose a protocol other than HTTP/1.1"
                    ));
                    continue;
                }
            }
            let (sender, connection) =
                hyper::client::conn::http1::handshake::<_, UpstreamBody>(TokioIo::new(tls))
                    .await
                    .map_err(|error| UpstreamError(format!("{target}: HTTP: {error}")))?;
            tokio::spawn(async move {
                let _ = connection.await;
            });
            return Ok(sender);
        }
        Err(UpstreamError(format!(
            "{} could not be reached: {}",
            route.host,
            failures.join("; ")
        )))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn loopback_unspecified_and_own_addresses_are_local() {
        use axocoatl_tools::fetch_guard::{interface_addresses, is_local_destination};
        for address in ["127.0.0.1", "::1", "0.0.0.0", "::", "::ffff:127.0.0.1"] {
            assert!(is_local_destination(address.parse().unwrap()), "{address}");
        }
        for interface in interface_addresses().unwrap() {
            assert!(
                is_local_destination(interface.address),
                "{}",
                interface.address
            );
        }
        // Documentation addresses are never this computer's.
        assert!(!is_local_destination("192.0.2.10".parse().unwrap()));
        assert!(!is_local_destination("2001:db8::10".parse().unwrap()));
    }
}
