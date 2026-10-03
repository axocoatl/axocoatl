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
//!
//! A public address can still be this computer: its own global IPv6 address,
//! or a public IPv4 address bound on a server, reaches every service listening
//! on all interfaces. So every destination is also checked against the
//! addresses of this computer's network interfaces, read again for each
//! check, and refused when it is one of them or is on the same directly
//! connected network as one of them.

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
    /// A destination address is one of this computer's own interface
    /// addresses, or on the network one of them is directly connected to.
    LocalDestination,
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
            Self::LocalDestination => "local_destination",
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
                | Self::LocalDestination
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

/// One address of this computer's network interfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InterfaceAddress {
    pub address: IpAddr,
    /// The length of the network prefix the interface is directly
    /// connected to, when that network is an ordinary one: `None` for a
    /// point-to-point link or a mask that is not a prefix.
    pub prefix: Option<u8>,
}

/// Where this computer's own addresses come from. Production reads them
/// with `getifaddrs` on every check; tests substitute a fixed set.
pub type LocalAddresses = Arc<dyn Fn() -> Result<Vec<InterfaceAddress>, String> + Send + Sync>;

/// The addresses of this computer's network interfaces, with each one's
/// prefix length.
#[cfg(unix)]
pub fn interface_addresses() -> std::io::Result<Vec<InterfaceAddress>> {
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: on success `getifaddrs` stores a list that stays valid until
    // the `freeifaddrs` below; nothing read from it outlives this function.
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut found = Vec::new();
    let mut cursor = head;
    while !cursor.is_null() {
        // SAFETY: `cursor` is a non-null node of the list `getifaddrs` made.
        let entry = unsafe { &*cursor };
        // SAFETY: each pointer is null or points at a socket address of the
        // length its family (and, on the BSDs, its `sa_len`) says.
        if let Some(address) = unsafe { socket_ip(entry.ifa_addr) } {
            let point_to_point = entry.ifa_flags & (libc::IFF_POINTOPOINT as libc::c_uint) != 0;
            let prefix = if point_to_point {
                None
            } else {
                unsafe { netmask_prefix(entry.ifa_netmask, address) }
            };
            found.push(InterfaceAddress { address, prefix });
        }
        cursor = entry.ifa_next;
    }
    // SAFETY: `head` came from a successful `getifaddrs` and is freed once.
    unsafe { libc::freeifaddrs(head) };
    Ok(found)
}

/// Without `getifaddrs` no interface address is known.
#[cfg(not(unix))]
pub fn interface_addresses() -> std::io::Result<Vec<InterfaceAddress>> {
    Ok(Vec::new())
}

/// The bytes of a socket address the kernel filled in. On the BSDs and macOS
/// `sa_len` may be shorter than the structure (a netmask often is); missing
/// bytes read as zero.
#[cfg(unix)]
unsafe fn socket_bytes(socket: *const libc::sockaddr, full: usize) -> Vec<u8> {
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    ))]
    let length = usize::from((*socket).sa_len).min(full);
    #[cfg(not(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    )))]
    let length = full;
    let mut bytes = vec![0u8; full];
    std::ptr::copy_nonoverlapping(socket.cast::<u8>(), bytes.as_mut_ptr(), length);
    bytes
}

/// The IP address in an interface's `ifa_addr`.
#[cfg(unix)]
unsafe fn socket_ip(socket: *const libc::sockaddr) -> Option<IpAddr> {
    if socket.is_null() {
        return None;
    }
    match i32::from((*socket).sa_family) {
        libc::AF_INET => {
            let bytes = socket_bytes(socket, std::mem::size_of::<libc::sockaddr_in>());
            let octets: [u8; 4] = bytes[4..8].try_into().ok()?;
            Some(IpAddr::from(octets))
        }
        libc::AF_INET6 => {
            let bytes = socket_bytes(socket, std::mem::size_of::<libc::sockaddr_in6>());
            let octets: [u8; 16] = bytes[8..24].try_into().ok()?;
            Some(IpAddr::from(octets))
        }
        _ => None,
    }
}

