//! Validation shared by `sandbox.egress` and `browser.allow`, plus the
//! network cross-field rules.

use std::collections::HashSet;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr};

use axocoatl_core::netaddr::{self, AddrClass, Cidr};

use crate::egress_presets::{self, preset, preset_names, PRESETS};
use crate::error::ConfigError;
use crate::types::{AxocoatlConfig, EgressAllowYaml, EgressConfigYaml};

/// Most entries one allowlist or `private_destinations` list may hold.
pub const MAX_ALLOW_ENTRIES: usize = 256;
/// Most ports one entry may list.
pub const MAX_ENTRY_PORTS: usize = 64;
/// Ports an entry gets when it lists none.
pub const DEFAULT_ALLOW_PORTS: [u16; 1] = [443];
pub const MIN_MAX_CONNECTIONS: u32 = 8;
pub const MAX_MAX_CONNECTIONS: u32 = 256;
pub const MIN_RECORD_EVENTS: u32 = 1_000;
pub const MAX_RECORD_EVENTS: u32 = 1_000_000;

/// Addresses that lead from a Podman container to the computer running
/// Podman: gvproxy's gateway and host-loopback addresses in a Podman machine,
/// and the gateway of Podman's default network. `network: egress` refuses
/// them whatever the policy lists; the gateway of a configured
/// `sidecar_network` is looked up when the sidecar starts.
pub const HOST_GATEWAYS: [Ipv4Addr; 3] = [
    Ipv4Addr::new(192, 168, 127, 1),
    Ipv4Addr::new(192, 168, 127, 254),
    Ipv4Addr::new(10, 88, 0, 1),
];

const GATEWAY_WARNING: &str = "a Podman host gateway, which leads to services on this computer; Axocoatl refuses it whatever this list says";

fn host_gateway_in(range: &Cidr) -> Option<Ipv4Addr> {
    HOST_GATEWAYS
        .iter()
        .copied()
        .find(|gateway| range.contains(IpAddr::V4(*gateway)))
}

const WILDCARD_WARNING: &str =
    "a wildcard allows every subdomain, including ones created later by whoever controls the domain";
const CDN_WARNING: &str =
    "a CDN-fronted host can reach other sites on the same CDN through an opaque tunnel";
const WRITE_WARNING: &str = "accepts uploads; data can leave through it";

/// A configuration problem worth reporting that does not stop the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigWarning {
    pub field: String,
    pub message: String,
}

impl fmt::Display for ConfigWarning {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.field, self.message)
    }
}

fn invalid(
    field: String,
    value: impl fmt::Debug,
    reason: impl Into<String>,
    suggestion: impl Into<String>,
) -> ConfigError {
    ConfigError::InvalidField {
        field,
        value: format!("{value:?}"),
        reason: reason.into(),
        suggestion: suggestion.into(),
    }
}

/// The validated form of one host entry: an exact name, or a wildcard suffix
/// (`*.example.com` stored as `example.com`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum HostPattern {
    Exact(String),
    Subdomains(String),
}

impl HostPattern {
    pub fn matches(&self, host: &str) -> bool {
        match self {
            Self::Exact(name) => name == host,
            Self::Subdomains(suffix) => host
                .strip_suffix(suffix.as_str())
                .is_some_and(|prefix| prefix.len() > 1 && prefix.ends_with('.')),
        }
    }
}

impl fmt::Display for HostPattern {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exact(name) => formatter.write_str(name),
            Self::Subdomains(suffix) => write!(formatter, "*.{suffix}"),
        }
    }
}

/// Parse one allowlist host. A wildcard is allowed only as a leftmost `*.`
/// followed by at least two labels. IP literals belong under `cidr`.
pub fn parse_host_pattern(host: &str) -> Result<HostPattern, String> {
    if netaddr::parse_ip_literal(host).is_some() {
        return Err(format!(
            "{host} is an IP address; list it as {{cidr: {host}/32}} (or /128 for IPv6) instead"
        ));
    }
    if let Some(suffix) = host.strip_prefix("*.") {
        if suffix.contains('*') {
            return Err("a wildcard may appear only once, as the leftmost \"*.\"".into());
        }
        let name = netaddr::normalize_host_name(suffix).map_err(|error| error.to_string())?;
        if name.split('.').count() < 2 {
            return Err(format!(
                "*.{name} would cover a whole top-level domain; a wildcard needs at least two labels after \"*.\""
            ));
        }
        return Ok(HostPattern::Subdomains(name));
    }
    if host.contains('*') {
        return Err("a wildcard is allowed only as a leftmost \"*.\" followed by at least two labels, such as *.example.com".into());
    }
    netaddr::normalize_host_name(host)
        .map(HostPattern::Exact)
        .map_err(|error| error.to_string())
}

