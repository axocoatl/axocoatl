//! Routes compiled from `sandbox.egress.routes`, the request checks a route
//! applies, and where its credential is read.
//!
//! A request's path must be canonical before any rule sees it: no `.` or
//! `..` segment (also before a `;` path parameter), no `//`, no `\`, no
//! escaped or twice-escaped `/`, `\` or `.`, no control or non-ASCII byte,
//! and every `%` a two-digit escape. Rules then match the method exactly
//! (methods are case-sensitive), the path segment by segment (`*` one
//! segment, `**` any number), and each required query parameter, which must
//! appear exactly once with the required value after decoding. A query with
//! a raw `;` matches no rule that requires a parameter, since some servers
//! split parameters on `;` as well as `&`. Everything no rule allows is
//! refused.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axocoatl_config::egress_routes::{
    canonical_segment, check_owner_only_file, expand_user_path, parse_path_glob, PathGlob,
    MAX_CA_FILE_BYTES, MAX_SECRET_FILE_BYTES, READ_ONLY_METHODS,
};
use axocoatl_config::{
    CredentialSourceYaml, EgressRouteYaml, RouteAccessYaml, RouteForYaml, RouteInjectYaml,
};
use axocoatl_session::network_record::BindingKind;
use hyper::header::HeaderName;
use rustls::pki_types::CertificateDer;
use secrecy::SecretString;
use sha2::{Digest, Sha256};

use super::x509;

/// Longest credential value accepted.
pub const MAX_SECRET_BYTES: usize = 8 * 1024;

/// Where a route's credential value is read, each time a request needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialSource {
    /// An environment variable of the daemon.
    Env(String),
    /// An owner-only file outside every Workspace.
    File(PathBuf),
}

/// Why a credential could not be read. Never carries the value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("credential {name} is unavailable: {reason}")]
pub struct CredentialError {
    pub name: String,
    pub reason: String,
}

fn valid_secret(value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err("it is empty".into());
    }
    if value.len() > MAX_SECRET_BYTES {
        return Err(format!("it is longer than {MAX_SECRET_BYTES} bytes"));
    }
    if value.bytes().any(|byte| byte.is_ascii_control()) {
        return Err("it contains a line break or another control character".into());
    }
    Ok(())
}

/// Whether `path` is inside one of `workspaces` (both canonicalized when
/// they exist).
pub fn inside_workspace(path: &Path, workspaces: &[PathBuf]) -> Option<PathBuf> {
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    workspaces.iter().find_map(|root| {
        let root_canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.clone());
        (canonical.starts_with(&root_canonical) || path.starts_with(root)).then(|| root.clone())
    })
}

impl CredentialSource {
    /// Read the value now. A file must still be a regular, owner-only file
    /// owned by this user and outside every Workspace in `workspaces`.
    pub fn read(
        &self,
        name: &str,
        workspaces: &[PathBuf],
    ) -> Result<SecretString, CredentialError> {
        let error = |reason: String| CredentialError {
            name: name.to_string(),
            reason,
        };
        match self {
            Self::Env(variable) => {
                let value = std::env::var(variable).map_err(|_| {
                    error(format!(
                        "the daemon's environment has no {variable} (set it where the daemon starts)"
                    ))
                })?;
                valid_secret(&value).map_err(|reason| error(format!("{variable}: {reason}")))?;
                Ok(SecretString::from(value))
            }
            Self::File(path) => {
                if let Some(root) = inside_workspace(path, workspaces) {
                    return Err(error(format!(
                        "{} is inside the Workspace {}, where Agents can read or replace it",
                        path.display(),
                        root.display()
                    )));
                }
                let text = read_owner_only(path, MAX_SECRET_FILE_BYTES).map_err(&error)?;
                let value = text
                    .strip_suffix("\r\n")
                    .or_else(|| text.strip_suffix('\n'))
                    .unwrap_or(&text);
                valid_secret(value)
                    .map_err(|reason| error(format!("{}: {reason}", path.display())))?;
                Ok(SecretString::from(value.to_string()))
            }
        }
    }
}