/// The prefix length of an interface's `ifa_netmask`, read in the family of
/// its address because a netmask's own family field is not always set.
#[cfg(unix)]
unsafe fn netmask_prefix(socket: *const libc::sockaddr, address: IpAddr) -> Option<u8> {
    if socket.is_null() {
        return None;
    }
    let mask: Vec<u8> = match address {
        IpAddr::V4(_) => {
            socket_bytes(socket, std::mem::size_of::<libc::sockaddr_in>())[4..8].to_vec()
        }
        IpAddr::V6(_) => {
            socket_bytes(socket, std::mem::size_of::<libc::sockaddr_in6>())[8..24].to_vec()
        }
    };
    prefix_length(&mask)
}

/// Leading one bits of a contiguous mask, or `None` when the mask has a
/// one after a zero.
fn prefix_length(mask: &[u8]) -> Option<u8> {
    let mut ones = 0u32;
    let mut ended = false;
    for byte in mask {
        for bit in (0..8).rev() {
            if byte & (1 << bit) != 0 {
                if ended {
                    return None;
                }
                ones += 1;
            } else {
                ended = true;
            }
        }
    }
    u8::try_from(ones).ok()
}

/// The production source of this computer's addresses.
fn system_local_addresses() -> LocalAddresses {
    Arc::new(|| {
        interface_addresses()
            .map_err(|error| format!("listing this computer's network addresses failed: {error}"))
    })
}

/// Shortest prefixes treated as a directly connected network. A wider mask
/// is not an ordinary local network, so only the interface's own address is
/// refused.
const MIN_LOCAL_V4_PREFIX: u8 = 16;
const MIN_LOCAL_V6_PREFIX: u8 = 48;

/// How a destination is this computer or its network.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalMatch {
    /// One of this computer's own addresses.
    Own,
    /// On the network `interface/prefix` is directly connected to.
    SameNetwork { interface: IpAddr, prefix: u8 },
}

fn same_prefix(left: IpAddr, right: IpAddr, prefix: u8) -> bool {
    let (left, right, bits): (u128, u128, u32) = match (left, right) {
        (IpAddr::V4(left), IpAddr::V4(right)) => {
            (u32::from(left).into(), u32::from(right).into(), 32)
        }
        (IpAddr::V6(left), IpAddr::V6(right)) => (left.into(), right.into(), 128),
        _ => return false,
    };
    let prefix = u32::from(prefix).min(bits);
    if prefix == 0 {
        return true;
    }
    let shift = bits - prefix;
    (left >> shift) == (right >> shift)
}

/// Whether `address` is this computer or on a network one of `interfaces`
/// is directly connected to. An IPv4-mapped IPv6 address is checked as its
/// IPv4 address too.
fn local_match(address: IpAddr, interfaces: &[InterfaceAddress]) -> Option<LocalMatch> {
    let mapped = match address {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4),
        IpAddr::V4(_) => None,
    };
    let candidates = std::iter::once(address).chain(mapped);
    let mut same_network = None;
    for candidate in candidates {
        for interface in interfaces {
            if interface.address == candidate {
                return Some(LocalMatch::Own);
            }
            let Some(prefix) = interface.prefix else {
                continue;
            };
            let minimum = match interface.address {
                IpAddr::V4(_) => MIN_LOCAL_V4_PREFIX,
                IpAddr::V6(_) => MIN_LOCAL_V6_PREFIX,
            };
            if same_network.is_none()
                && prefix >= minimum
                && same_prefix(candidate, interface.address, prefix)
            {
                same_network = Some(LocalMatch::SameNetwork {
                    interface: interface.address,
                    prefix,
                });
            }
        }
    }
    same_network
}

/// Why a [`GuardResolver`] refused an address.
#[derive(Debug, Clone, Copy)]
enum Refusal {
    Class(AddrClass),
    Local(LocalMatch),
}

/// The refusal a [`GuardResolver`] returns, found again in reqwest's error
/// chain.
#[derive(Debug)]
struct RefusedAddress {
    host: String,
    address: IpAddr,
    refusal: Refusal,
}