/// Check one entry's ports. `None` means the default `[443]`.
pub fn validate_ports(ports: Option<&[u16]>) -> Result<Vec<u16>, String> {
    let Some(ports) = ports else {
        return Ok(DEFAULT_ALLOW_PORTS.to_vec());
    };
    if ports.is_empty() {
        return Err("ports cannot be empty; omit it for the default [443]".into());
    }
    if ports.len() > MAX_ENTRY_PORTS {
        return Err(format!("at most {MAX_ENTRY_PORTS} ports per entry"));
    }
    let mut seen = HashSet::new();
    for port in ports {
        if *port == 0 {
            return Err("ports must be 1-65535".into());
        }
        if !seen.insert(*port) {
            return Err(format!("port {port} is listed twice"));
        }
    }
    Ok(ports.to_vec())
}

/// Parse and check `private_destinations`: each entry must be a range whose
/// every address is Private, and none may touch a range Axocoatl never allows.
pub fn parse_private_destinations(
    field: &str,
    private: &[String],
) -> Result<Vec<Cidr>, ConfigError> {
    let field = format!("{field}.private_destinations");
    if private.len() > MAX_ALLOW_ENTRIES {
        return Err(invalid(
            field,
            private.len(),
            format!("at most {MAX_ALLOW_ENTRIES} ranges"),
            "List fewer, wider private ranges.",
        ));
    }
    private
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let field = format!("{field}[{index}]");
            let cidr: Cidr = entry.parse().map_err(|error: netaddr::CidrError| {
                invalid(field.clone(), entry, error.to_string(), "Write a range such as 10.0.0.0/8 or fd00::/8.")
            })?;
            if netaddr::never_allowable(&cidr) {
                return Err(invalid(
                    field,
                    entry,
                    "the range includes loopback, link-local, multicast, documentation or other addresses Axocoatl never connects to",
                    "List only private ranges such as 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16, 100.64.0.0/10 or fd00::/8.",
                ));
            }
            if !matches!(netaddr::range_class(&cidr), Some(AddrClass::Private(_))) {
                return Err(invalid(
                    field,
                    entry,
                    "every address of a private destination must be private; public addresses are allowed through allow entries",
                    "Narrow the range to a private one such as 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16, 100.64.0.0/10 or fd00::/8.",
                ));
            }
            Ok(cidr)
        })
        .collect()
}