/// Open `path` without following a symbolic link and read it, after checking
/// that it is a regular file owned by this user that only its owner can read
/// or write.
fn read_owner_only(path: &Path, max_bytes: u64) -> Result<zeroize::Zeroizing<String>, String> {
    check_owner_only_file(path, max_bytes)?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let file = options
        .open(path)
        .map_err(|error| format!("{} cannot be opened: {error}", path.display()))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("{} cannot be read: {error}", path.display()))?;
    if !metadata.is_file() || metadata.len() > max_bytes {
        return Err(format!(
            "{} is not a regular file of at most {max_bytes} bytes",
            path.display()
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: geteuid has no preconditions and cannot fail.
        let me = unsafe { libc::geteuid() };
        if metadata.uid() != me || metadata.mode() & 0o077 != 0 {
            return Err(format!(
                "{} must be owned by the daemon's user and readable only by it (chmod 600)",
                path.display()
            ));
        }
    }
    let mut text = zeroize::Zeroizing::new(String::new());
    file.take(max_bytes + 1)
        .read_to_string(&mut text)
        .map_err(|error| format!("{} cannot be read as text: {error}", path.display()))?;
    Ok(text)
}

/// How a route adds its credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Injection {
    /// `Authorization: Basic base64(username:credential)`.
    Basic { username: String },
    /// `name: prefix + credential + suffix`.
    Header {
        name: HeaderName,
        prefix: String,
        suffix: String,
    },
}

impl Injection {
    /// The header name this injection sets.
    pub fn header_name(&self) -> HeaderName {
        match self {
            Self::Basic { .. } => hyper::header::AUTHORIZATION,
            Self::Header { name, .. } => name.clone(),
        }
    }
}

/// A route's credential: its name (recorded), source and injection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteCredential {
    pub name: String,
    pub source: CredentialSource,
    pub inject: Injection,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum QueryValue {
    Any,
    Exact(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CompiledRule {
    methods: Vec<String>,
    path: PathGlob,
    query: Vec<(String, QueryValue)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Rules {
    Access(RouteAccessYaml),
    List(Vec<CompiledRule>),
}

/// One compiled route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    /// Position in `sandbox.egress.routes`.
    pub index: usize,
    /// Lowercase, without a trailing dot.
    pub host: String,
    pub ports: Vec<u16>,
    /// Kinds of egress credential the route serves.
    pub bindings: Vec<BindingKind>,
    pub credential: Option<RouteCredential>,
    rules: Rules,
    /// Extra trusted roots for the upstream, from `upstream_ca`.
    pub upstream_roots: Vec<CertificateDer<'static>>,
    /// SHA-256 of `upstream_roots`, to share TLS settings between routes.
    pub upstream_roots_id: Option<[u8; 32]>,
    pub env_placeholders: Vec<String>,
    pub allow_encoded_responses: bool,
    /// Pass `Set-Cookie` and `Set-Cookie2` on a credentialed route.
    pub allow_set_cookie: bool,
    pub max_request_bytes: u64,
}

/// What a route decided about one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleDecision {
    /// Allowed by the rule named here, such as `route#0.rules[1]`.
    Allowed { rule: String },
    /// Refused: no rule allows it. `hint` says what rule would.
    Denied { reason: String, hint: String },
}

/// A request path checked for canonical form, with its query parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalPath {
    /// The path without the query, as sent.
    pub path: String,
    /// The segments after the leading `/`, escapes in uppercase hex.
    pub segments: Vec<String>,
    /// The raw query, without `?`.
    pub query: Option<String>,
    /// Decoded `(name, value)` pairs, or `None` when the query is not
    /// well-formed: a bad escape, or a raw `;` that some servers read as a
    /// separator (then no rule that requires a parameter matches).
    pub params: Option<Vec<(String, String)>>,
}

/// Why a path is not canonical (`path_not_canonical`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotCanonical(pub String);

