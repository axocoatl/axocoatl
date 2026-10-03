//! Outbound HTTP for `web_fetch`, which refuses private and local
//! destinations.
//!
//! Every request goes out from this process, never through a proxy from the
//! environment. A URL host that is an IP literal is classified before any
//! request. A name is resolved by [`GuardResolver`], which fails the
//! connection if **any** address it returns is not public, so the addresses
//! checked are exactly the ones the connection may use. Redirects are followed
//! by hand, at most [`MAX_REDIRECTS`], and every hop is checked again for
//! scheme, user information, host and resolution.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use axocoatl_core::netaddr::{self, AddrClass};

use crate::limits::limit_text;

/// Most redirects followed for one fetch.
pub const MAX_REDIRECTS: usize = 5;
/// Longest URL accepted, in characters.
pub const MAX_URL_CHARS: usize = 8192;
/// Time allowed to open one TCP connection.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Time allowed to resolve one name.
pub const RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);
/// What `web_fetch` asks for.
pub const ACCEPT: &str = "text/html, application/xhtml+xml, text/plain, text/markdown, \
                          application/json, application/xml, text/xml;q=0.9";
/// Content types `web_fetch` reads. Anything else is refused by name.
pub const ACCEPTED_CONTENT_TYPES: &[&str] = &[
    "text/html",
    "application/xhtml+xml",
    "text/plain",
    "text/markdown",
    "text/x-markdown",
    "application/json",
    "application/xml",
    "text/xml",
];

const DETAIL_MAX_BYTES: usize = 1024;

/// `User-Agent` sent with every fetch.
pub fn user_agent() -> String {
    format!(
        "Axocoatl-web_fetch/{} (+https://axocoatl.ai)",
        env!("CARGO_PKG_VERSION")
    )
}

/// Why a fetch failed or was refused. `code` is the stable reason recorded
/// in the network record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchError {
    pub kind: FetchErrorKind,
    pub detail: String,
    /// Redirect targets followed before the failure, in order.
    pub redirects: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchErrorKind {
    /// Not an `http`/`https` URL, has `user:password@`, has no host, or is too long.
    InvalidUrl,
    /// A destination address is private (RFC 1918, CGNAT, unique local, ...).
    PrivateDestination,
    /// A destination address is loopback, link-local, multicast or another
    /// range Axocoatl never connects to.
    ForbiddenDestination,
    /// The name did not resolve.
    ResolveFailed,
    /// More than [`MAX_REDIRECTS`] redirects.
    TooManyRedirects,
    /// The response's content type is not one `web_fetch` reads.
    UnsupportedContentType,
    /// The connection, request or body read failed or timed out.
    FetchFailed,
}

impl FetchErrorKind {
    pub fn code(self) -> &'static str {
        match self {
            Self::InvalidUrl => "invalid_url",
            Self::PrivateDestination => "private_destination",
            Self::ForbiddenDestination => "forbidden_destination",
            Self::ResolveFailed => "resolve_failed",
            Self::TooManyRedirects => "too_many_redirects",
            Self::UnsupportedContentType => "unsupported_content_type",
            Self::FetchFailed => "fetch_failed",
        }
    }

    /// Whether Axocoatl refused the destination, as opposed to a failure of
    /// a request it allowed.
    pub fn is_refusal(self) -> bool {
        matches!(
            self,
            Self::InvalidUrl
                | Self::PrivateDestination
                | Self::ForbiddenDestination
                | Self::TooManyRedirects
        )
    }
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{} ({})", self.detail, self.kind.code())
    }
}

impl std::error::Error for FetchError {}

impl FetchError {
    fn new(kind: FetchErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: limit_text(detail.into(), DETAIL_MAX_BYTES).text,
            redirects: Vec::new(),
        }
    }
}