/// Validate one allowlist and its private ranges. Used for `sandbox.egress`
/// and `browser`. Returns warnings for entries that weaken the policy.
pub fn validate_allow_list(
    field: &str,
    allow: &[EgressAllowYaml],
    private: &[String],
) -> Result<Vec<ConfigWarning>, ConfigError> {
    let private_entries = private;
    let private = parse_private_destinations(field, private)?;
    let mut warnings = Vec::new();
    for (index, (range, entry)) in private.iter().zip(private_entries).enumerate() {
        if let Some(gateway) = host_gateway_in(range) {
            warnings.push(ConfigWarning {
                field: format!("{field}.private_destinations[{index}]"),
                message: format!("{entry} includes {gateway}, {GATEWAY_WARNING}"),
            });
        }
    }
    if allow.len() > MAX_ALLOW_ENTRIES {
        return Err(invalid(
            format!("{field}.allow"),
            allow.len(),
            format!("at most {MAX_ALLOW_ENTRIES} entries"),
            "Use presets or wildcard entries to shorten the list.",
        ));
    }
    let write_hosts: HashSet<&str> = PRESETS
        .iter()
        .filter(|preset| preset.write_capable)
        .flat_map(|preset| preset.hosts.iter().map(|(host, _)| *host))
        .collect();
    for (index, entry) in allow.iter().enumerate() {
        let entry_field = format!("{field}.allow[{index}]");
        match entry {
            EgressAllowYaml::Preset(name) => {
                let Some(found) = preset(name) else {
                    return Err(invalid(
                        entry_field,
                        name,
                        "unknown preset",
                        format!(
                            "Use one of: {}, or write {{host: <name>, ports: [443]}}.",
                            preset_names().join(", ")
                        ),
                    ));
                };
                if found.cdn_fronted {
                    warnings.push(ConfigWarning {
                        field: entry_field.clone(),
                        message: format!("preset {name}: {CDN_WARNING}"),
                    });
                }
                if found.write_capable {
                    warnings.push(ConfigWarning {
                        field: entry_field,
                        message: format!("preset {name} {WRITE_WARNING}"),
                    });
                }
            }
            EgressAllowYaml::Host(rule) => {
                let pattern = parse_host_pattern(&rule.host).map_err(|reason| {
                    invalid(
                        format!("{entry_field}.host"),
                        &rule.host,
                        reason,
                        "Write a host name such as registry.example.com, or *.example.com for its subdomains.",
                    )
                })?;
                validate_ports(rule.ports.as_deref()).map_err(|reason| {
                    invalid(
                        format!("{entry_field}.ports"),
                        &rule.ports,
                        reason,
                        "List ports 1-65535 once each, such as [443].",
                    )
                })?;
                if matches!(pattern, HostPattern::Subdomains(_)) {
                    warnings.push(ConfigWarning {
                        field: entry_field.clone(),
                        message: format!("{pattern}: {WILDCARD_WARNING}"),
                    });
                }
                if write_hosts.iter().any(|host| pattern.matches(host)) {
                    warnings.push(ConfigWarning {
                        field: entry_field,
                        message: format!("{pattern} {WRITE_WARNING}"),
                    });
                }
            }
            EgressAllowYaml::Cidr(rule) => {
                let cidr: Cidr = rule.cidr.parse().map_err(|error: netaddr::CidrError| {
                    invalid(
                        format!("{entry_field}.cidr"),
                        &rule.cidr,
                        error.to_string(),
                        "Write a range such as 203.0.113.0/24, or 203.0.113.7/32 for one address.",
                    )
                })?;
                if netaddr::never_allowable(&cidr) {
                    return Err(invalid(
                        format!("{entry_field}.cidr"),
                        &rule.cidr,
                        "the range includes loopback, link-local, multicast, documentation or other addresses Axocoatl never connects to",
                        "Allow a narrower public or private range.",
                    ));
                }
                if netaddr::range_class(&cidr) != Some(AddrClass::Public)
                    && !private.iter().any(|range| cidr.within(range))
                {
                    return Err(invalid(
                        format!("{entry_field}.cidr"),
                        &rule.cidr,
                        "the range includes private addresses that are not inside a private_destinations entry",
                        format!("Add a range containing {cidr} under {field}.private_destinations."),
                    ));
                }
                validate_ports(rule.ports.as_deref()).map_err(|reason| {
                    invalid(
                        format!("{entry_field}.ports"),
                        &rule.ports,
                        reason,
                        "List ports 1-65535 once each, such as [443].",
                    )
                })?;
                if let Some(gateway) = host_gateway_in(&cidr) {
                    warnings.push(ConfigWarning {
                        field: format!("{entry_field}.cidr"),
                        message: format!("{} includes {gateway}, {GATEWAY_WARNING}", rule.cidr),
                    });
                }
            }
        }
    }
    Ok(warnings)
}

/// A Podman network name for the egress sidecar.
pub fn valid_network_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
}

fn validate_egress_block(egress: &EgressConfigYaml) -> Result<Vec<ConfigWarning>, ConfigError> {
    let warnings = validate_allow_list(
        "sandbox.egress",
        &egress.allow,
        &egress.private_destinations,
    )?;
    if !(MIN_MAX_CONNECTIONS..=MAX_MAX_CONNECTIONS).contains(&egress.max_connections) {
        return Err(invalid(
            "sandbox.egress.max_connections".into(),
            egress.max_connections,
            format!("must be {MIN_MAX_CONNECTIONS}-{MAX_MAX_CONNECTIONS}"),
            "Omit it for the default 128.",
        ));
    }
    if !(MIN_RECORD_EVENTS..=MAX_RECORD_EVENTS).contains(&egress.record_max_events) {
        return Err(invalid(
            "sandbox.egress.record_max_events".into(),
            egress.record_max_events,
            format!("must be {MIN_RECORD_EVENTS}-{MAX_RECORD_EVENTS}"),
            "Omit it for the default 50000.",
        ));
    }
    if let Some(network) = &egress.sidecar_network {
        if !valid_network_name(network) {
            return Err(invalid(
                "sandbox.egress.sidecar_network".into(),
                network,
                "a Podman network name is 1-64 letters, digits, '_', '.' or '-', starting with a letter or digit",
                "Omit it to use Podman's default network.",
            ));
        }
    }
    Ok(warnings)
}

