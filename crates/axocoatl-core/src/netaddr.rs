//! Address classes shared by egress policy, `web_fetch` and config validation.
//!
//! One classifier decides whether a destination address is ordinary public
//! internet, a private range the user may opt into, or a range Axocoatl never
//! connects to. IPv6 forms that carry an IPv4 address (mapped, compatible,
//! NAT64, 6to4 and ISATAP) are classified by the stricter of the outer range
//! and the embedded IPv4 address, so `::ffff:127.0.0.1` is loopback.
//!
//! Host-name helpers are deliberately strict: a host that any resolver could
//! read as a number (`2130706433`, `0x7f.1`, `127.1`, `010.0.0.1`) is refused
//! as a name and is never an IP literal either.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::str::FromStr;

/// The class of one destination address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AddrClass {
    /// Ordinary internet address.
    Public,
    /// Not routed on the public internet. Allowed only when the user lists a
    /// range containing it.
    Private(PrivateKind),
    /// Never allowed, whatever the configuration says.
    Forbidden(ForbiddenKind),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PrivateKind {
    Rfc1918,
    Cgnat,
    UniqueLocal,
    Benchmarking,
    IetfProtocol,
    Nat64Local,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ForbiddenKind {
    Unspecified,
    Loopback,
    LinkLocal,
    Multicast,
    Broadcast,
    Reserved,
    Documentation,
    Teredo,
}

impl AddrClass {
    fn rank(self) -> u8 {
        match self {
            Self::Public => 0,
            Self::Private(_) => 1,
            Self::Forbidden(_) => 2,
        }
    }

    /// The stricter of two classes. On a tie the receiver is kept.
    pub fn stricter(self, other: Self) -> Self {
        if other.rank() > self.rank() {
            other
        } else {
            self
        }
    }

    pub fn is_public(self) -> bool {
        matches!(self, Self::Public)
    }

    pub fn is_forbidden(self) -> bool {
        matches!(self, Self::Forbidden(_))
    }

    /// Stable lowercase label for records and messages.
    pub fn label(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Private(kind) => kind.label(),
            Self::Forbidden(kind) => kind.label(),
        }
    }
}

impl PrivateKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Rfc1918 => "private",
            Self::Cgnat => "shared address space",
            Self::UniqueLocal => "unique local",
            Self::Benchmarking => "benchmarking",
            Self::IetfProtocol => "IETF protocol assignment",
            Self::Nat64Local => "local-use NAT64",
        }
    }
}

impl ForbiddenKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Unspecified => "unspecified",
            Self::Loopback => "loopback",
            Self::LinkLocal => "link-local",
            Self::Multicast => "multicast",
            Self::Broadcast => "broadcast",
            Self::Reserved => "reserved",
            Self::Documentation => "documentation",
            Self::Teredo => "Teredo",
        }
    }
}

