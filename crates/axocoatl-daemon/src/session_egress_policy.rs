//! Compiled egress policies. Pure: no resolution, no I/O.
//!
//! A policy is the config allowlist for one scope plus the per-Session allows
//! that are still in force. Rules match an exact host name, a `*.` suffix or
//! an address range, each with its ports. The digest is the SHA-256 of the
//! policy's canonical JSON, so a replayed Session reproduces the same digest.

use std::net::IpAddr;

use axocoatl_config::egress::{
    parse_host_pattern, parse_private_destinations, validate_ports, HostPattern,
};
use axocoatl_config::egress_presets::{preset, DISTRO_PRESETS};
use axocoatl_config::EgressAllowYaml;
use axocoatl_core::netaddr::{self, Cidr};
use axocoatl_session::network_record::EgressScope;
use serde::Serialize;
use sha2::{Digest, Sha256};

/// A per-Session allow still in force.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRule {
    /// The policy revision that added it.
    pub revision: u64,
    /// A normalized exact host name.
    pub host: String,
    pub ports: Vec<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Matcher {
    Exact(String),
    Subdomains(String),
    Range(Cidr),
}

impl Matcher {
    fn render(&self) -> String {
        match self {
            Self::Exact(host) => host.clone(),
            Self::Subdomains(suffix) => format!("*.{suffix}"),
            Self::Range(range) => range.to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleSource {
    Preset,
    Config,
    Session,
}

impl RuleSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Preset => "preset",
            Self::Config => "config",
            Self::Session => "session",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledRule {
    /// `preset:npm/registry.npmjs.org`, `config#3` or `session#rev7`.
    pub id: String,
    pub matcher: Matcher,
    pub ports: Vec<u16>,
    pub source: RuleSource,
    /// `registry.npmjs.org:443 (preset npm)`.
    pub text: String,
}

impl CompiledRule {
    fn new(
        id: String,
        matcher: Matcher,
        mut ports: Vec<u16>,
        source: RuleSource,
        label: &str,
    ) -> Self {
        ports.sort_unstable();
        ports.dedup();
        let rendered_ports = ports
            .iter()
            .map(u16::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let text = format!("{}:{rendered_ports} ({label})", matcher.render());
        Self {
            id,
            matcher,
            ports,
            source,
            text,
        }
    }
}

#[derive(Serialize)]
struct CanonicalRule<'a> {
    id: &'a str,
    #[serde(rename = "match")]
    matcher: String,
    ports: &'a [u16],
}

#[derive(Serialize)]
struct Canonical<'a> {
    v: u32,
    scope: &'static str,
    rules: Vec<CanonicalRule<'a>>,
    private: Vec<String>,
}

/// One scope's compiled policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledPolicy {
    scope: EgressScope,
    rules: Vec<CompiledRule>,
    private: Vec<Cidr>,
    digest: String,
}

impl CompiledPolicy {
    /// Compile `allow` and `private` as validated config (they are checked
    /// again here), then the per-Session rules in revision order.
    pub fn compile(
        scope: EgressScope,
        allow: &[EgressAllowYaml],
        private: &[String],
        session_rules: &[SessionRule],
    ) -> Result<Self, String> {
        let field = match scope {
            EgressScope::Session => "sandbox.egress",
            EgressScope::Provisioning => "provisioning",
            EgressScope::Browser => "browser",
        };
        axocoatl_config::validate_allow_list(field, allow, private)
            .map_err(|error| error.to_string())?;
        let private =
            parse_private_destinations(field, private).map_err(|error| error.to_string())?;
        let mut rules = Vec::new();
        for (index, entry) in allow.iter().enumerate() {
            match entry {
                EgressAllowYaml::Preset(name) => {
                    let found = preset(name).ok_or_else(|| format!("unknown preset {name}"))?;
                    for (host, ports) in found.hosts {
                        rules.push(CompiledRule::new(
                            format!("preset:{name}/{host}"),
                            Matcher::Exact((*host).to_string()),
                            ports.to_vec(),
                            RuleSource::Preset,
                            &format!("preset {name}"),
                        ));
                    }
                }
                EgressAllowYaml::Host(rule) => {
                    let matcher = match parse_host_pattern(&rule.host)? {
                        HostPattern::Exact(host) => Matcher::Exact(host),
                        HostPattern::Subdomains(suffix) => Matcher::Subdomains(suffix),
                    };
                    rules.push(CompiledRule::new(
                        format!("config#{index}"),
                        matcher,
                        validate_ports(rule.ports.as_deref())?,
                        RuleSource::Config,
                        "config",
                    ));
                }
                EgressAllowYaml::Cidr(rule) => {
                    let range: Cidr = rule
                        .cidr
                        .parse()
                        .map_err(|error: netaddr::CidrError| error.to_string())?;
                    rules.push(CompiledRule::new(
                        format!("config#{index}"),
                        Matcher::Range(range),
                        validate_ports(rule.ports.as_deref())?,
                        RuleSource::Config,
                        "config",
                    ));
                }
            }
        }
        for rule in session_rules {
            let host = validate_session_host(&rule.host)?;
            rules.push(CompiledRule::new(
                format!("session#rev{}", rule.revision),
                Matcher::Exact(host),
                validate_ports(Some(&rule.ports))?,
                RuleSource::Session,
                "allowed for this Session",
            ));
        }
        let canonical = Canonical {
            v: 1,
            scope: scope.as_str(),
            rules: rules
                .iter()
                .map(|rule| CanonicalRule {
                    id: &rule.id,
                    matcher: rule.matcher.render(),
                    ports: &rule.ports,
                })
                .collect(),
            private: private.iter().map(ToString::to_string).collect(),
        };
        let digest = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&canonical).map_err(|error| error.to_string())?)
        );
        Ok(Self {
            scope,
            rules,
            private,
            digest,
        })
    }

    /// The provisioning policy: the distribution presets only.
    pub fn provisioning() -> Self {
        let allow: Vec<EgressAllowYaml> = DISTRO_PRESETS
            .iter()
            .map(|name| EgressAllowYaml::Preset((*name).to_string()))
            .collect();
        Self::compile(EgressScope::Provisioning, &allow, &[], &[])
            .expect("distribution presets compile")
    }

    pub fn scope(&self) -> EgressScope {
        self.scope
    }

    pub fn digest(&self) -> &str {
        &self.digest
    }

    pub fn rules(&self) -> &[CompiledRule] {
        &self.rules
    }

    pub fn private_destinations(&self) -> &[Cidr] {
        &self.private
    }

    /// Rendered rules, for the record's `policy` events.
    pub fn rendered(&self) -> Vec<String> {
        self.rules.iter().map(|rule| rule.text.clone()).collect()
    }

    /// The first rule allowing a normalized host name on `port`.
    pub fn match_name(&self, host: &str, port: u16) -> Option<&CompiledRule> {
        self.rules.iter().find(|rule| {
            rule.ports.contains(&port)
                && match &rule.matcher {
                    Matcher::Exact(name) => name == host,
                    Matcher::Subdomains(suffix) => {
                        HostPattern::Subdomains(suffix.clone()).matches(host)
                    }
                    Matcher::Range(_) => false,
                }
        })
    }

    /// The first range rule allowing an IP-literal destination on `port`.
    pub fn match_ip(&self, ip: IpAddr, port: u16) -> Option<&CompiledRule> {
        self.rules.iter().find(|rule| {
            rule.ports.contains(&port)
                && matches!(&rule.matcher, Matcher::Range(range) if range.contains(ip))
        })
    }

    /// Whether a Private address is inside a listed private destination.
    pub fn allows_private(&self, ip: IpAddr) -> bool {
        self.private.iter().any(|range| range.contains(ip))
    }
}