/// The warning for `sandbox.egress` fields that do nothing under `bridge`
/// or `none`. `allow`, `private_destinations` and `routes` apply only to
/// `network: egress`. The browser's own proxy also reads `sidecar_network`
/// and `max_connections` in those modes, and `record_max_events` caps the
/// Session network record, which web and browser events use, in every mode.
fn unused_egress_fields(
    egress: &EgressConfigYaml,
    browser: bool,
    network: &str,
) -> Option<ConfigWarning> {
    let mut unused = Vec::new();
    if !egress.allow.is_empty() {
        unused.push("allow");
    }
    if !egress.private_destinations.is_empty() {
        unused.push("private_destinations");
    }
    if !egress.routes.is_empty() {
        unused.push("routes");
    }
    if !browser {
        if egress.sidecar_network.is_some() {
            unused.push("sidecar_network");
        }
        if egress.max_connections != EgressConfigYaml::default().max_connections {
            unused.push("max_connections");
        }
    }
    if unused.is_empty() {
        return None;
    }
    Some(ConfigWarning {
        field: "sandbox.egress".into(),
        message: format!(
            "{} ignored because sandbox.network is {network:?}: allow, private_destinations and \
             routes apply only to network: egress, and sidecar_network and max_connections only \
             to it and the browser's own proxy",
            unused.join(", ")
        ),
    })
}

/// Validate `sandbox.egress` and the network cross-field rules.
pub fn validate_egress(config: &AxocoatlConfig) -> Result<Vec<ConfigWarning>, ConfigError> {
    let sandbox = &config.sandbox;
    let mut warnings = Vec::new();
    if let Some(egress) = &sandbox.egress {
        warnings.extend(validate_egress_block(egress)?);
        if sandbox.network != "egress" {
            warnings.extend(unused_egress_fields(
                egress,
                config.browser.is_some(),
                &sandbox.network,
            ));
        }
    }
    if sandbox.backend == "e2b" && sandbox.network != "bridge" {
        return Err(invalid(
            "sandbox.network".into(),
            &sandbox.network,
            "the E2B backend cannot enforce a container network policy; only network: bridge is supported with backend: e2b",
            "Use backend: podman for network: none or egress, or set network: bridge.",
        ));
    }
    if let Some(browser) = &config.browser {
        if sandbox.backend == "e2b" {
            return Err(invalid(
                "browser".into(),
                "e2b",
                "the browser tool runs in a local Podman container and is not available with backend: e2b",
                "Remove the browser block or use backend: podman.",
            ));
        }
        if sandbox.network == "none" && !browser.allow.is_empty() {
            return Err(invalid(
                "browser.allow".into(),
                browser.allow.len(),
                "network: none gives the browser only the app under test",
                "Remove browser.allow, or use network: bridge or egress.",
            ));
        }
    }
    Ok(warnings)
}

/// Every network-related warning for a config that already validated:
/// egress, routes and credentials, web and browser settings, and MCP
/// environment inheritance.
pub fn network_warnings(config: &AxocoatlConfig) -> Vec<ConfigWarning> {
    let mut warnings = validate_egress(config).unwrap_or_default();
    warnings.extend(crate::egress_routes::validate_egress_routes(config).unwrap_or_default());
    warnings.extend(crate::web::validate_web(config).unwrap_or_default());
    warnings.extend(crate::browser::validate_browser(config).unwrap_or_default());
    warnings
}

/// Short summary of an allowlist for `doctor`: preset names, then a count of
/// other hosts and ranges.
pub fn allow_summary(allow: &[EgressAllowYaml]) -> String {
    let presets: Vec<&str> = allow
        .iter()
        .filter_map(|entry| match entry {
            EgressAllowYaml::Preset(name) => Some(name.as_str()),
            _ => None,
        })
        .collect();
    let hosts = allow
        .iter()
        .filter(|entry| matches!(entry, EgressAllowYaml::Host(_)))
        .count();
    let ranges = allow
        .iter()
        .filter(|entry| matches!(entry, EgressAllowYaml::Cidr(_)))
        .count();
    let mut parts: Vec<String> = presets.iter().map(|name| (*name).to_string()).collect();
    if hosts > 0 {
        parts.push(format!("{hosts} host{}", if hosts == 1 { "" } else { "s" }));
    }
    if ranges > 0 {
        parts.push(format!(
            "{ranges} range{}",
            if ranges == 1 { "" } else { "s" }
        ));
    }
    if parts.is_empty() {
        "nothing".into()
    } else {
        parts.join(", ")
    }
}