/// A response body `web_fetch` may read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedPage {
    /// The requested URL, as parsed (fragment removed).
    pub url: String,
    /// The URL that answered, after redirects.
    pub final_url: String,
    pub status: u16,
    /// Lowercase media type without parameters, such as `text/html`.
    pub content_type: String,
    /// The `charset` parameter, if any.
    pub charset: Option<String>,
    /// The body bytes received, at most the configured cap.
    pub body: Vec<u8>,
    /// The body was longer than the cap; `body` is its prefix.
    pub body_truncated: bool,
    /// Redirect targets followed, in order.
    pub redirects: Vec<String>,
}

/// What `web_fetch` reads pages through. [`FetchGuard`] is the real one.
#[async_trait::async_trait]
pub trait PageFetcher: Send + Sync + 'static {
    async fn fetch(&self, url: &str) -> Result<FetchedPage, FetchError>;
}

/// How an address is classified. Production always uses
/// [`netaddr::classify`]; tests may substitute one so a loopback test server
/// can stand in for the internet.
pub type Classifier = fn(IpAddr) -> AddrClass;

/// Names a test answers itself, before the host resolver. Only the
/// test-only constructor can set one.
type TestLookup = Arc<dyn Fn(&str) -> Option<Vec<IpAddr>> + Send + Sync>;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// The refusal a [`GuardResolver`] returns, found again in reqwest's error
/// chain.
#[derive(Debug)]
struct RefusedAddress {
    host: String,
    address: IpAddr,
    class: AddrClass,
}

impl std::fmt::Display for RefusedAddress {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} resolves to {} ({})",
            self.host,
            self.address,
            self.class.label()
        )
    }
}

impl std::error::Error for RefusedAddress {}

#[derive(Debug)]
struct ResolveFailure(String);

impl std::fmt::Display for ResolveFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ResolveFailure {}

/// Resolves names with the host resolver and fails unless every address is
/// public.
struct GuardResolver {
    classify: Classifier,
    lookup: Option<TestLookup>,
}

impl GuardResolver {
    async fn lookup(&self, host: &str) -> Result<Vec<IpAddr>, String> {
        if let Some(lookup) = &self.lookup {
            if let Some(addresses) = lookup(host) {
                return Ok(addresses);
            }
        }
        let resolved = tokio::time::timeout(RESOLVE_TIMEOUT, tokio::net::lookup_host((host, 0)))
            .await
            .map_err(|_| format!("resolving {host} timed out"))?
            .map_err(|error| format!("resolving {host} failed: {error}"))?;
        let mut addresses: Vec<IpAddr> = Vec::new();
        for address in resolved {
            if !addresses.contains(&address.ip()) {
                addresses.push(address.ip());
            }
        }
        Ok(addresses)
    }
}

impl reqwest::dns::Resolve for GuardResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        let classify = self.classify;
        let resolver = GuardResolver {
            classify,
            lookup: self.lookup.clone(),
        };
        Box::pin(async move {
            let addresses = resolver
                .lookup(&host)
                .await
                .map_err(|reason| Box::new(ResolveFailure(reason)) as BoxError)?;
            if addresses.is_empty() {
                return Err(
                    Box::new(ResolveFailure(format!("{host} has no addresses"))) as BoxError
                );
            }
            // All or nothing: one non-public answer refuses the name.
            for address in &addresses {
                let class = classify(*address);
                if !class.is_public() {
                    return Err(Box::new(RefusedAddress {
                        host: host.clone(),
                        address: *address,
                        class,
                    }) as BoxError);
                }
            }
            let addrs: reqwest::dns::Addrs = Box::new(
                addresses
                    .into_iter()
                    .map(|address| SocketAddr::new(address, 0)),
            );
            Ok(addrs)
        })
    }
}

/// Fetches pages for `web_fetch` without reaching private or local addresses.
pub struct FetchGuard {
    client: reqwest::Client,
    classify: Classifier,
    max_bytes: u64,
    timeout: Duration,
}