impl fmt::Display for AddrClass {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

const fn v4(a: u8, b: u8, c: u8, d: u8, prefix: u8) -> Cidr {
    Cidr {
        addr: IpAddr::V4(Ipv4Addr::new(a, b, c, d)),
        prefix,
    }
}

#[allow(clippy::too_many_arguments)]
const fn v6(segments: [u16; 8], prefix: u8) -> Cidr {
    Cidr {
        addr: IpAddr::V6(Ipv6Addr::new(
            segments[0],
            segments[1],
            segments[2],
            segments[3],
            segments[4],
            segments[5],
            segments[6],
            segments[7],
        )),
        prefix,
    }
}

/// IPv4 ranges in lookup order. Narrower ranges precede the ranges that
/// contain them (`255.255.255.255/32` before `240/4`).
const V4_TABLE: [(Cidr, AddrClass); 16] = [
    (
        v4(0, 0, 0, 0, 8),
        AddrClass::Forbidden(ForbiddenKind::Unspecified),
    ),
    (v4(10, 0, 0, 0, 8), AddrClass::Private(PrivateKind::Rfc1918)),
    (
        v4(100, 64, 0, 0, 10),
        AddrClass::Private(PrivateKind::Cgnat),
    ),
    (
        v4(127, 0, 0, 0, 8),
        AddrClass::Forbidden(ForbiddenKind::Loopback),
    ),
    (
        v4(169, 254, 0, 0, 16),
        AddrClass::Forbidden(ForbiddenKind::LinkLocal),
    ),
    (
        v4(172, 16, 0, 0, 12),
        AddrClass::Private(PrivateKind::Rfc1918),
    ),
    (
        v4(192, 0, 0, 0, 24),
        AddrClass::Private(PrivateKind::IetfProtocol),
    ),
    (
        v4(192, 0, 2, 0, 24),
        AddrClass::Forbidden(ForbiddenKind::Documentation),
    ),
    (
        v4(192, 88, 99, 0, 24),
        AddrClass::Forbidden(ForbiddenKind::Reserved),
    ),
    (
        v4(192, 168, 0, 0, 16),
        AddrClass::Private(PrivateKind::Rfc1918),
    ),
    (
        v4(198, 18, 0, 0, 15),
        AddrClass::Private(PrivateKind::Benchmarking),
    ),
    (
        v4(198, 51, 100, 0, 24),
        AddrClass::Forbidden(ForbiddenKind::Documentation),
    ),
    (
        v4(203, 0, 113, 0, 24),
        AddrClass::Forbidden(ForbiddenKind::Documentation),
    ),
    (
        v4(224, 0, 0, 0, 4),
        AddrClass::Forbidden(ForbiddenKind::Multicast),
    ),
    (
        v4(255, 255, 255, 255, 32),
        AddrClass::Forbidden(ForbiddenKind::Broadcast),
    ),
    (
        v4(240, 0, 0, 0, 4),
        AddrClass::Forbidden(ForbiddenKind::Reserved),
    ),
];

/// IPv6 ranges with a fixed class. Embedding ranges (mapped, compatible,
/// NAT64, 6to4) are handled separately because their class comes from the
/// embedded IPv4 address. `3fff::/20` (RFC 9637) and `2001:2::/48` (RFC 5180)
/// go beyond the base table; both are stricter, never looser.
const V6_TABLE: [(Cidr, AddrClass); 12] = [
    (
        v6([0, 0, 0, 0, 0, 0, 0, 0], 128),
        AddrClass::Forbidden(ForbiddenKind::Unspecified),
    ),
    (
        v6([0, 0, 0, 0, 0, 0, 0, 1], 128),
        AddrClass::Forbidden(ForbiddenKind::Loopback),
    ),
    (
        v6([0x64, 0xff9b, 1, 0, 0, 0, 0, 0], 48),
        AddrClass::Private(PrivateKind::Nat64Local),
    ),
    (
        v6([0x100, 0, 0, 0, 0, 0, 0, 0], 64),
        AddrClass::Forbidden(ForbiddenKind::Reserved),
    ),
    (
        v6([0x2001, 0, 0, 0, 0, 0, 0, 0], 32),
        AddrClass::Forbidden(ForbiddenKind::Teredo),
    ),
    (
        v6([0x2001, 2, 0, 0, 0, 0, 0, 0], 48),
        AddrClass::Private(PrivateKind::Benchmarking),
    ),
    (
        v6([0x2001, 0xdb8, 0, 0, 0, 0, 0, 0], 32),
        AddrClass::Forbidden(ForbiddenKind::Documentation),
    ),
    (
        v6([0x3fff, 0, 0, 0, 0, 0, 0, 0], 20),
        AddrClass::Forbidden(ForbiddenKind::Documentation),
    ),
    (
        v6([0xfc00, 0, 0, 0, 0, 0, 0, 0], 7),
        AddrClass::Private(PrivateKind::UniqueLocal),
    ),
    (
        v6([0xfe80, 0, 0, 0, 0, 0, 0, 0], 10),
        AddrClass::Forbidden(ForbiddenKind::LinkLocal),
    ),
    (
        v6([0xfec0, 0, 0, 0, 0, 0, 0, 0], 10),
        AddrClass::Forbidden(ForbiddenKind::Reserved),
    ),
    (
        v6([0xff00, 0, 0, 0, 0, 0, 0, 0], 8),
        AddrClass::Forbidden(ForbiddenKind::Multicast),
    ),
];

/// IPv6 prefixes whose class is that of an embedded IPv4 address, with the
/// bit offset of that address.
#[derive(Clone, Copy)]
struct Embedding {
    range: Cidr,
    v4_offset: u32,
}

const EMBEDDINGS: [Embedding; 4] = [
    // ::ffff:0:0/96, IPv4-mapped.
    Embedding {
        range: v6([0, 0, 0, 0, 0, 0xffff, 0, 0], 96),
        v4_offset: 96,
    },
    // ::/96, IPv4-compatible (deprecated). :: and ::1 are matched first.
    Embedding {
        range: v6([0, 0, 0, 0, 0, 0, 0, 0], 96),
        v4_offset: 96,
    },
    // 64:ff9b::/96, well-known NAT64 prefix.
    Embedding {
        range: v6([0x64, 0xff9b, 0, 0, 0, 0, 0, 0], 96),
        v4_offset: 96,
    },
    // 2002::/16, 6to4: the IPv4 address is bits 16-47.
    Embedding {
        range: v6([0x2002, 0, 0, 0, 0, 0, 0, 0], 16),
        v4_offset: 16,
    },
];

fn classify_v4(ip: Ipv4Addr) -> AddrClass {
    let address = IpAddr::V4(ip);
    V4_TABLE
        .iter()
        .find(|(range, _)| range.contains(address))
        .map_or(AddrClass::Public, |(_, class)| *class)
}

fn v4_at(ip: Ipv6Addr, offset: u32) -> Ipv4Addr {
    let bits = u128::from(ip);
    Ipv4Addr::from(((bits >> (96 - offset)) & 0xffff_ffff) as u32)
}

/// The IPv4 address an ISATAP interface identifier carries, under any prefix.
fn isatap_ipv4(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    let segments = ip.segments();
    ((segments[4] == 0x0000 || segments[4] == 0x0200) && segments[5] == 0x5efe)
        .then(|| v4_at(ip, 96))
}

fn classify_v6(ip: Ipv6Addr) -> AddrClass {
    let address = IpAddr::V6(ip);
    let base = if let Some((_, class)) = V6_TABLE
        .iter()
        .take(2)
        .find(|(range, _)| range.contains(address))
    {
        // :: and ::1 precede the compatible range that contains them.
        *class
    } else if let Some(embedding) = EMBEDDINGS
        .iter()
        .find(|embedding| embedding.range.contains(address))
    {
        classify_v4(v4_at(ip, embedding.v4_offset))
    } else {
        V6_TABLE
            .iter()
            .skip(2)
            .find(|(range, _)| range.contains(address))
            .map_or(AddrClass::Public, |(_, class)| *class)
    };
    match isatap_ipv4(ip) {
        Some(embedded) => base.stricter(classify_v4(embedded)),
        None => base,
    }
}

/// Classify one address: the stricter of its own range and any IPv4 address
/// it embeds.
pub fn classify(ip: IpAddr) -> AddrClass {
    match ip {
        IpAddr::V4(ip) => classify_v4(ip),
        IpAddr::V6(ip) => classify_v6(ip),
    }
}

/// The IPv4 address embedded in an IPv6 address: mapped, compatible
/// (excluding `::` and `::1`), NAT64 `64:ff9b::/96`, 6to4, or an ISATAP
/// interface identifier, in that order.
pub fn embedded_ipv4(v6_address: Ipv6Addr) -> Option<Ipv4Addr> {
    let address = IpAddr::V6(v6_address);
    if V6_TABLE
        .iter()
        .take(2)
        .any(|(range, _)| range.contains(address))
    {
        return None;
    }
    EMBEDDINGS
        .iter()
        .find(|embedding| embedding.range.contains(address))
        .map(|embedding| v4_at(v6_address, embedding.v4_offset))
        .or_else(|| isatap_ipv4(v6_address))
}

/// An address prefix such as `10.0.0.0/8` or `fd00::/8`. Host bits must be
/// zero, so every value has exactly one spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Cidr {
    addr: IpAddr,
    prefix: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CidrError {
    MissingPrefix,
    BadAddress,
    BadPrefix,
    HostBitsSet { network: String },
}

impl fmt::Display for CidrError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingPrefix => formatter
                .write_str("a range needs a prefix length, such as 10.0.0.0/8 or 203.0.113.7/32"),
            Self::BadAddress => formatter.write_str(
                "the address is not a plain IPv4 address (four decimal parts) or an IPv6 address",
            ),
            Self::BadPrefix => {
                formatter.write_str("the prefix length must be 0-32 for IPv4 or 0-128 for IPv6")
            }
            Self::HostBitsSet { network } => {
                write!(
                    formatter,
                    "the address has bits set past the prefix; write {network}"
                )
            }
        }
    }
}