// Keep the preset module reachable through this one for callers that only
// import `egress`.
pub use egress_presets::{Preset, DISTRO_PRESETS};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{EgressCidrYaml, EgressHostYaml};

    fn host(name: &str, ports: Option<Vec<u16>>) -> EgressAllowYaml {
        EgressAllowYaml::Host(EgressHostYaml {
            host: name.into(),
            ports,
        })
    }

    fn cidr(range: &str, ports: Option<Vec<u16>>) -> EgressAllowYaml {
        EgressAllowYaml::Cidr(EgressCidrYaml {
            cidr: range.into(),
            ports,
        })
    }

    fn reason(error: ConfigError) -> String {
        error.to_string()
    }

    #[test]
    fn yaml_entries_parse_into_their_variants() {
        let allow: Vec<EgressAllowYaml> = serde_yaml::from_str(
            "- npm\n- host: api.example.com\n- host: '*.example.org'\n  ports: [80, 443]\n- cidr: 203.0.113.0/24\n",
        )
        .unwrap();
        assert_eq!(
            allow,
            vec![
                EgressAllowYaml::Preset("npm".into()),
                host("api.example.com", None),
                host("*.example.org", Some(vec![80, 443])),
                cidr("203.0.113.0/24", None),
            ]
        );
        let error =
            serde_yaml::from_str::<Vec<EgressAllowYaml>>("- host: a.example\n  port: 443\n")
                .unwrap_err()
                .to_string();
        assert!(error.contains("a preset name such as npm"), "{error}");
        assert!(serde_yaml::from_str::<Vec<EgressAllowYaml>>(
            "- {host: a.example, cidr: 10.0.0.0/8}\n"
        )
        .is_err());
    }

    #[test]
    fn ranges_that_include_a_host_gateway_are_named() {
        let warnings = validate_allow_list(
            "sandbox.egress",
            &[
                cidr("192.168.127.0/24", Some(vec![8080])),
                cidr("10.20.0.0/16", Some(vec![8000])),
            ],
            &[
                "192.168.0.0/16".into(),
                "10.0.0.0/8".into(),
                "172.16.0.0/12".into(),
            ],
        )
        .unwrap();
        let text: Vec<String> = warnings.iter().map(ToString::to_string).collect();
        let named = |field: &str, gateway: &str| {
            text.iter().any(|warning| {
                warning.starts_with(field)
                    && warning.contains(gateway)
                    && warning.contains("Podman host gateway")
            })
        };
        assert!(
            named("sandbox.egress.private_destinations[0]", "192.168.127.1"),
            "{text:?}"
        );
        assert!(
            named("sandbox.egress.private_destinations[1]", "10.88.0.1"),
            "{text:?}"
        );
        assert!(
            named("sandbox.egress.allow[0].cidr", "192.168.127.1"),
            "{text:?}"
        );
        assert!(
            !text.iter().any(|w| w.contains("private_destinations[2]")),
            "{text:?}"
        );
        assert!(!text.iter().any(|w| w.contains("allow[1]")), "{text:?}");
    }

    #[test]
    fn accepted_lists_and_their_warnings() {
        let warnings = validate_allow_list(
            "sandbox.egress",
            &[
                EgressAllowYaml::Preset("npm".into()),
                EgressAllowYaml::Preset("ubuntu".into()),
                host("api.example.com", None),
                host("*.example.org", Some(vec![80, 443])),
                host("github.com", None),
                cidr("8.8.8.0/24", Some(vec![53])),
                cidr("10.1.0.0/16", Some(vec![8000])),
            ],
            &["10.0.0.0/8".into()],
        )
        .unwrap();
        let text: Vec<String> = warnings.iter().map(ToString::to_string).collect();
        assert!(
            text.iter()
                .any(|w| w.contains("allow[0]") && w.contains("CDN-fronted")),
            "{text:?}"
        );
        assert!(
            text.iter()
                .any(|w| w.contains("allow[0]") && w.contains("accepts uploads")),
            "{text:?}"
        );
        assert!(!text.iter().any(|w| w.contains("allow[1]")), "{text:?}");
        assert!(
            text.iter()
                .any(|w| w.contains("allow[3]") && w.contains("wildcard")),
            "{text:?}"
        );
        assert!(
            text.iter()
                .any(|w| w.contains("allow[4]") && w.contains("accepts uploads")),
            "{text:?}"
        );
        assert!(!text.iter().any(|w| w.contains("allow[2]")), "{text:?}");
        let wildcard_over_writer =
            validate_allow_list("browser", &[host("*.github.com", None)], &[]).unwrap();
        assert!(wildcard_over_writer
            .iter()
            .any(|w| w.message.contains("accepts uploads")));
    }

    #[test]
    fn refused_entries() {
        let cases: Vec<(Vec<EgressAllowYaml>, Vec<String>, &str)> = vec![
            (
                vec![EgressAllowYaml::Preset("npmjs".into())],
                vec![],
                "unknown preset",
            ),
            (vec![host("*", None)], vec![], "wildcard"),
            (vec![host("*.com", None)], vec![], "two labels"),
            (vec![host("a.*.com", None)], vec![], "wildcard"),
            (vec![host("*.*.example.com", None)], vec![], "only once"),
            (vec![host("10.0.0.1", None)], vec![], "cidr"),
            (vec![host("[::1]", None)], vec![], "cidr"),
            (vec![host("2130706433", None)], vec![], "number"),
            (
                vec![host("bad_host.example", None)],
                vec![],
                "letters, digits or hyphens",
            ),
            (vec![host("münchen.example", None)], vec![], "punycode"),
            (
                vec![host("a.example", Some(vec![]))],
                vec![],
                "cannot be empty",
            ),
            (vec![host("a.example", Some(vec![0]))], vec![], "1-65535"),
            (
                vec![host("a.example", Some(vec![443, 443]))],
                vec![],
                "twice",
            ),
            (vec![cidr("10.0.0.0", None)], vec![], "prefix length"),
            (vec![cidr("10.0.0.1/8", None)], vec![], "10.0.0.0/8"),
            (vec![cidr("127.0.0.1/32", None)], vec![], "never connects"),
            (vec![cidr("0.0.0.0/0", None)], vec![], "never connects"),
            (
                vec![cidr("::ffff:169.254.0.0/112", None)],
                vec![],
                "never connects",
            ),
            (
                vec![cidr("10.0.0.0/8", None)],
                vec![],
                "private_destinations",
            ),
            (
                vec![cidr("10.0.0.0/8", None)],
                vec!["10.1.0.0/16".into()],
                "private_destinations",
            ),
            (vec![], vec!["127.0.0.0/8".into()], "never connects"),
            (vec![], vec!["8.8.8.0/24".into()], "must be private"),
            (vec![], vec!["10.0.0.0/7".into()], "must be private"),
            (vec![], vec!["0.0.0.0/0".into()], "never connects"),
            (vec![], vec!["not-a-range".into()], "prefix length"),
        ];
        for (allow, private, expected) in cases {
            let error =
                reason(validate_allow_list("sandbox.egress", &allow, &private).unwrap_err());
            assert!(error.contains(expected), "{allow:?} {private:?}: {error}");
        }
        let too_many: Vec<EgressAllowYaml> = (0..257)
            .map(|i| host(&format!("h{i}.example"), None))
            .collect();
        assert!(
            reason(validate_allow_list("sandbox.egress", &too_many, &[]).unwrap_err())
                .contains("at most 256")
        );
    }

    #[test]
    fn host_patterns_match_exactly() {
        let exact = parse_host_pattern("API.Example.com.").unwrap();
        assert_eq!(exact, HostPattern::Exact("api.example.com".into()));
        assert!(exact.matches("api.example.com"));
        assert!(!exact.matches("x.api.example.com"));
        let wildcard = parse_host_pattern("*.example.com").unwrap();
        assert!(wildcard.matches("a.example.com"));
        assert!(wildcard.matches("a.b.example.com"));
        assert!(!wildcard.matches("example.com"));
        assert!(!wildcard.matches("badexample.com"));
        assert!(!wildcard.matches(".example.com"));
        assert_eq!(wildcard.to_string(), "*.example.com");
    }

    #[test]
    fn egress_block_bounds_and_cross_field_rules() {
        let mut config = AxocoatlConfig::default();
        config.sandbox.network = "egress".into();
        config.sandbox.egress = Some(EgressConfigYaml::default());
        assert!(validate_egress(&config).unwrap().is_empty());

        for (max_connections, ok) in [(7, false), (8, true), (256, true), (257, false)] {
            config.sandbox.egress.as_mut().unwrap().max_connections = max_connections;
            assert_eq!(validate_egress(&config).is_ok(), ok, "{max_connections}");
        }
        config.sandbox.egress.as_mut().unwrap().max_connections = 128;
        for (events, ok) in [
            (999, false),
            (1_000, true),
            (1_000_000, true),
            (1_000_001, false),
        ] {
            config.sandbox.egress.as_mut().unwrap().record_max_events = events;
            assert_eq!(validate_egress(&config).is_ok(), ok, "{events}");
        }
        config.sandbox.egress.as_mut().unwrap().record_max_events = 50_000;
        for (network, ok) in [
            ("podman", true),
            ("axo-egress-test-1", true),
            ("a.b_c", true),
            ("-x", false),
            ("", false),
            ("a b", false),
        ] {
            config.sandbox.egress.as_mut().unwrap().sidecar_network = Some(network.into());
            assert_eq!(validate_egress(&config).is_ok(), ok, "{network}");
        }
        config.sandbox.egress.as_mut().unwrap().sidecar_network = Some("x".repeat(65));
        assert!(validate_egress(&config).is_err());
        config.sandbox.egress.as_mut().unwrap().sidecar_network = None;

        // Under bridge only the fields that do nothing there are named. The
        // browser's own proxy reads sidecar_network and max_connections, and
        // record_max_events caps the record in every mode.
        config.sandbox.network = "bridge".into();
        let ignored = |config: &AxocoatlConfig| -> Vec<String> {
            validate_egress(config)
                .unwrap()
                .into_iter()
                .filter(|w| w.field == "sandbox.egress" && w.message.contains("ignored"))
                .map(|w| w.message)
                .collect()
        };
        assert!(ignored(&config).is_empty());
        config.sandbox.egress.as_mut().unwrap().record_max_events = 2_000;
        assert!(ignored(&config).is_empty());
        config.sandbox.egress.as_mut().unwrap().sidecar_network = Some("axo-net".into());
        config.sandbox.egress.as_mut().unwrap().max_connections = 64;
        let messages = ignored(&config);
        assert_eq!(messages.len(), 1);
        assert!(
            messages[0].starts_with(
                "sidecar_network, max_connections ignored because sandbox.network is \"bridge\""
            ),
            "{messages:?}"
        );
        config.browser = Some(Default::default());
        assert!(ignored(&config).is_empty());
        config.sandbox.egress.as_mut().unwrap().allow = vec![host("a.example", None)];
        let messages = ignored(&config);
        assert!(messages[0].starts_with("allow ignored"), "{messages:?}");
        config.browser = None;
        config.sandbox.egress = Some(EgressConfigYaml {
            allow: vec![host("a.example", None)],
            ..EgressConfigYaml::default()
        });

        config.sandbox.backend = "e2b".into();
        assert!(validate_egress(&config).is_ok());
        for network in ["none", "egress"] {
            config.sandbox.network = network.into();
            assert!(
                reason(validate_egress(&config).unwrap_err()).contains("E2B"),
                "{network}"
            );
        }
        config.sandbox.network = "bridge".into();
        config.browser = Some(Default::default());
        assert!(reason(validate_egress(&config).unwrap_err()).contains("browser"));

        config.sandbox.backend = "podman".into();
        config.sandbox.network = "none".into();
        assert!(validate_egress(&config).is_ok());
        config.browser.as_mut().unwrap().allow = vec![host("a.example", None)];
        assert!(reason(validate_egress(&config).unwrap_err()).contains("only the app under test"));
        config.sandbox.network = "bridge".into();
        assert!(validate_egress(&config).is_ok());
    }

    #[test]
    fn doctor_summary() {
        assert_eq!(allow_summary(&[]), "nothing");
        assert_eq!(
            allow_summary(&[
                EgressAllowYaml::Preset("npm".into()),
                EgressAllowYaml::Preset("pypi".into()),
                host("a.example", None),
                host("b.example", None),
                cidr("8.8.8.0/24", None),
            ]),
            "npm, pypi, 2 hosts, 1 range"
        );
    }
}