impl std::fmt::Debug for FetchGuard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FetchGuard")
            .field("max_bytes", &self.max_bytes)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl FetchGuard {
    /// A guard reading at most `max_bytes` of a body, with `timeout` for the
    /// whole fetch including redirects.
    pub fn new(max_bytes: u64, timeout: Duration) -> Result<Self, String> {
        Self::build(netaddr::classify, max_bytes, timeout, None)
    }

    fn build(
        classify: Classifier,
        max_bytes: u64,
        timeout: Duration,
        lookup: Option<TestLookup>,
    ) -> Result<Self, String> {
        let resolver = GuardResolver { classify, lookup };
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(timeout)
            .user_agent(user_agent())
            .dns_resolver(Arc::new(resolver))
            .build()
            .map_err(|error| format!("building the web_fetch client failed: {error}"))?;
        Ok(Self {
            client,
            classify,
            max_bytes,
            timeout,
        })
    }

    /// Test seam: classify addresses with `classify` instead of
    /// [`netaddr::classify`], and answer names from `lookup` first.
    #[cfg(test)]
    pub(crate) fn with_classifier(
        classify: Classifier,
        lookup: Option<TestLookup>,
        max_bytes: u64,
        timeout: Duration,
    ) -> Self {
        Self::build(classify, max_bytes, timeout, lookup).unwrap()
    }

    /// Check one URL before it is requested: scheme, user information,
    /// length, host, and the class of an IP-literal host.
    pub fn check_url(&self, raw: &str) -> Result<url::Url, FetchError> {
        check_url_with(raw, self.classify)
    }

    async fn fetch_inner(
        &self,
        raw: &str,
        redirects: &mut Vec<String>,
    ) -> Result<FetchedPage, FetchError> {
        let requested = self.check_url(raw)?;
        let mut current = requested.clone();
        let response = loop {
            let response = self
                .client
                .get(current.clone())
                .header(reqwest::header::ACCEPT, ACCEPT)
                .send()
                .await
                .map_err(|error| request_error(&current, &error))?;
            let status = response.status();
            if !status.is_redirection() {
                break response;
            }
            let Some(location) = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
            else {
                break response;
            };
            if redirects.len() >= MAX_REDIRECTS {
                return Err(FetchError::new(
                    FetchErrorKind::TooManyRedirects,
                    format!("more than {MAX_REDIRECTS} redirects"),
                ));
            }
            let next = current.join(location).map_err(|error| {
                FetchError::new(
                    FetchErrorKind::InvalidUrl,
                    format!("redirect to an invalid URL: {error}"),
                )
            })?;
            let next = self.check_url(next.as_str())?;
            redirects.push(next.to_string());
            current = next;
        };

        let status = response.status().as_u16();
        let (content_type, charset) = content_type(response.headers());
        if !ACCEPTED_CONTENT_TYPES.contains(&content_type.as_str()) {
            let named = if content_type.is_empty() {
                "no content type".to_string()
            } else {
                content_type.clone()
            };
            return Err(FetchError::new(
                FetchErrorKind::UnsupportedContentType,
                format!(
                    "web_fetch reads HTML, plain text, Markdown, JSON and XML; {} returned {named}",
                    current
                ),
            ));
        }
        let (body, body_truncated) = read_capped(response, self.max_bytes).await?;
        Ok(FetchedPage {
            url: requested.to_string(),
            final_url: current.to_string(),
            status,
            content_type,
            charset,
            body,
            body_truncated,
            redirects: redirects.clone(),
        })
    }
}

#[async_trait::async_trait]
impl PageFetcher for FetchGuard {
    async fn fetch(&self, url: &str) -> Result<FetchedPage, FetchError> {
        let mut redirects = Vec::new();
        let result =
            match tokio::time::timeout(self.timeout, self.fetch_inner(url, &mut redirects)).await {
                Ok(result) => result,
                Err(_) => Err(FetchError::new(
                    FetchErrorKind::FetchFailed,
                    format!("the fetch took longer than {} s", self.timeout.as_secs()),
                )),
            };
        result.map_err(|mut error| {
            error.redirects = redirects;
            error
        })
    }
}