impl std::error::Error for CidrError {}

fn width(ip: IpAddr) -> u8 {
    match ip {
        IpAddr::V4(_) => 32,
        IpAddr::V6(_) => 128,
    }
}

fn bits(ip: IpAddr) -> u128 {
    match ip {
        IpAddr::V4(ip) => u128::from(u32::from(ip)),
        IpAddr::V6(ip) => u128::from(ip),
    }
}

fn mask(width: u8, prefix: u8) -> u128 {
    if prefix == 0 {
        return 0;
    }
    let all = if width == 32 {
        u128::from(u32::MAX)
    } else {
        u128::MAX
    };
    all & !all.checked_shr(u32::from(prefix)).unwrap_or(0)
}

fn from_bits(family: IpAddr, value: u128) -> IpAddr {
    match family {
        IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::from(value as u32)),
        IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::from(value)),
    }
}

impl Cidr {
    /// A prefix whose host bits are zero.
    pub fn new(addr: IpAddr, prefix: u8) -> Result<Self, CidrError> {
        if prefix > width(addr) {
            return Err(CidrError::BadPrefix);
        }
        let network = bits(addr) & mask(width(addr), prefix);
        if network != bits(addr) {
            return Err(CidrError::HostBitsSet {
                network: format!("{}/{prefix}", from_bits(addr, network)),
            });
        }
        Ok(Self { addr, prefix })
    }

    /// The single-address prefix for `ip`.
    pub fn host(ip: IpAddr) -> Self {
        Self {
            addr: ip,
            prefix: width(ip),
        }
    }

    pub fn addr(&self) -> IpAddr {
        self.addr
    }

    pub fn prefix(&self) -> u8 {
        self.prefix
    }

    pub fn is_ipv4(&self) -> bool {
        self.addr.is_ipv4()
    }