impl std::fmt::Display for RefusedAddress {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let what = match self.refusal {
            Refusal::Class(class) => class.label(),
            Refusal::Local(LocalMatch::Own) => "this computer",
            Refusal::Local(LocalMatch::SameNetwork { .. }) => "this computer's network",
        };
        write!(
            formatter,
            "{} resolves to {} ({what})",
            self.host, self.address
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
/// public and none is this computer or on its network.
struct GuardResolver {
    classify: Classifier,
    lookup: Option<TestLookup>,
    local: LocalAddresses,
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
            local: self.local.clone(),
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
                        refusal: Refusal::Class(class),
                    }) as BoxError);
                }
            }
            // A public answer may still be this computer or its network.
            let interfaces = (resolver.local)()
                .map_err(|reason| Box::new(ResolveFailure(reason)) as BoxError)?;
            for address in &addresses {
                if let Some(matched) = local_match(*address, &interfaces) {
                    return Err(Box::new(RefusedAddress {
                        host: host.clone(),
                        address: *address,
                        refusal: Refusal::Local(matched),
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
    local: LocalAddresses,
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
        Self::build(
            netaddr::classify,
            max_bytes,
            timeout,
            None,
            system_local_addresses(),
        )
    }

    fn build(
        classify: Classifier,
        max_bytes: u64,
        timeout: Duration,
        lookup: Option<TestLookup>,
        local: LocalAddresses,
    ) -> Result<Self, String> {
        let resolver = GuardResolver {
            classify,
            lookup,
            local: local.clone(),
        };
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
            local,
            max_bytes,
            timeout,
        })
    }

    /// Test seam: classify addresses with `classify` instead of
    /// [`netaddr::classify`], answer names from `lookup` first, and take this
    /// computer's addresses from `local` (the real interfaces when `None`).
    #[cfg(test)]
    pub(crate) fn with_classifier(
        classify: Classifier,
        lookup: Option<TestLookup>,
        local: Option<LocalAddresses>,
        max_bytes: u64,
        timeout: Duration,
    ) -> Self {
        Self::build(
            classify,
            max_bytes,
            timeout,
            lookup,
            local.unwrap_or_else(system_local_addresses),
        )
        .unwrap()
    }

    /// Check one URL before it is requested: scheme, user information,
    /// length, host, and for an IP-literal host its class and whether it is
    /// this computer or on its network.
    pub fn check_url(&self, raw: &str) -> Result<url::Url, FetchError> {
        let url = check_url_with(raw, self.classify)?;
        let address = match url.host() {
            Some(url::Host::Ipv4(address)) => Some(IpAddr::V4(address)),
            Some(url::Host::Ipv6(address)) => Some(IpAddr::V6(address)),
            _ => None,
        };
        if let Some(address) = address {
            let interfaces = (self.local)()
                .map_err(|reason| FetchError::new(FetchErrorKind::FetchFailed, reason))?;
            if let Some(matched) = local_match(address, &interfaces) {
                return Err(refuse_local(&address.to_string(), address, matched));
            }
        }
        Ok(url)
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

fn refuse_local(host: &str, address: IpAddr, matched: LocalMatch) -> FetchError {
    let detail = match matched {
        LocalMatch::Own => format!(
            "{host} is this computer's own address ({address}); web_fetch never reads from this \
             computer"
        ),
        LocalMatch::SameNetwork { interface, prefix } => format!(
            "{host} is on this computer's network ({address} is in the network of {interface}/{prefix}); \
             web_fetch never reads from your network"
        ),
    };
    FetchError::new(FetchErrorKind::LocalDestination, detail)
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
            return match refused.refusal {
                Refusal::Class(class) => refuse_class(&refused.host, refused.address, class)
                    .expect_err("a refused address is never public"),
                Refusal::Local(matched) => refuse_local(&refused.host, refused.address, matched),
            };
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

/// Whether `address` is this computer: a loopback or unspecified address,
/// or one of the addresses of its network interfaces, read again for each
/// call (an IPv4-mapped IPv6 address is checked as its IPv4 address too).
/// Unlike `web_fetch`, a destination merely on the same network is not
/// counted: callers that reach configured private destinations, such as the
/// egress route broker, decide about those themselves. If the interfaces
/// cannot be listed every address counts as local, so a caller that refuses
/// local destinations fails closed.
pub fn is_local_destination(address: IpAddr) -> bool {
    let mapped = match address {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4),
        IpAddr::V4(_) => None,
    };
    if std::iter::once(address)
        .chain(mapped)
        .any(|candidate| candidate.is_loopback() || candidate.is_unspecified())
    {
        return true;
    }
    match interface_addresses() {
        Ok(interfaces) => matches!(local_match(address, &interfaces), Some(LocalMatch::Own)),
        Err(_) => true,
    }
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

    /// No interface addresses, so the loopback test server is not "this
    /// computer".
    fn no_local_addresses() -> LocalAddresses {
        Arc::new(|| Ok(Vec::new()))
    }

    fn guard(lookup: Option<TestLookup>, max_bytes: u64) -> FetchGuard {
        FetchGuard::with_classifier(
            loopback_is_public,
            lookup,
            Some(no_local_addresses()),
            max_bytes,
            Duration::from_secs(10),
        )
    }

    fn real_guard(lookup: Option<TestLookup>) -> FetchGuard {
        FetchGuard::with_classifier(
            netaddr::classify,
            lookup,
            None,
            1 << 20,
            Duration::from_secs(10),
        )
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

    fn local_set(entries: &[(&str, Option<u8>)]) -> LocalAddresses {
        let interfaces: Vec<InterfaceAddress> = entries
            .iter()
            .map(|(address, prefix)| InterfaceAddress {
                address: address.parse().unwrap(),
                prefix: *prefix,
            })
            .collect();
        Arc::new(move || Ok(interfaces.clone()))
    }

    fn url_for(address: IpAddr) -> String {
        match address {
            IpAddr::V4(v4) => format!("http://{v4}:5000/"),
            IpAddr::V6(v6) => format!("http://[{v6}]:5000/"),
        }
    }

    #[test]
    fn prefixes_and_local_matches() {
        assert_eq!(prefix_length(&[255, 255, 255, 0]), Some(24));
        assert_eq!(prefix_length(&[255, 255, 255, 255]), Some(32));
        assert_eq!(prefix_length(&[0; 16]), Some(0));
        assert_eq!(prefix_length(&[255, 0, 255, 0]), None);
        let mut v6 = [0u8; 16];
        v6[..8].fill(255);
        assert_eq!(prefix_length(&v6), Some(64));

        let interfaces = [
            InterfaceAddress {
                address: "2a01:4f8:c0c:1234::10".parse().unwrap(),
                prefix: Some(64),
            },
            InterfaceAddress {
                address: "81.2.69.160".parse().unwrap(),
                prefix: Some(24),
            },
            // A wide mask is not an ordinary local network.
            InterfaceAddress {
                address: "81.3.0.1".parse().unwrap(),
                prefix: Some(8),
            },
            // A point-to-point link has no network of its own.
            InterfaceAddress {
                address: "81.4.0.1".parse().unwrap(),
                prefix: None,
            },
        ];
        let matched = |raw: &str| local_match(raw.parse().unwrap(), &interfaces);
        assert_eq!(matched("2a01:4f8:c0c:1234::10"), Some(LocalMatch::Own));
        assert_eq!(matched("::ffff:81.2.69.160"), Some(LocalMatch::Own));
        assert_eq!(matched("81.3.0.1"), Some(LocalMatch::Own));
        assert_eq!(matched("81.4.0.1"), Some(LocalMatch::Own));
        assert!(matches!(
            matched("2a01:4f8:c0c:1234::1"),
            Some(LocalMatch::SameNetwork { prefix: 64, .. })
        ));
        assert!(matches!(
            matched("81.2.69.7"),
            Some(LocalMatch::SameNetwork { prefix: 24, .. })
        ));
        assert!(matches!(
            matched("::ffff:81.2.69.7"),
            Some(LocalMatch::SameNetwork { prefix: 24, .. })
        ));
        for other in ["2a01:4f8:c0c:1235::10", "81.2.70.1", "81.3.9.9", "81.4.0.2"] {
            assert_eq!(matched(other), None, "{other}");
        }
    }

    #[tokio::test]
    async fn this_computer_and_its_network_are_refused_by_literal_name_and_redirect() {
        let interfaces = local_set(&[
            ("2a01:4f8:c0c:1234::10", Some(64)),
            ("81.2.69.160", Some(24)),
        ]);
        let lookup = table(&[
            ("own.test", &["2a01:4f8:c0c:1234::10"]),
            ("neighbour.test", &["93.184.216.34", "2a01:4f8:c0c:1234::1"]),
            ("own4.test", &["81.2.69.160"]),
        ]);
        let guard = FetchGuard::with_classifier(
            netaddr::classify,
            Some(lookup.clone()),
            Some(interfaces.clone()),
            1 << 20,
            Duration::from_secs(10),
        );
        for raw in [
            "http://[2a01:4f8:c0c:1234::10]:5000/",
            "http://[2a01:4f8:c0c:1234::1]/",
            "http://81.2.69.160:7000/",
            "http://81.2.69.7/",
            "http://[::ffff:81.2.69.160]/",
        ] {
            let error = guard.check_url(raw).unwrap_err();
            assert_eq!(
                error.kind,
                FetchErrorKind::LocalDestination,
                "{raw}: {error}"
            );
            assert!(error.kind.is_refusal());
            assert_eq!(error.kind.code(), "local_destination");
        }
        for raw in ["http://[2a01:4f8:c0c:1235::1]/", "http://81.2.70.1/"] {
            assert!(guard.check_url(raw).is_ok(), "{raw}");
        }
        // A name is refused at resolution, before any connection.
        for host in ["own.test", "neighbour.test", "own4.test"] {
            let error = guard
                .fetch(&format!("http://{host}:5000/"))
                .await
                .unwrap_err();
            assert_eq!(
                error.kind,
                FetchErrorKind::LocalDestination,
                "{host}: {error}"
            );
            assert!(error.detail.contains(host), "{error}");
        }
        assert!(guard
            .fetch("http://own.test/")
            .await
            .unwrap_err()
            .detail
            .contains("this computer's own address"));

        // A redirect hop to this computer is refused the same way.
        let mut routes = Routes::new();
        routes.insert("/own".into(), redirect("http://own.test:5000/"));
        routes.insert("/literal".into(), redirect("http://81.2.69.160:7000/"));
        let address = serve(routes).await;
        let guard = FetchGuard::with_classifier(
            loopback_is_public,
            Some(lookup),
            Some(interfaces),
            1 << 20,
            Duration::from_secs(10),
        );
        for path in ["/own", "/literal"] {
            let error = guard
                .fetch(&format!("http://{address}{path}"))
                .await
                .unwrap_err();
            assert_eq!(
                error.kind,
                FetchErrorKind::LocalDestination,
                "{path}: {error}"
            );
            assert_eq!(error.redirects.len(), usize::from(path == "/own"));
        }

        // Without a list of this computer's addresses nothing is fetched.
        let failing: LocalAddresses = Arc::new(|| Err("no interfaces".into()));
        let guard = FetchGuard::with_classifier(
            netaddr::classify,
            Some(table(&[("elsewhere.test", &["81.2.70.1"])])),
            Some(failing),
            1 << 20,
            Duration::from_secs(10),
        );
        assert_eq!(
            guard.check_url("http://81.2.70.1/").unwrap_err().kind,
            FetchErrorKind::FetchFailed
        );
        assert_eq!(
            guard
                .fetch("http://elsewhere.test/")
                .await
                .unwrap_err()
                .kind,
            FetchErrorKind::ResolveFailed
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_real_interfaces_are_read_and_their_public_addresses_refused() {
        let interfaces = interface_addresses().unwrap();
        assert!(
            interfaces.iter().any(|interface| interface.address
                == IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
                && interface.prefix == Some(8)),
            "{interfaces:?}"
        );
        // Whatever public address this machine has is refused by the
        // production guard.
        let guard = real_guard(None);
        let mut public = 0;
        for interface in &interfaces {
            if !netaddr::classify(interface.address).is_public() {
                continue;
            }
            public += 1;
            let error = guard.check_url(&url_for(interface.address)).unwrap_err();
            assert_eq!(
                error.kind,
                FetchErrorKind::LocalDestination,
                "{interface:?}: {error}"
            );
        }
        let prefixes: Vec<Option<u8>> = interfaces
            .iter()
            .filter(|interface| netaddr::classify(interface.address).is_public())
            .map(|interface| interface.prefix)
            .collect();
        eprintln!(
            "fetch_guard: {public} public interface addresses refused, prefixes {prefixes:?}"
        );
    }

    #[test]
    fn user_agent_names_the_tool_and_version() {
        assert!(user_agent().starts_with("Axocoatl-web_fetch/"));
        assert!(user_agent().ends_with("(+https://axocoatl.ai)"));
    }
}