/// [`FetchGuard::check_url`] with an explicit classifier.
pub fn check_url_with(raw: &str, classify: Classifier) -> Result<url::Url, FetchError> {
    let invalid = |detail: String| FetchError::new(FetchErrorKind::InvalidUrl, detail);
    if raw.chars().count() > MAX_URL_CHARS {
        return Err(invalid(format!(
            "the URL is longer than {MAX_URL_CHARS} characters"
        )));
    }
    let mut url = url::Url::parse(raw.trim()).map_err(|error| invalid(format!("{error}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(invalid(format!(
            "only http and https URLs can be fetched, not {}:",
            url.scheme()
        )));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(invalid(
            "URLs with user:password@ are refused; web_fetch sends no credentials".into(),
        ));
    }
    url.set_fragment(None);
    let address = match url.host() {
        None => return Err(invalid("the URL has no host".into())),
        Some(url::Host::Domain(_)) => None,
        Some(url::Host::Ipv4(address)) => Some(IpAddr::V4(address)),
        Some(url::Host::Ipv6(address)) => Some(IpAddr::V6(address)),
    };
    if let Some(address) = address {
        refuse_class(&address.to_string(), address, classify(address))?;
    }
    Ok(url)
}

fn refuse_class(host: &str, address: IpAddr, class: AddrClass) -> Result<(), FetchError> {
    match class {
        AddrClass::Public => Ok(()),
        AddrClass::Private(_) => Err(FetchError::new(
            FetchErrorKind::PrivateDestination,
            format!(
                "{host} is a private address ({address}, {}); web_fetch reads only public pages",
                class.label()
            ),
        )),
        AddrClass::Forbidden(_) => Err(FetchError::new(
            FetchErrorKind::ForbiddenDestination,
            format!(
                "{host} is a {} address ({address}); web_fetch never reads local or special addresses",
                class.label()
            ),
        )),
    }
}

/// Map a reqwest error, finding a resolver refusal in its source chain.
fn request_error(url: &url::Url, error: &reqwest::Error) -> FetchError {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(current) = source {
        if let Some(refused) = current.downcast_ref::<RefusedAddress>() {
            return refuse_class(&refused.host, refused.address, refused.class)
                .expect_err("a refused address is never public");
        }
        if let Some(failure) = current.downcast_ref::<ResolveFailure>() {
            return FetchError::new(FetchErrorKind::ResolveFailed, failure.0.clone());
        }
        source = current.source();
    }
    let what = if error.is_timeout() {
        "timed out"
    } else if error.is_connect() {
        "could not connect"
    } else {
        "failed"
    };
    FetchError::new(
        FetchErrorKind::FetchFailed,
        format!("requesting {url} {what}: {error}"),
    )
}

fn content_type(headers: &reqwest::header::HeaderMap) -> (String, Option<String>) {
    let Some(value) = headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
    else {
        return (String::new(), None);
    };
    let mut parts = value.split(';');
    let essence = parts.next().unwrap_or_default().trim().to_ascii_lowercase();
    let charset = parts.find_map(|part| {
        let (name, value) = part.split_once('=')?;
        name.trim()
            .eq_ignore_ascii_case("charset")
            .then(|| value.trim().trim_matches('"').to_ascii_lowercase())
    });
    (essence, charset)
}