    /// Whether `ip` lies in this range. An IPv4-mapped IPv6 address is matched
    /// against IPv4 ranges by its embedded address.
    pub fn contains(&self, ip: IpAddr) -> bool {
        let ip = match (self.addr, ip) {
            (IpAddr::V4(_), IpAddr::V6(v6)) => match v6.to_ipv4_mapped() {
                Some(v4) => IpAddr::V4(v4),
                None => return false,
            },
            _ => ip,
        };
        if self.addr.is_ipv4() != ip.is_ipv4() {
            return false;
        }
        let mask = mask(width(ip), self.prefix);
        bits(ip) & mask == bits(self.addr)
    }

    /// Whether the two ranges share at least one address.
    pub fn overlaps(&self, other: &Cidr) -> bool {
        self.addr.is_ipv4() == other.addr.is_ipv4()
            && (self.contains(other.addr) || other.contains(self.addr))
    }

    /// Whether every address of this range lies in `other`.
    pub fn within(&self, other: &Cidr) -> bool {
        self.addr.is_ipv4() == other.addr.is_ipv4()
            && other.prefix <= self.prefix
            && other.contains(self.addr)
    }

    /// For an IPv6 range inside an embedding prefix, the IPv4 range it carries.
    fn embedded_v4_range(&self) -> Option<Cidr> {
        let IpAddr::V6(address) = self.addr else {
            return None;
        };
        if V6_TABLE.iter().take(2).any(|(range, _)| self.within(range)) {
            return None;
        }
        EMBEDDINGS
            .iter()
            .find(|embedding| self.within(&embedding.range))
            .map(|embedding| {
                let offset = embedding.v4_offset as u8;
                let v4_prefix = self.prefix.saturating_sub(offset).min(32);
                let start = v4_at(address, u32::from(offset));
                let network = u32::from(start) & (mask(32, v4_prefix) as u32);
                Cidr {
                    addr: IpAddr::V4(Ipv4Addr::from(network)),
                    prefix: v4_prefix,
                }
            })
    }
}

impl fmt::Display for Cidr {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.addr, self.prefix)
    }
}

impl FromStr for Cidr {
    type Err = CidrError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (address, prefix) = value.split_once('/').ok_or(CidrError::MissingPrefix)?;
        let addr = parse_ip_literal(address).ok_or(CidrError::BadAddress)?;
        if prefix.is_empty()
            || prefix.len() > 3
            || !prefix.bytes().all(|byte| byte.is_ascii_digit())
            || (prefix.len() > 1 && prefix.starts_with('0'))
        {
            return Err(CidrError::BadPrefix);
        }
        let prefix: u8 = prefix.parse().map_err(|_| CidrError::BadPrefix)?;
        Self::new(addr, prefix)
    }
}

impl serde::Serialize for Cidr {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for Cidr {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

/// Every Forbidden range as a prefix.
fn forbidden_ranges() -> impl Iterator<Item = Cidr> {
    V4_TABLE
        .iter()
        .chain(V6_TABLE.iter())
        .filter(|(_, class)| class.is_forbidden())
        .map(|(range, _)| *range)
}

/// Whether any address of `range` is Forbidden: it overlaps a Forbidden range
/// directly, or embeds an IPv4 range that does. ISATAP identifiers are not a
/// contiguous range; concrete addresses are still refused by [`classify`].
pub fn never_allowable(range: &Cidr) -> bool {
    if forbidden_ranges().any(|forbidden| forbidden.overlaps(range)) {
        return true;
    }
    if let Some(embedded) = range.embedded_v4_range() {
        return never_allowable(&embedded);
    }
    // A range wider than an embedding prefix contains embedded forms of every
    // IPv4 address, loopback included.
    EMBEDDINGS
        .iter()
        .any(|embedding| embedding.range.within(range))
}

/// The class shared by every address of `range`, when it has one. `None` when
/// the range mixes classes (`0.0.0.0/0`) or kinds of one class.
pub fn range_class(range: &Cidr) -> Option<AddrClass> {
    if let Some(embedded) = range.embedded_v4_range() {
        return range_class(&embedded);
    }
    let table: &[(Cidr, AddrClass)] = if range.is_ipv4() {
        &V4_TABLE
    } else {
        &V6_TABLE
    };
    if let Some((_, class)) = table.iter().find(|(candidate, _)| range.within(candidate)) {
        // The narrowest listed range containing it decides, but a narrower
        // differently-classed range inside it would make it mixed.
        let mixed = table.iter().any(|(candidate, other)| {
            other != class && candidate.overlaps(range) && !range.within(candidate)
        });
        return (!mixed).then_some(*class);
    }
    let touches_listed = table.iter().any(|(candidate, _)| candidate.overlaps(range));
    let touches_embedding = !range.is_ipv4()
        && EMBEDDINGS
            .iter()
            .any(|embedding| embedding.range.overlaps(range));
    (!touches_listed && !touches_embedding).then_some(AddrClass::Public)
}

/// Parse an IP literal strictly: four decimal octets without leading zeros,
/// or an IPv6 address bare or in brackets, without a zone id. Every other
/// numeric spelling (`2130706433`, `0x7f.1`, `127.1`, `010.0.0.1`) is `None`.
pub fn parse_ip_literal(host: &str) -> Option<IpAddr> {
    if let Some(inner) = host.strip_prefix('[') {
        let inner = inner.strip_suffix(']')?;
        return parse_ipv6(inner).map(IpAddr::V6);
    }
    if host.contains(':') {
        return parse_ipv6(host).map(IpAddr::V6);
    }
    parse_ipv4(host).map(IpAddr::V4)
}

fn parse_ipv6(value: &str) -> Option<Ipv6Addr> {
    if value.is_empty() || value.contains('%') || !value.is_ascii() {
        return None;
    }
    let address: Ipv6Addr = value.parse().ok()?;
    // An embedded dotted quad must itself be strict.
    if let Some((_, tail)) = value.rsplit_once(':') {
        if tail.contains('.') {
            parse_ipv4(tail)?;
        }
    }
    Some(address)
}

fn parse_ipv4(value: &str) -> Option<Ipv4Addr> {
    let mut octets = [0u8; 4];
    let mut parts = value.split('.');
    for octet in &mut octets {
        let part = parts.next()?;
        if part.is_empty()
            || part.len() > 3
            || !part.bytes().all(|byte| byte.is_ascii_digit())
            || (part.len() > 1 && part.starts_with('0'))
        {
            return None;
        }
        *octet = part.parse().ok()?;
    }
    parts.next().is_none().then(|| Ipv4Addr::from(octets))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostError {
    Empty,
    TooLong,
    BadLabel,
    NonAscii,
    Numeric,
    IpLiteral,
}

impl fmt::Display for HostError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Empty => "the host name is empty",
            Self::TooLong => "the host name is longer than 253 characters",
            Self::BadLabel => {
                "each part of a host name must be 1-63 letters, digits or hyphens, \
                 not starting or ending with a hyphen"
            }
            Self::NonAscii => "the host name is not ASCII; write it in punycode (xn--...)",
            Self::Numeric => {
                "the host name reads as a number, which resolvers may treat as an address"
            }
            Self::IpLiteral => "the host is an IP address, not a name",
        })
    }
}