/// A host a human may allow for one Session: an exact name only, with no
/// wildcard and no IP literal.
pub fn validate_session_host(host: &str) -> Result<String, String> {
    if host.contains('*') {
        return Err(
            "a Session allow names one exact host; wildcards belong in sandbox.egress.allow".into(),
        );
    }
    if netaddr::parse_ip_literal(host).is_some() {
        return Err("a Session allow names a host, not an IP address; list address ranges under sandbox.egress.allow".into());
    }
    netaddr::normalize_host_name(host).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axocoatl_config::{EgressCidrYaml, EgressHostYaml};

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

    fn sample() -> CompiledPolicy {
        CompiledPolicy::compile(
            EgressScope::Session,
            &[
                EgressAllowYaml::Preset("npm".into()),
                host("api.example.com", Some(vec![8443, 443])),
                host("*.example.org", None),
                cidr("10.20.0.0/16", Some(vec![8000])),
            ],
            &["10.0.0.0/8".into()],
            &[SessionRule {
                revision: 7,
                host: "Extra.Example.NET".into(),
                ports: vec![443],
            }],
        )
        .unwrap()
    }

    #[test]
    fn rule_ids_text_and_matching() {
        let policy = sample();
        let ids: Vec<&str> = policy.rules().iter().map(|rule| rule.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "preset:npm/registry.npmjs.org",
                "config#1",
                "config#2",
                "config#3",
                "session#rev7"
            ]
        );
        assert_eq!(
            policy.rendered(),
            [
                "registry.npmjs.org:443 (preset npm)",
                "api.example.com:443,8443 (config)",
                "*.example.org:443 (config)",
                "10.20.0.0/16:8000 (config)",
                "extra.example.net:443 (allowed for this Session)",
            ]
        );
        assert_eq!(
            policy.match_name("registry.npmjs.org", 443).unwrap().id,
            "preset:npm/registry.npmjs.org"
        );
        assert!(policy.match_name("registry.npmjs.org", 80).is_none());
        assert_eq!(
            policy.match_name("api.example.com", 8443).unwrap().id,
            "config#1"
        );
        assert_eq!(
            policy.match_name("a.b.example.org", 443).unwrap().id,
            "config#2"
        );
        assert!(policy.match_name("example.org", 443).is_none());
        assert!(policy.match_name("evilexample.org", 443).is_none());
        assert_eq!(
            policy.match_name("extra.example.net", 443).unwrap().id,
            "session#rev7"
        );
        assert!(policy.match_name("10.20.1.1", 8000).is_none());
        assert_eq!(
            policy
                .match_ip("10.20.1.1".parse().unwrap(), 8000)
                .unwrap()
                .id,
            "config#3"
        );
        assert!(policy.match_ip("10.20.1.1".parse().unwrap(), 443).is_none());
        assert!(policy
            .match_ip("10.21.1.1".parse().unwrap(), 8000)
            .is_none());
        assert!(policy.allows_private("10.9.9.9".parse().unwrap()));
        assert!(!policy.allows_private("192.168.1.1".parse().unwrap()));
    }

    #[test]
    fn digests_are_stable_and_sensitive() {
        let first = sample();
        let second = sample();
        assert_eq!(first.digest(), second.digest());
        assert_eq!(first.digest().len(), 64);
        let changed = CompiledPolicy::compile(
            EgressScope::Session,
            &[EgressAllowYaml::Preset("npm".into())],
            &[],
            &[],
        )
        .unwrap();
        assert_ne!(first.digest(), changed.digest());
        let other_scope = CompiledPolicy::compile(
            EgressScope::Browser,
            &[EgressAllowYaml::Preset("npm".into())],
            &[],
            &[],
        )
        .unwrap();
        assert_ne!(changed.digest(), other_scope.digest());
        let empty = CompiledPolicy::compile(EgressScope::Session, &[], &[], &[]).unwrap();
        assert!(empty.rules().is_empty());
        assert!(empty.match_name("registry.npmjs.org", 443).is_none());
    }

    #[test]
    fn provisioning_allows_only_distribution_hosts() {
        let policy = CompiledPolicy::provisioning();
        assert_eq!(policy.scope(), EgressScope::Provisioning);
        assert!(policy.match_name("deb.debian.org", 80).is_some());
        assert!(policy.match_name("dl-cdn.alpinelinux.org", 443).is_some());
        assert!(policy.match_name("archive.ubuntu.com", 80).is_some());
        assert!(policy.match_name("registry.npmjs.org", 443).is_none());
    }

    #[test]
    fn invalid_inputs_are_refused() {
        assert!(
            CompiledPolicy::compile(EgressScope::Session, &[host("*", None)], &[], &[]).is_err()
        );
        assert!(CompiledPolicy::compile(
            EgressScope::Session,
            &[cidr("10.0.0.0/8", None)],
            &[],
            &[]
        )
        .is_err());
        for bad in ["*.example.com", "10.0.0.1", "[::1]", "bad_host", ""] {
            assert!(validate_session_host(bad).is_err(), "{bad}");
            assert!(CompiledPolicy::compile(
                EgressScope::Session,
                &[],
                &[],
                &[SessionRule {
                    revision: 1,
                    host: bad.into(),
                    ports: vec![443]
                }]
            )
            .is_err());
        }
        assert_eq!(
            validate_session_host("API.example.com.").unwrap(),
            "api.example.com"
        );
    }
}