/// Read at most `max_bytes` of the body. Past the cap the prefix is kept and
/// the rest is not read.
async fn read_capped(
    mut response: reqwest::Response,
    max_bytes: u64,
) -> Result<(Vec<u8>, bool), FetchError> {
    let max = usize::try_from(max_bytes).unwrap_or(usize::MAX);
    let declared = response.content_length().unwrap_or(0);
    let mut body = Vec::with_capacity(usize::try_from(declared).unwrap_or(0).min(max));
    while let Some(chunk) = response.chunk().await.map_err(|error| {
        FetchError::new(
            FetchErrorKind::FetchFailed,
            format!("reading the response body failed: {error}"),
        )
    })? {
        let room = max - body.len();
        if chunk.len() > room {
            body.extend_from_slice(&chunk[..room]);
            return Ok((body, true));
        }
        body.extend_from_slice(&chunk);
    }
    Ok((body, false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Treat loopback as public so a local server can stand in for the
    /// internet; everything else keeps its real class.
    fn loopback_is_public(address: IpAddr) -> AddrClass {
        if address.is_loopback() {
            AddrClass::Public
        } else {
            netaddr::classify(address)
        }
    }

    type Routes = HashMap<String, (u16, Vec<(String, String)>, Vec<u8>)>;

    /// A minimal HTTP/1.1 server answering from a fixed route table.
    async fn serve(routes: Routes) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let routes = Arc::new(routes);
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let routes = routes.clone();
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    let mut buffer = [0u8; 1024];
                    while !head.windows(4).any(|window| window == b"\r\n\r\n") {
                        let Ok(read) = stream.read(&mut buffer).await else {
                            return;
                        };
                        if read == 0 {
                            return;
                        }
                        head.extend_from_slice(&buffer[..read]);
                    }
                    let head = String::from_utf8_lossy(&head);
                    let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let (status, headers, body) = routes.get(&path).cloned().unwrap_or((
                        404,
                        vec![("Content-Type".into(), "text/plain".into())],
                        b"missing".to_vec(),
                    ));
                    let mut response = format!(
                        "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n",
                        body.len()
                    );
                    for (name, value) in headers {
                        response.push_str(&format!("{name}: {value}\r\n"));
                    }
                    response.push_str("\r\n");
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.write_all(&body).await;
                });
            }
        });
        address
    }

    fn html(body: &str) -> (u16, Vec<(String, String)>, Vec<u8>) {
        (
            200,
            vec![("Content-Type".into(), "text/html; charset=UTF-8".into())],
            body.as_bytes().to_vec(),
        )
    }

    fn redirect(location: &str) -> (u16, Vec<(String, String)>, Vec<u8>) {
        (302, vec![("Location".into(), location.into())], Vec::new())
    }

    fn guard(lookup: Option<TestLookup>, max_bytes: u64) -> FetchGuard {
        FetchGuard::with_classifier(
            loopback_is_public,
            lookup,
            max_bytes,
            Duration::from_secs(10),
        )
    }

    fn real_guard(lookup: Option<TestLookup>) -> FetchGuard {
        FetchGuard::with_classifier(netaddr::classify, lookup, 1 << 20, Duration::from_secs(10))
    }

    fn table(entries: &[(&str, &[&str])]) -> TestLookup {
        let map: HashMap<String, Vec<IpAddr>> = entries
            .iter()
            .map(|(host, addresses)| {
                (
                    host.to_string(),
                    addresses
                        .iter()
                        .map(|address| address.parse().unwrap())
                        .collect(),
                )
            })
            .collect();
        Arc::new(move |host: &str| map.get(host).cloned())
    }

    #[test]
    fn url_checks_refuse_scheme_userinfo_length_and_literal_hosts() {
        let check = |raw: &str| check_url_with(raw, netaddr::classify).map_err(|error| error.kind);
        assert!(check("https://example.com/a?b=c#frag").is_ok());
        assert_eq!(
            check("https://example.com/a#frag").unwrap().as_str(),
            "https://example.com/a"
        );
        for (raw, kind) in [
            ("ftp://example.com/", FetchErrorKind::InvalidUrl),
            ("file:///etc/passwd", FetchErrorKind::InvalidUrl),
            ("javascript:alert(1)", FetchErrorKind::InvalidUrl),
            ("https://user:pass@example.com/", FetchErrorKind::InvalidUrl),
            ("https://user@example.com/", FetchErrorKind::InvalidUrl),
            ("not a url", FetchErrorKind::InvalidUrl),
            ("http://10.0.0.1/", FetchErrorKind::PrivateDestination),
            (
                "http://192.168.1.10:8080/",
                FetchErrorKind::PrivateDestination,
            ),
            ("http://[fd00::1]/", FetchErrorKind::PrivateDestination),
            ("http://127.0.0.1/", FetchErrorKind::ForbiddenDestination),
            (
                "http://169.254.169.254/latest/meta-data",
                FetchErrorKind::ForbiddenDestination,
            ),
            ("http://[::1]:3000/", FetchErrorKind::ForbiddenDestination),
            (
                "http://[::ffff:127.0.0.1]/",
                FetchErrorKind::ForbiddenDestination,
            ),
            (
                "http://[::ffff:10.0.0.1]/",
                FetchErrorKind::PrivateDestination,
            ),
            (
                "http://[64:ff9b::a9fe:a9fe]/",
                FetchErrorKind::ForbiddenDestination,
            ),
            // Numeric spellings are normalized by the URL parser, then classified.
            ("http://2130706433/", FetchErrorKind::ForbiddenDestination),
            ("http://0x7f.1/", FetchErrorKind::ForbiddenDestination),
            ("http://0/", FetchErrorKind::ForbiddenDestination),
        ] {
            assert_eq!(check(raw).unwrap_err(), kind, "{raw}");
        }
        let long = format!("https://example.com/{}", "a".repeat(MAX_URL_CHARS));
        assert_eq!(check(&long).unwrap_err(), FetchErrorKind::InvalidUrl);
    }

    #[tokio::test]
    async fn resolved_private_forbidden_mixed_and_mapped_answers_are_refused() {
        let guard = real_guard(Some(table(&[
            ("private.test", &["10.1.2.3"]),
            ("forbidden.test", &["169.254.169.254"]),
            ("mixed.test", &["93.184.216.34", "192.168.0.7"]),
            ("mapped.test", &["::ffff:127.0.0.1"]),
            ("teredo.test", &["2001::1"]),
        ])));
        for (host, kind) in [
            ("private.test", FetchErrorKind::PrivateDestination),
            ("forbidden.test", FetchErrorKind::ForbiddenDestination),
            ("mixed.test", FetchErrorKind::PrivateDestination),
            ("mapped.test", FetchErrorKind::ForbiddenDestination),
            ("teredo.test", FetchErrorKind::ForbiddenDestination),
        ] {
            let error = guard.fetch(&format!("http://{host}/")).await.unwrap_err();
            assert_eq!(error.kind, kind, "{host}: {error}");
            assert!(error.detail.contains(host), "{error}");
        }
        // The real resolver: localhost is loopback.
        let error = guard.fetch("http://localhost:9/").await.unwrap_err();
        assert_eq!(error.kind, FetchErrorKind::ForbiddenDestination, "{error}");
    }

    #[tokio::test]
    async fn fetches_html_and_follows_checked_redirects() {
        let mut routes = Routes::new();
        routes.insert("/page".into(), html("<title>T</title><p>Hello</p>"));
        routes.insert("/one".into(), redirect("/two"));
        routes.insert("/two".into(), redirect("/page"));
        let address = serve(routes).await;
        let guard = guard(None, 1 << 20);
        let page = guard
            .fetch(&format!("http://{address}/one#x"))
            .await
            .unwrap();
        assert_eq!(page.status, 200);
        assert_eq!(page.content_type, "text/html");
        assert_eq!(page.charset.as_deref(), Some("utf-8"));
        assert_eq!(page.url, format!("http://{address}/one"));
        assert_eq!(page.final_url, format!("http://{address}/page"));
        assert_eq!(
            page.redirects,
            [
                format!("http://{address}/two"),
                format!("http://{address}/page")
            ]
        );
        assert_eq!(page.body, b"<title>T</title><p>Hello</p>");
        assert!(!page.body_truncated);
    }

    #[tokio::test]
    async fn a_redirect_to_a_private_host_is_refused_at_that_hop() {
        let mut routes = Routes::new();
        routes.insert("/literal".into(), redirect("http://10.9.8.7/admin"));
        routes.insert("/named".into(), redirect("http://internal.test/admin"));
        routes.insert("/metadata".into(), redirect("http://169.254.169.254/"));
        routes.insert("/scheme".into(), redirect("file:///etc/passwd"));
        routes.insert("/creds".into(), redirect("http://a:b@example.com/"));
        let address = serve(routes).await;
        let guard = guard(
            Some(table(&[("internal.test", &["192.168.50.1"])])),
            1 << 20,
        );
        for (path, kind) in [
            ("/literal", FetchErrorKind::PrivateDestination),
            ("/named", FetchErrorKind::PrivateDestination),
            ("/metadata", FetchErrorKind::ForbiddenDestination),
            ("/scheme", FetchErrorKind::InvalidUrl),
            ("/creds", FetchErrorKind::InvalidUrl),
        ] {
            let error = guard
                .fetch(&format!("http://{address}{path}"))
                .await
                .unwrap_err();
            assert_eq!(error.kind, kind, "{path}: {error}");
            assert!(error.kind.is_refusal());
        }
        // The named hop was followed (and recorded) before its resolution refused it.
        let error = guard
            .fetch(&format!("http://{address}/named"))
            .await
            .unwrap_err();
        assert_eq!(error.redirects, ["http://internal.test/admin"]);
    }

    #[tokio::test]
    async fn redirects_are_limited() {
        let mut routes = Routes::new();
        for hop in 0..8 {
            routes.insert(format!("/r{hop}"), redirect(&format!("/r{}", hop + 1)));
        }
        let address = serve(routes).await;
        let error = guard(None, 1 << 20)
            .fetch(&format!("http://{address}/r0"))
            .await
            .unwrap_err();
        assert_eq!(error.kind, FetchErrorKind::TooManyRedirects);
        assert_eq!(error.redirects.len(), MAX_REDIRECTS);
    }

    #[tokio::test]
    async fn body_cap_keeps_the_prefix() {
        let mut routes = Routes::new();
        routes.insert(
            "/big".into(),
            (
                200,
                vec![("Content-Type".into(), "text/plain".into())],
                vec![b'x'; 300_000],
            ),
        );
        let address = serve(routes).await;
        let page = guard(None, 65_536)
            .fetch(&format!("http://{address}/big"))
            .await
            .unwrap();
        assert!(page.body_truncated);
        assert_eq!(page.body.len(), 65_536);
    }

    #[tokio::test]
    async fn other_content_types_are_refused_by_name() {
        let mut routes = Routes::new();
        routes.insert(
            "/image".into(),
            (
                200,
                vec![("Content-Type".into(), "image/png".into())],
                vec![0x89, b'P', b'N', b'G'],
            ),
        );
        routes.insert("/none".into(), (200, Vec::new(), b"x".to_vec()));
        routes.insert(
            "/json".into(),
            (
                200,
                vec![("Content-Type".into(), "Application/JSON".into())],
                b"{}".to_vec(),
            ),
        );
        let address = serve(routes).await;
        let guard = guard(None, 1 << 20);
        let error = guard
            .fetch(&format!("http://{address}/image"))
            .await
            .unwrap_err();
        assert_eq!(error.kind, FetchErrorKind::UnsupportedContentType);
        assert!(error.detail.contains("image/png"), "{error}");
        assert!(!error.kind.is_refusal());
        let error = guard
            .fetch(&format!("http://{address}/none"))
            .await
            .unwrap_err();
        assert!(error.detail.contains("no content type"), "{error}");
        let page = guard
            .fetch(&format!("http://{address}/json"))
            .await
            .unwrap();
        assert_eq!(page.content_type, "application/json");
    }

    #[test]
    fn user_agent_names_the_tool_and_version() {
        assert!(user_agent().starts_with("Axocoatl-web_fetch/"));
        assert!(user_agent().ends_with("(+https://axocoatl.ai)"));
    }
}