impl std::error::Error for HostError {}

/// Lowercase a host name, strip one trailing dot, and check its syntax.
pub fn normalize_host_name(host: &str) -> Result<String, HostError> {
    if host.is_empty() {
        return Err(HostError::Empty);
    }
    if !host.is_ascii() {
        return Err(HostError::NonAscii);
    }
    if parse_ip_literal(host).is_some() {
        return Err(HostError::IpLiteral);
    }
    let lowered = host.to_ascii_lowercase();
    let name = lowered.strip_suffix('.').unwrap_or(&lowered);
    if name.is_empty() {
        return Err(HostError::Empty);
    }
    if name.len() > 253 {
        return Err(HostError::TooLong);
    }
    let mut last = "";
    for label in name.split('.') {
        let bytes = label.as_bytes();
        if bytes.is_empty()
            || bytes.len() > 63
            || bytes[0] == b'-'
            || bytes[bytes.len() - 1] == b'-'
            || !bytes
                .iter()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
        {
            return Err(HostError::BadLabel);
        }
        last = label;
    }
    let hex = last
        .strip_prefix("0x")
        .is_some_and(|digits| digits.bytes().all(|byte| byte.is_ascii_hexdigit()));
    if last.bytes().all(|byte| byte.is_ascii_digit()) || hex {
        return Err(HostError::Numeric);
    }
    Ok(name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(value: &str) -> IpAddr {
        value.parse().unwrap()
    }

    const PUBLIC: AddrClass = AddrClass::Public;
    const RFC1918: AddrClass = AddrClass::Private(PrivateKind::Rfc1918);
    const CGNAT: AddrClass = AddrClass::Private(PrivateKind::Cgnat);
    const ULA: AddrClass = AddrClass::Private(PrivateKind::UniqueLocal);
    const BENCH: AddrClass = AddrClass::Private(PrivateKind::Benchmarking);
    const IETF: AddrClass = AddrClass::Private(PrivateKind::IetfProtocol);
    const NAT64L: AddrClass = AddrClass::Private(PrivateKind::Nat64Local);
    const UNSPEC: AddrClass = AddrClass::Forbidden(ForbiddenKind::Unspecified);
    const LOOP: AddrClass = AddrClass::Forbidden(ForbiddenKind::Loopback);
    const LINK: AddrClass = AddrClass::Forbidden(ForbiddenKind::LinkLocal);
    const MCAST: AddrClass = AddrClass::Forbidden(ForbiddenKind::Multicast);
    const BCAST: AddrClass = AddrClass::Forbidden(ForbiddenKind::Broadcast);
    const RESV: AddrClass = AddrClass::Forbidden(ForbiddenKind::Reserved);
    const DOC: AddrClass = AddrClass::Forbidden(ForbiddenKind::Documentation);
    const TEREDO: AddrClass = AddrClass::Forbidden(ForbiddenKind::Teredo);

    #[test]
    fn classification_table() {
        let rows: &[(&str, AddrClass)] = &[
            // IPv4 table, both edges of each range where it matters.
            ("0.0.0.0", UNSPEC),
            ("0.255.255.255", UNSPEC),
            ("1.1.1.1", PUBLIC),
            ("8.8.8.8", PUBLIC),
            ("9.255.255.255", PUBLIC),
            ("10.0.0.0", RFC1918),
            ("10.255.255.255", RFC1918),
            ("11.0.0.0", PUBLIC),
            ("100.63.255.255", PUBLIC),
            ("100.64.0.0", CGNAT),
            ("100.127.255.255", CGNAT),
            ("100.128.0.0", PUBLIC),
            ("126.255.255.255", PUBLIC),
            ("127.0.0.1", LOOP),
            ("127.255.255.254", LOOP),
            ("128.0.0.0", PUBLIC),
            ("169.253.255.255", PUBLIC),
            ("169.254.0.1", LINK),
            ("169.254.169.254", LINK),
            ("169.255.0.0", PUBLIC),
            ("172.15.255.255", PUBLIC),
            ("172.16.0.0", RFC1918),
            ("172.31.255.255", RFC1918),
            ("172.32.0.0", PUBLIC),
            ("192.0.0.0", IETF),
            ("192.0.0.170", IETF),
            ("192.0.1.0", PUBLIC),
            ("192.0.2.1", DOC),
            ("192.88.99.1", RESV),
            ("192.88.100.1", PUBLIC),
            ("192.167.255.255", PUBLIC),
            ("192.168.0.1", RFC1918),
            ("192.168.127.254", RFC1918),
            ("192.169.0.0", PUBLIC),
            ("198.17.255.255", PUBLIC),
            ("198.18.0.0", BENCH),
            ("198.19.255.255", BENCH),
            ("198.20.0.0", PUBLIC),
            ("198.51.100.7", DOC),
            ("203.0.113.9", DOC),
            ("203.0.114.0", PUBLIC),
            ("223.255.255.255", PUBLIC),
            ("224.0.0.1", MCAST),
            ("239.255.255.255", MCAST),
            ("240.0.0.1", RESV),
            ("254.255.255.255", RESV),
            ("255.255.255.254", RESV),
            ("255.255.255.255", BCAST),
            // IPv6 table.
            ("::", UNSPEC),
            ("::1", LOOP),
            ("2606:4700::1", PUBLIC),
            ("2a00:1450:4001::200e", PUBLIC),
            ("100::1", RESV),
            ("100::ffff:ffff:ffff:ffff", RESV),
            ("100:0:0:1::", PUBLIC),
            ("2001::1", TEREDO),
            ("2001:0:ffff::1", TEREDO),
            ("2001:2::1", BENCH),
            ("2001:db8::1", DOC),
            ("2001:db9::1", PUBLIC),
            ("3fff::1", DOC),
            ("fc00::1", ULA),
            ("fd12:3456::1", ULA),
            ("fe80::1", LINK),
            ("febf::1", LINK),
            ("fec0::1", RESV),
            ("ff02::1", MCAST),
            ("64:ff9b:1::1", NAT64L),
            // Embedded IPv4 forms.
            ("::ffff:127.0.0.1", LOOP),
            ("::ffff:7f00:1", LOOP),
            ("::ffff:8.8.8.8", PUBLIC),
            ("::ffff:10.1.2.3", RFC1918),
            ("::ffff:169.254.169.254", LINK),
            ("::127.0.0.1", LOOP),
            ("::7f00:1", LOOP),
            ("::8.8.8.8", PUBLIC),
            ("::0.0.0.2", UNSPEC),
            ("64:ff9b::a9fe:a9fe", LINK),
            ("64:ff9b::8.8.8.8", PUBLIC),
            ("64:ff9b::10.0.0.1", RFC1918),
            ("2002:7f00:1::", LOOP),
            ("2002:0808:0808::1", PUBLIC),
            ("2002:c0a8:0101::1", RFC1918),
            ("2002:a9fe:a9fe:1::", LINK),
            ("fe80::5efe:7f00:1", LINK),
            ("2606:4700::5efe:7f00:1", LOOP),
            ("2606:4700::200:5efe:a9fe:a9fe", LINK),
            ("fd00::5efe:808:808", ULA),
            ("fd00::5efe:7f00:1", LOOP),
            ("2606:4700::5efe:808:808", PUBLIC),
            ("2606:4700::300:5efe:7f00:1", PUBLIC),
        ];
        assert!(rows.len() >= 80, "{} rows", rows.len());
        for (address, expected) in rows {
            assert_eq!(classify(ip(address)), *expected, "{address}");
        }
    }

    #[test]
    fn embedded_ipv4_forms() {
        let rows: &[(&str, Option<&str>)] = &[
            ("::ffff:127.0.0.1", Some("127.0.0.1")),
            ("::127.0.0.1", Some("127.0.0.1")),
            ("64:ff9b::a9fe:a9fe", Some("169.254.169.254")),
            ("2002:7f00:1::", Some("127.0.0.1")),
            ("fe80::5efe:7f00:1", Some("127.0.0.1")),
            ("fe80::200:5efe:a00:1", Some("10.0.0.1")),
            ("::", None),
            ("::1", None),
            ("2606:4700::1", None),
            ("64:ff9b:1::1", None),
        ];
        for (address, expected) in rows {
            let IpAddr::V6(address) = ip(address) else {
                unreachable!()
            };
            assert_eq!(
                embedded_ipv4(address),
                expected.map(|value| value.parse().unwrap()),
                "{address}"
            );
        }
    }

    #[test]
    fn strict_ip_literals() {
        let accepted: &[(&str, &str)] = &[
            ("1.2.3.4", "1.2.3.4"),
            ("0.0.0.0", "0.0.0.0"),
            ("255.255.255.255", "255.255.255.255"),
            ("::1", "::1"),
            ("[::1]", "::1"),
            ("[2606:4700::1]", "2606:4700::1"),
            ("::ffff:127.0.0.1", "::ffff:127.0.0.1"),
        ];
        for (literal, expected) in accepted {
            assert_eq!(parse_ip_literal(literal), Some(ip(expected)), "{literal}");
        }
        for refused in [
            "2130706433",
            "0x7f.1",
            "0x7f000001",
            "127.1",
            "127.0.1",
            "010.0.0.1",
            "1.2.3.04",
            "1.2.3.256",
            "1.2.3.4.5",
            "1.2.3.",
            ".1.2.3",
            "1..2.3",
            "+1.2.3.4",
            "1.2.3.4 ",
            "fe80::1%eth0",
            "[fe80::1%25eth0]",
            "[::1",
            "::1]",
            "[]",
            "",
            "::ffff:127.0.0.01",
            "example.com",
            "١.٢.٣.٤",
        ] {
            assert_eq!(parse_ip_literal(refused), None, "{refused}");
        }
    }

    #[test]
    fn host_names() {
        let accepted: &[(&str, &str)] = &[
            ("Registry.NPMJS.org", "registry.npmjs.org"),
            ("example.com.", "example.com"),
            ("xn--bcher-kva.example", "xn--bcher-kva.example"),
            ("a-b.c0", "a-b.c0"),
            ("localhost", "localhost"),
            ("1password.com", "1password.com"),
            ("123.example.com", "123.example.com"),
        ];
        for (host, expected) in accepted {
            assert_eq!(
                normalize_host_name(host).as_deref(),
                Ok(*expected),
                "{host}"
            );
        }
        let long_label = "a".repeat(64);
        let long_name = format!("{}.com", vec!["a".repeat(63); 4].join("."));
        let refused: &[(&str, HostError)] = &[
            ("", HostError::Empty),
            (".", HostError::Empty),
            ("bücher.example", HostError::NonAscii),
            ("2130706433", HostError::Numeric),
            ("0x7f.1", HostError::Numeric),
            ("127.1", HostError::Numeric),
            ("010.0.0.1", HostError::Numeric),
            ("0x7f000001", HostError::Numeric),
            ("example.123", HostError::Numeric),
            ("1.2.3.4", HostError::IpLiteral),
            ("::1", HostError::IpLiteral),
            ("[::1]", HostError::IpLiteral),
            ("under_score.example", HostError::BadLabel),
            ("-lead.example", HostError::BadLabel),
            ("trail-.example", HostError::BadLabel),
            ("a..b", HostError::BadLabel),
            (".a.b", HostError::BadLabel),
            ("example.com..", HostError::BadLabel),
            ("*.example.com", HostError::BadLabel),
            ("host:443", HostError::BadLabel),
            ("a b.example", HostError::BadLabel),
            (long_label.as_str(), HostError::BadLabel),
            (long_name.as_str(), HostError::TooLong),
        ];
        for (host, expected) in refused {
            assert_eq!(normalize_host_name(host), Err(*expected), "{host}");
        }
    }

    #[test]
    fn cidr_parsing_and_display() {
        for (text, display) in [
            ("10.0.0.0/8", "10.0.0.0/8"),
            ("fd00::/8", "fd00::/8"),
            ("203.0.113.7/32", "203.0.113.7/32"),
            ("0.0.0.0/0", "0.0.0.0/0"),
            ("::/0", "::/0"),
        ] {
            let cidr: Cidr = text.parse().unwrap();
            assert_eq!(cidr.to_string(), display);
        }
        assert_eq!("10.0.0.0".parse::<Cidr>(), Err(CidrError::MissingPrefix));
        assert_eq!("10.0.0/8".parse::<Cidr>(), Err(CidrError::BadAddress));
        assert_eq!("10.0.0.0/33".parse::<Cidr>(), Err(CidrError::BadPrefix));
        assert_eq!("::/129".parse::<Cidr>(), Err(CidrError::BadPrefix));
        assert_eq!("10.0.0.0/08".parse::<Cidr>(), Err(CidrError::BadPrefix));
        assert_eq!("10.0.0.0/".parse::<Cidr>(), Err(CidrError::BadPrefix));
        assert_eq!(
            "10.0.0.1/8".parse::<Cidr>(),
            Err(CidrError::HostBitsSet {
                network: "10.0.0.0/8".into()
            })
        );
        let cidr: Cidr = serde_json::from_str("\"192.168.0.0/16\"").unwrap();
        assert_eq!(serde_json::to_string(&cidr).unwrap(), "\"192.168.0.0/16\"");
        assert!(serde_json::from_str::<Cidr>("\"192.168.0.0\"").is_err());
    }

    #[test]
    fn cidr_containment_and_overlap() {
        let c = |text: &str| text.parse::<Cidr>().unwrap();
        assert!(c("10.0.0.0/8").contains(ip("10.200.1.1")));
        assert!(!c("10.0.0.0/8").contains(ip("11.0.0.0")));
        assert!(c("10.0.0.0/8").contains(ip("::ffff:10.0.0.1")));
        assert!(!c("10.0.0.0/8").contains(ip("::10.0.0.1")));
        assert!(!c("fd00::/8").contains(ip("10.0.0.1")));
        assert!(c("0.0.0.0/0").contains(ip("255.255.255.255")));
        assert!(c("::/0").contains(ip("2606:4700::1")));
        assert!(c("203.0.113.7/32").contains(ip("203.0.113.7")));
        assert!(!c("203.0.113.7/32").contains(ip("203.0.113.8")));

        assert!(c("10.0.0.0/8").overlaps(&c("10.1.0.0/16")));
        assert!(c("10.1.0.0/16").overlaps(&c("10.0.0.0/8")));
        assert!(!c("10.0.0.0/8").overlaps(&c("11.0.0.0/8")));
        assert!(!c("10.0.0.0/8").overlaps(&c("fd00::/8")));
        assert!(c("10.1.0.0/16").within(&c("10.0.0.0/8")));
        assert!(!c("10.0.0.0/8").within(&c("10.1.0.0/16")));
        assert!(c("10.0.0.0/8").within(&c("10.0.0.0/8")));
        assert!(!c("10.0.0.0/8").within(&c("::/0")));
        assert!(c("fd00:1::/32").within(&c("fc00::/7")));
    }

    #[test]
    fn never_allowable_ranges() {
        let c = |text: &str| text.parse::<Cidr>().unwrap();
        for forbidden in [
            "127.0.0.0/8",
            "127.0.0.1/32",
            "169.254.0.0/16",
            "0.0.0.0/0",
            "0.0.0.0/8",
            "224.0.0.0/4",
            "192.0.2.0/24",
            "255.255.255.255/32",
            "::1/128",
            "::/0",
            "fe80::/10",
            "ff00::/8",
            "2001::/32",
            "::ffff:127.0.0.0/104",
            "::ffff:0:0/96",
            "64:ff9b::a9fe:0/112",
            "2002:7f00::/24",
            "::/96",
            "fc00::/6",
        ] {
            assert!(never_allowable(&c(forbidden)), "{forbidden}");
        }
        for allowable in [
            "10.0.0.0/8",
            "192.168.0.0/16",
            "100.64.0.0/10",
            "8.8.8.0/24",
            "fd00::/8",
            "fc00::/7",
            "::ffff:10.0.0.0/104",
            "64:ff9b::a00:0/104",
            "2002:c0a8::/32",
        ] {
            assert!(!never_allowable(&c(allowable)), "{allowable}");
        }
    }

    #[test]
    fn whole_range_classes() {
        let c = |text: &str| text.parse::<Cidr>().unwrap();
        assert_eq!(range_class(&c("10.1.0.0/16")), Some(RFC1918));
        assert_eq!(range_class(&c("10.0.0.0/8")), Some(RFC1918));
        assert_eq!(range_class(&c("192.168.4.0/24")), Some(RFC1918));
        assert_eq!(range_class(&c("fd00::/8")), Some(ULA));
        assert_eq!(range_class(&c("::ffff:10.0.0.0/104")), Some(RFC1918));
        assert_eq!(range_class(&c("8.8.8.0/24")), Some(PUBLIC));
        assert_eq!(range_class(&c("2606:4700::/32")), Some(PUBLIC));
        assert_eq!(range_class(&c("0.0.0.0/0")), None);
        assert_eq!(range_class(&c("8.0.0.0/5")), None);
        assert_eq!(range_class(&c("127.0.0.0/8")), Some(LOOP));
        assert_eq!(range_class(&c("::/0")), None);
        assert_eq!(range_class(&c("2000::/3")), None);
    }

    #[test]
    fn stricter_keeps_the_receiver_on_a_tie() {
        assert_eq!(PUBLIC.stricter(RFC1918), RFC1918);
        assert_eq!(RFC1918.stricter(LOOP), LOOP);
        assert_eq!(LOOP.stricter(RFC1918), LOOP);
        assert_eq!(LINK.stricter(LOOP), LINK);
        assert_eq!(RFC1918.stricter(CGNAT), RFC1918);
    }
}