fn decode_component(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = std::str::from_utf8(bytes.get(index + 1..index + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Whether a checked segment has an escape of a control byte (`%00`-`%1F`,
/// `%7F`).
fn escapes_control(segment: &str) -> bool {
    segment.match_indices('%').any(|(at, _)| {
        segment
            .get(at + 1..at + 3)
            .and_then(|hex| u8::from_str_radix(hex, 16).ok())
            .is_some_and(|byte| byte < 0x20 || byte == 0x7f)
    })
}

/// Check a request target (`/path?query`) and split it.
pub fn canonicalize(target: &str) -> Result<CanonicalPath, NotCanonical> {
    let refuse = |reason: &str| Err(NotCanonical(reason.to_string()));
    if target
        .bytes()
        .any(|byte| byte.is_ascii_control() || !byte.is_ascii() || byte == b'\\')
    {
        return refuse("the request target has a control, non-ASCII or '\\' byte");
    }
    if target.contains('#') {
        return refuse("the request target has a fragment");
    }
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (target, None),
    };
    let Some(rest) = path.strip_prefix('/') else {
        return refuse("the path does not start with '/'");
    };
    let parts: Vec<&str> = rest.split('/').collect();
    let last = parts.len() - 1;
    let mut segments = Vec::with_capacity(parts.len());
    for (index, part) in parts.iter().enumerate() {
        if part.is_empty() && index != last {
            return refuse("the path has an empty segment ('//')");
        }
        let segment = canonical_segment(part).map_err(NotCanonical)?;
        if escapes_control(&segment) {
            return refuse("the path escapes a control character");
        }
        segments.push(segment);
    }
    if let Some(query) = query {
        if query.bytes().any(|byte| byte == b' ') {
            return refuse("the query has a space");
        }
    }
    // Rack 2, older Python and Go and others also split parameters on ';',
    // so `a=1&x=;service=b` holds a second `service` for them. A query with
    // a raw ';' is read as not well-formed: no rule that requires a
    // parameter matches it.
    let params = query.map_or(Some(Vec::new()), |query| {
        if query.contains(';') {
            return None;
        }
        query
            .split('&')
            .filter(|pair| !pair.is_empty())
            .map(|pair| {
                let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
                Some((decode_component(name)?, decode_component(value)?))
            })
            .collect()
    });
    Ok(CanonicalPath {
        path: path.to_string(),
        segments,
        query: query.map(str::to_string),
        params,
    })
}

impl CompiledRule {
    fn matches(&self, method: &str, path: &CanonicalPath) -> bool {
        if !self.methods.iter().any(|allowed| allowed == method) {
            return false;
        }
        let segments: Vec<&str> = path.segments.iter().map(String::as_str).collect();
        if !self.path.matches(&segments) {
            return false;
        }
        if self.query.is_empty() {
            return true;
        }
        let Some(params) = &path.params else {
            return false;
        };
        self.query.iter().all(|(name, required)| {
            let mut values = params
                .iter()
                .filter(|(param, _)| param == name)
                .map(|(_, value)| value);
            match (values.next(), values.next()) {
                (Some(value), None) => match required {
                    QueryValue::Any => true,
                    QueryValue::Exact(expected) => value == expected,
                },
                _ => false,
            }
        })
    }

    fn render(&self) -> String {
        let mut text = format!("{} {}", self.methods.join("|"), self.path);
        if !self.query.is_empty() {
            let query: Vec<String> = self
                .query
                .iter()
                .map(|(name, value)| match value {
                    QueryValue::Any => format!("{name}=*"),
                    QueryValue::Exact(value) => format!("{name}={value}"),
                })
                .collect();
            text.push('?');
            text.push_str(&query.join("&"));
        }
        text
    }
}

fn binding_kind(kind: RouteForYaml) -> BindingKind {
    match kind {
        RouteForYaml::Agent => BindingKind::Agent,
        RouteForYaml::Terminal => BindingKind::Terminal,
        RouteForYaml::Setup => BindingKind::Setup,
    }
}

fn compile_inject(inject: &RouteInjectYaml) -> Result<Injection, String> {
    match (&inject.basic, &inject.header) {
        (Some(basic), None) => Ok(Injection::Basic {
            username: basic.username.clone(),
        }),
        (None, Some(header)) => {
            let name = HeaderName::from_bytes(header.as_bytes())
                .map_err(|error| format!("header {header:?}: {error}"))?;
            let format = inject.format.as_deref().unwrap_or("{}");
            let (prefix, suffix) = format
                .split_once("{}")
                .ok_or_else(|| "format has no {}".to_string())?;
            Ok(Injection::Header {
                name,
                prefix: prefix.to_string(),
                suffix: suffix.to_string(),
            })
        }
        _ => Err("inject names exactly one of basic and header".into()),
    }
}

impl Route {
    /// Compile one validated route. `credentials` is the config's
    /// `credentials` block; `workspaces` are refused as places for an
    /// `upstream_ca`.
    pub fn compile(
        index: usize,
        route: &EgressRouteYaml,
        credentials: &BTreeMap<String, CredentialSourceYaml>,
        workspaces: &[PathBuf],
    ) -> Result<Self, String> {
        let field = format!("sandbox.egress.routes[{index}]");
        let host = axocoatl_core::netaddr::normalize_host_name(&route.host)
            .map_err(|error| format!("{field}.host: {error}"))?;
        let credential = match (&route.credential, &route.inject) {
            (Some(name), Some(inject)) => {
                let source = credentials
                    .get(name)
                    .ok_or_else(|| format!("{field}.credential: no credential named {name}"))?;
                let source = match (&source.env, &source.file) {
                    (Some(variable), None) => CredentialSource::Env(variable.clone()),
                    (None, Some(path)) => CredentialSource::File(
                        expand_user_path(path)
                            .map_err(|error| format!("credentials.{name}.file: {error}"))?,
                    ),
                    _ => return Err(format!("credentials.{name}: name exactly one source")),
                };
                Some(RouteCredential {
                    name: name.clone(),
                    source,
                    inject: compile_inject(inject)
                        .map_err(|error| format!("{field}.inject: {error}"))?,
                })
            }
            (None, None) => None,
            _ => return Err(format!("{field}: credential and inject go together")),
        };
        let rules = match (&route.access, route.rules.is_empty()) {
            (Some(access), true) => Rules::Access(*access),
            (None, false) => Rules::List(
                route
                    .rules
                    .iter()
                    .enumerate()
                    .map(|(rule_index, rule)| {
                        Ok(CompiledRule {
                            methods: rule.methods.clone(),
                            path: parse_path_glob(&rule.path).map_err(|error| {
                                format!("{field}.rules[{rule_index}].path: {error}")
                            })?,
                            query: rule
                                .query
                                .iter()
                                .map(|(name, value)| {
                                    (
                                        name.clone(),
                                        if value == "*" {
                                            QueryValue::Any
                                        } else {
                                            QueryValue::Exact(value.clone())
                                        },
                                    )
                                })
                                .collect(),
                        })
                    })
                    .collect::<Result<_, String>>()?,
            ),
            _ => return Err(format!("{field}: set exactly one of access and rules")),
        };
        let (upstream_roots, upstream_roots_id) = match &route.upstream_ca {
            Some(path) => {
                let path = expand_user_path(path)
                    .map_err(|error| format!("{field}.upstream_ca: {error}"))?;
                if let Some(root) = inside_workspace(&path, workspaces) {
                    return Err(format!(
                        "{field}.upstream_ca: {} is inside the Workspace {}",
                        path.display(),
                        root.display()
                    ));
                }
                let text = read_owner_only(&path, MAX_CA_FILE_BYTES)
                    .map_err(|error| format!("{field}.upstream_ca: {error}"))?;
                let roots = x509::parse_pem_certificates(&text)
                    .map_err(|error| format!("{field}.upstream_ca: {error}"))?;
                let mut digest = Sha256::new();
                for root in &roots {
                    digest.update((root.len() as u64).to_be_bytes());
                    digest.update(root);
                }
                (
                    roots.into_iter().map(CertificateDer::from).collect(),
                    Some(digest.finalize().into()),
                )
            }
            None => (Vec::new(), None),
        };
        Ok(Self {
            index,
            host,
            ports: axocoatl_config::egress::validate_ports(route.ports.as_deref())
                .map_err(|error| format!("{field}.ports: {error}"))?,
            bindings: route
                .bindings
                .as_deref()
                .unwrap_or(&[RouteForYaml::Agent])
                .iter()
                .copied()
                .map(binding_kind)
                .collect(),
            credential,
            rules,
            upstream_roots,
            upstream_roots_id,
            env_placeholders: route.env_placeholders.clone(),
            allow_encoded_responses: route.allow_encoded_responses,
            allow_set_cookie: route.allow_set_cookie,
            max_request_bytes: route.max_request_bytes,
        })
    }

    /// `route#<index>`, as the record names it.
    pub fn label(&self) -> String {
        format!("route#{}", self.index)
    }

    /// Whether an egress credential of `kind` may use this route.
    pub fn allows_binding(&self, kind: BindingKind) -> bool {
        self.bindings.contains(&kind)
    }

    /// Whether the route covers `port`.
    pub fn covers_port(&self, port: u16) -> bool {
        self.ports.contains(&port)
    }

    /// Everything the route decides with, as JSON for the Session policy's
    /// digest: never a credential value, only where it is read.
    pub fn canonical(&self) -> serde_json::Value {
        let credential = self.credential.as_ref().map(|credential| {
            let source = match &credential.source {
                CredentialSource::Env(variable) => serde_json::json!({ "env": variable }),
                CredentialSource::File(path) => {
                    serde_json::json!({ "file": path.display().to_string() })
                }
            };
            let inject = match &credential.inject {
                Injection::Basic { username } => serde_json::json!({ "basic": username }),
                Injection::Header {
                    name,
                    prefix,
                    suffix,
                } => serde_json::json!({
                    "header": name.as_str(),
                    "prefix": prefix,
                    "suffix": suffix,
                }),
            };
            serde_json::json!({
                "name": credential.name,
                "source": source,
                "inject": inject,
            })
        });
        serde_json::json!({
            "host": self.host,
            "ports": self.ports,
            "for": self.bindings,
            "credential": credential,
            "rules": self.describe(),
            "upstream_ca": self.upstream_roots_id.map(hex::encode),
            "env_placeholders": self.env_placeholders,
            "allow_encoded_responses": self.allow_encoded_responses,
            "allow_set_cookie": self.allow_set_cookie,
            "max_request_bytes": self.max_request_bytes,
        })
    }

    /// One line for the Session policy's rendered rules, such as
    /// `github.com:443 (route#0: 2 rules, credential github, for agent)`.
    pub fn policy_text(&self) -> String {
        let ports = self
            .ports
            .iter()
            .map(u16::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let rules = match &self.rules {
            Rules::Access(RouteAccessYaml::ReadOnly) => "access read-only".to_string(),
            Rules::Access(RouteAccessYaml::Full) => "access full".to_string(),
            Rules::List(rules) if rules.len() == 1 => "1 rule".to_string(),
            Rules::List(rules) => format!("{} rules", rules.len()),
        };
        let credential = self
            .credential
            .as_ref()
            .map(|credential| format!(", credential {}", credential.name))
            .unwrap_or_default();
        let kinds = self
            .bindings
            .iter()
            .map(|kind| match kind {
                BindingKind::Agent => "agent",
                BindingKind::Terminal => "terminal",
                BindingKind::Setup => "setup",
                BindingKind::Provisioning => "provisioning",
                BindingKind::Browser => "browser",
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "{}:{ports} ({}: {rules}{credential}, for {kinds})",
            self.host,
            self.label()
        )
    }

    /// Check one request whose path is already canonical.
    pub fn check(&self, method: &str, path: &CanonicalPath) -> RuleDecision {
        let label = self.label();
        match &self.rules {
            Rules::Access(RouteAccessYaml::ReadOnly) => {
                if READ_ONLY_METHODS.contains(&method) {
                    return RuleDecision::Allowed {
                        rule: format!("{label}.access=read-only"),
                    };
                }
            }
            Rules::Access(RouteAccessYaml::Full) => {
                if method != "CONNECT" {
                    return RuleDecision::Allowed {
                        rule: format!("{label}.access=full"),
                    };
                }
            }
            Rules::List(rules) => {
                if let Some(index) = rules.iter().position(|rule| rule.matches(method, path)) {
                    return RuleDecision::Allowed {
                        rule: format!("{label}.rules[{index}]"),
                    };
                }
            }
        }
        let shown = if path.path.chars().count() > 120 {
            format!("{}...", path.path.chars().take(120).collect::<String>())
        } else {
            path.path.clone()
        };
        RuleDecision::Denied {
            reason: format!("no rule of {label} ({}) allows {method} {shown}", self.host),
            hint: format!(
                "Add a rule to sandbox.egress.routes[{}] such as {{methods: [{method}], path: \"{shown}\"}}, \
                 then run axocoatl network reload.",
                self.index
            ),
        }
    }

    /// The route's rules, rendered for records and refusals.
    pub fn describe(&self) -> Vec<String> {
        match &self.rules {
            Rules::Access(RouteAccessYaml::ReadOnly) => {
                vec![format!("{} {}", READ_ONLY_METHODS.join("|"), "/**")]
            }
            Rules::Access(RouteAccessYaml::Full) => vec!["* /**".into()],
            Rules::List(rules) => rules.iter().map(CompiledRule::render).collect(),
        }
    }
}

/// Every route of a Session, looked up by host and port.
#[derive(Debug, Clone, Default)]
pub struct RouteTable {
    routes: Vec<Arc<Route>>,
}

impl RouteTable {
    /// Compile `sandbox.egress.routes`.
    pub fn compile(
        routes: &[EgressRouteYaml],
        credentials: &BTreeMap<String, CredentialSourceYaml>,
        workspaces: &[PathBuf],
    ) -> Result<Self, String> {
        Ok(Self {
            routes: routes
                .iter()
                .enumerate()
                .map(|(index, route)| {
                    Route::compile(index, route, credentials, workspaces).map(Arc::new)
                })
                .collect::<Result<_, String>>()?,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }

    pub fn routes(&self) -> &[Arc<Route>] {
        &self.routes
    }

    /// The route for `host:port`. `host` is compared case-insensitively,
    /// without a trailing dot.
    pub fn find(&self, host: &str, port: u16) -> Option<Arc<Route>> {
        let host = host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase();
        self.routes
            .iter()
            .find(|route| route.host == host && route.covers_port(port))
            .cloned()
    }
}

#[cfg(test)]
#[path = "rules_tests.rs"]
mod tests;
