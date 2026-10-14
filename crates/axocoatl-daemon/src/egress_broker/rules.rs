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

use axocoatl_config::egress_host_ollama::{
    host_ollama_bindings, HOST_OLLAMA_ROUTE_HOST, HOST_OLLAMA_ROUTE_PORT,
};
use axocoatl_config::egress_routes::{
    canonical_segment, check_owner_only_file, expand_user_path, is_model_api_host, parse_path_glob,
    PathGlob, MAX_CA_FILE_BYTES, MAX_SECRET_FILE_BYTES, READ_ONLY_METHODS,
};
use axocoatl_config::{
    CredentialSourceYaml, EgressRouteYaml, HostOllamaRouteYaml, ProviderFetchYaml, RouteAccessYaml,
    RouteForYaml, RouteInjectYaml,
};
use axocoatl_session::network_record::BindingKind;
use hyper::header::HeaderName;
use rustls::pki_types::CertificateDer;
use secrecy::SecretString;
use sha2::{Digest, Sha256};

use super::x509;

/// Longest credential value accepted.
pub const MAX_SECRET_BYTES: usize = 8 * 1024;

/// Largest request body on a host route, as a configured route's default.
const HOST_ROUTE_MAX_REQUEST_BYTES: u64 = 1 << 30;

/// What the route to Ollama on this computer allows: Ollama's inference and
/// read-only model listing, natively and through its OpenAI- and
/// Anthropic-compatible API, as the Ollama 0.20.6 server serves them.
/// Everything else is refused, among it what makes Ollama reach other hosts
/// or change its models: `/api/pull`, `/api/push`, `/api/create`,
/// `/api/copy`, `/api/delete`, `/api/blobs/*`, the account endpoints
/// (`/api/me`, `/api/signout`, `/api/user/keys/*`) and web search and fetch
/// (`/api/experimental/web_search`, `/api/experimental/web_fetch`), which
/// 0.20.6 sends to `ollama.com` or to the registry a model name names.
pub const HOST_OLLAMA_RULES: [(&[&str], &str); 17] = [
    (&["GET", "HEAD"], "/"),
    (&["GET", "HEAD"], "/api/version"),
    (&["GET", "HEAD"], "/api/tags"),
    (&["GET", "HEAD"], "/api/ps"),
    (&["GET", "HEAD"], "/api/status"),
    (&["GET", "HEAD"], "/v1/models"),
    (&["GET", "HEAD"], "/v1/models/*"),
    (&["POST"], "/api/show"),
    (&["POST"], "/api/chat"),
    (&["POST"], "/api/generate"),
    (&["POST"], "/api/embed"),
    (&["POST"], "/api/embeddings"),
    (&["POST"], "/v1/chat/completions"),
    (&["POST"], "/v1/completions"),
    (&["POST"], "/v1/embeddings"),
    (&["POST"], "/v1/responses"),
    (&["POST"], "/v1/messages"),
];

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

/// How a value that only a [`ManagedCredential`] sends begins: a sign-in
/// the daemon keeps current itself (`{"format":"axocoatl.…`), such as the
/// ChatGPT sign-in `axocoatl connect codex` stores. Read as a plain
/// credential, it is refused: it holds a refresh token, never sent upstream.
pub const MANAGED_VALUE_PREFIX: &str = "{\"format\":\"axocoatl.";

fn valid_secret(value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err("it is empty".into());
    }
    // However a route names the file or variable that holds it, the receipt
    // signing key is never sent.
    if value.starts_with(crate::receipt_key::KEY_VALUE_PREFIX) {
        return Err(crate::receipt_key::NEVER_A_ROUTE_CREDENTIAL.into());
    }
    if value.starts_with(MANAGED_VALUE_PREFIX) {
        return Err(
            "it is a sign-in that the daemon sends only on a route to its own host (for \
             codex-chatgpt, a route to chatgpt.com, such as the one Axocoatl adds for a Codex \
             writer with credential: codex-chatgpt)"
                .into(),
        );
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
        if crate::receipt_key::is_receipt_key_name(name) {
            return Err(error(crate::receipt_key::NEVER_A_ROUTE_CREDENTIAL.into()));
        }
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
pub(crate) fn read_owner_only(
    path: &Path,
    max_bytes: u64,
) -> Result<zeroize::Zeroizing<String>, String> {
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

/// What a [`ManagedCredential`] adds to one request.
pub struct ManagedValue {
    /// The value the route's injection carries (an access token).
    pub secret: SecretString,
    /// Further headers the upstream needs with it, such as an account id:
    /// set by the route in place of any the client sent.
    pub headers: Vec<(HeaderName, hyper::header::HeaderValue)>,
}

/// A credential the daemon keeps current itself: a sign-in whose access
/// token it refreshes (`external_agent::codex_chatgpt::ChatgptBroker`). A
/// route to its host whose credential is read from the file one manages
/// asks it for the value of each request instead of reading the file; a
/// route to any other host reads the file as a plain credential, which
/// refuses the stored sign-in ([`MANAGED_VALUE_PREFIX`]).
#[async_trait::async_trait]
pub trait ManagedCredential: Send + Sync {
    /// The file it manages, as a route's `File` credential names it.
    fn file(&self) -> &Path;
    /// The only host its value is sent to (lowercase, as a route's host):
    /// for the ChatGPT sign-in, `chatgpt.com`.
    fn host(&self) -> &str;
    /// The value to send now, refreshed first when it is about to expire.
    /// The error never holds a value.
    async fn current(&self) -> Result<ManagedValue, String>;
    /// The upstream answered `401` to a request sent with `sent`: refresh,
    /// unless that has happened since.
    async fn rejected(&self, sent: &SecretString);
}

/// The credentials the daemon manages, found by the file a route names.
#[derive(Clone, Default)]
pub struct ManagedCredentials {
    entries: Vec<Arc<dyn ManagedCredential>>,
}

impl std::fmt::Debug for ManagedCredentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_list()
            .entries(
                self.entries
                    .iter()
                    .map(|entry| entry.file().display().to_string()),
            )
            .finish()
    }
}

impl ManagedCredentials {
    pub fn add(&mut self, credential: Arc<dyn ManagedCredential>) {
        self.entries.push(credential);
    }

    /// The managed credential a route to `host` (over TLS) reading `source`
    /// uses, if any: one that manages exactly that file and is sent to that
    /// host. A route to another host gets none, so it reads the file as a
    /// plain credential, which refuses a managed value.
    pub fn find(
        &self,
        source: &CredentialSource,
        host: &str,
    ) -> Option<Arc<dyn ManagedCredential>> {
        let CredentialSource::File(path) = source else {
            return None;
        };
        let canonical = std::fs::canonicalize(path).ok();
        self.entries
            .iter()
            .find(|entry| {
                entry.host() == host
                    && (entry.file() == path
                        || canonical.is_some()
                            && std::fs::canonicalize(entry.file()).ok() == canonical)
            })
            .cloned()
    }

    /// The managed credential `route` uses, if any: [`Self::find`] for its
    /// credential and host, when it reaches that host over TLS.
    pub fn for_route(&self, route: &Route) -> Option<Arc<dyn ManagedCredential>> {
        if route.upstream != RouteUpstream::Tls {
            return None;
        }
        let credential = route.credential.as_ref()?;
        self.find(&credential.source, &route.host)
    }
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

/// Where a route's requests go once the broker has checked them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteUpstream {
    /// The route host over TLS, at the addresses the decision point
    /// resolved and classified (`sandbox.egress.routes`).
    Tls,
    /// Plain HTTP to `127.0.0.1:<port>` on this computer
    /// (`sandbox.egress.host_ollama`). The decision point does not resolve
    /// the route host; nothing but this port is reachable through it.
    HostLoopback { port: u16 },
}

/// Whether a route reads request bodies and refuses those that ask the
/// model provider to reach other hosts or run tools itself
/// ([`super::provider_fetch`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ProviderFetch {
    /// A host that is not a model API, without `provider_fetch`: bodies
    /// stream unread, and the route's policy does not mention it.
    #[default]
    NotChecked,
    /// `provider_fetch: allow`: bodies stream unread.
    Allowed,
    /// `provider_fetch: refuse`, or a model API host without
    /// `provider_fetch`: each body is read whole and checked before the
    /// request is recorded as allowed or sent.
    Refused,
}

impl ProviderFetch {
    /// The route's policy: `provider_fetch` when it is set, otherwise
    /// `refuse` on a model API host.
    pub fn for_route(host: &str, setting: Option<ProviderFetchYaml>) -> Self {
        match (setting, is_model_api_host(host)) {
            (Some(ProviderFetchYaml::Refuse), _) | (None, true) => Self::Refused,
            (Some(ProviderFetchYaml::Allow), _) => Self::Allowed,
            (None, false) => Self::NotChecked,
        }
    }

    /// Whether bodies are read and checked.
    pub fn checks(self) -> bool {
        self == Self::Refused
    }
}

/// Where a route of a Session's policy comes from, so a refusal names the
/// place a person changes it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum RouteOrigin {
    /// `sandbox.egress.routes` of the configuration.
    #[default]
    Configured,
    /// The `routes` of the loadout the Session runs.
    Loadout,
    /// Added by Axocoatl for the loadout's external writer, whose program
    /// (`runtime`: `claude-code`, `codex`) calls its model API through it.
    ExternalWriter { runtime: String },
}

/// One compiled route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    /// Position in the Session's routes: `sandbox.egress.routes`, then a
    /// loadout's.
    pub index: usize,
    /// Where the route comes from.
    pub origin: RouteOrigin,
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
    /// Whether request bodies are checked for provider-side fetches.
    pub provider_fetch: ProviderFetch,
    /// Where checked requests go.
    pub upstream: RouteUpstream,
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
        let provider_fetch = ProviderFetch::for_route(&host, route.provider_fetch);
        Ok(Self {
            index,
            origin: RouteOrigin::Configured,
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
            provider_fetch,
            upstream: RouteUpstream::Tls,
        })
    }

    /// The route of `sandbox.egress.host_ollama`, after the `index` routes of
    /// `sandbox.egress.routes`: the requests [`HOST_OLLAMA_RULES`] lists on
    /// [`HOST_OLLAMA_ROUTE_HOST`] for the processes it lists, no credential,
    /// to `127.0.0.1:<port>`, with bodies checked for cloud models
    /// (`provider_fetch::check_ollama`).
    pub fn host_ollama(index: usize, route: &HostOllamaRouteYaml) -> Result<Self, String> {
        if route.port == 0 {
            return Err("sandbox.egress.host_ollama.port: 1-65535".into());
        }
        Ok(Self {
            index,
            origin: RouteOrigin::Configured,
            host: HOST_OLLAMA_ROUTE_HOST.to_string(),
            ports: vec![HOST_OLLAMA_ROUTE_PORT],
            bindings: host_ollama_bindings(route)
                .into_iter()
                .map(binding_kind)
                .collect(),
            credential: None,
            rules: Rules::List(
                HOST_OLLAMA_RULES
                    .iter()
                    .map(|(methods, path)| {
                        Ok(CompiledRule {
                            methods: methods.iter().map(|method| (*method).to_string()).collect(),
                            path: parse_path_glob(path)?,
                            query: Vec::new(),
                        })
                    })
                    .collect::<Result<_, String>>()?,
            ),
            upstream_roots: Vec::new(),
            upstream_roots_id: None,
            env_placeholders: Vec::new(),
            allow_encoded_responses: false,
            allow_set_cookie: false,
            max_request_bytes: HOST_ROUTE_MAX_REQUEST_BYTES,
            provider_fetch: ProviderFetch::Refused,
            upstream: RouteUpstream::HostLoopback { port: route.port },
        })
    }

    /// The loopback port of a `host_ollama` route.
    pub fn host_loopback_port(&self) -> Option<u16> {
        match self.upstream {
            RouteUpstream::HostLoopback { port } => Some(port),
            RouteUpstream::Tls => None,
        }
    }

    /// `route#<index>` (or `host_ollama`), as the record names it.
    pub fn label(&self) -> String {
        match self.upstream {
            RouteUpstream::Tls => format!("route#{}", self.index),
            RouteUpstream::HostLoopback { .. } => "host_ollama".into(),
        }
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
        let mut canonical = serde_json::json!({
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
        });
        // Only a host route names its upstream, so the digest of every
        // policy without one is what it was before host routes existed.
        if let RouteUpstream::HostLoopback { port } = self.upstream {
            canonical["upstream"] = serde_json::json!({ "host_loopback": port });
        }
        // Likewise only a route to a model API, or one that sets it, names
        // its provider_fetch.
        match self.provider_fetch {
            ProviderFetch::NotChecked => {}
            ProviderFetch::Allowed => canonical["provider_fetch"] = "allow".into(),
            ProviderFetch::Refused => canonical["provider_fetch"] = "refuse".into(),
        }
        canonical
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
        let upstream = match self.upstream {
            RouteUpstream::HostLoopback { port } => format!(" to 127.0.0.1:{port}"),
            RouteUpstream::Tls => String::new(),
        };
        let provider_fetch = match self.provider_fetch {
            ProviderFetch::NotChecked => "",
            ProviderFetch::Allowed => ", provider fetches allowed",
            ProviderFetch::Refused => ", provider fetches refused",
        };
        format!(
            "{}:{ports} ({}{upstream}: {rules}{credential}, for {kinds}{provider_fetch})",
            self.host,
            self.label()
        )
    }

    /// What to do about a request body this route refused because it asks
    /// the provider to reach other hosts or run tools itself.
    pub fn provider_fetch_hint(&self) -> String {
        let host = &self.host;
        if self.upstream != RouteUpstream::Tls {
            return "The route to Ollama on this computer refuses a cloud model (a tag that is \
                    cloud or ends in -cloud), which Ollama runs on ollama.com, and a request that \
                    asks for a tool or content the provider fetches itself. Use a model on this \
                    computer."
                .into();
        }
        match &self.origin {
            RouteOrigin::ExternalWriter { runtime } => format!(
                "This is the route Axocoatl adds for the loadout's {runtime} writer, which refuses \
                 requests that ask the model provider to fetch URLs, search the web, connect to \
                 MCP servers or run tools itself. The program's own tools run in the Session \
                 container, under its egress policy."
            ),
            RouteOrigin::Configured => format!(
                "Send the request without it. If the provider may reach other hosts for requests \
                 on this route, set provider_fetch: allow on sandbox.egress.routes[{}] ({host}), \
                 then run axocoatl network reload.",
                self.index
            ),
            RouteOrigin::Loadout => format!(
                "Send the request without it. If the provider may reach other hosts for requests \
                 on this route, set provider_fetch: allow on the loadout's route for {host}, then \
                 run the loadout again."
            ),
        }
    }

    /// What to do about a request in the AI SDK's own gateway protocol,
    /// which this route refuses unread
    /// ([`super::provider_fetch::ai_sdk_gateway`]): use the gateway's
    /// OpenAI-compatible API, which the route checks, or turn the check
    /// off, and what that gives up.
    pub fn ai_sdk_gateway_hint(&self) -> String {
        let host = &self.host;
        let compatible = "Use the gateway's OpenAI-compatible API instead \
                          (https://ai-gateway.vercel.sh/v1, POST /v1/chat/completions), whose \
                          bodies the route checks: for an e2e check, build the model in \
                          e2e.config.ts with createOpenAICompatible from \
                          @ai-sdk/openai-compatible, baseURL process.env.AXOCOATL_E2E_BASE_URL \
                          and apiKey process.env.AXOCOATL_E2E_API_KEY, not gateway() from ai.";
        let gives_up = "sends every body to the gateway unread, so a request could then ask the \
                        gateway or the provider behind it to fetch URLs, search the web, connect \
                        to MCP servers or run tools itself";
        match &self.origin {
            RouteOrigin::ExternalWriter { runtime } => format!(
                "{compatible} This is the route Axocoatl adds for the loadout's {runtime} \
                 writer, which never sends bodies unread."
            ),
            RouteOrigin::Configured => format!(
                "{compatible} Or set provider_fetch: allow on sandbox.egress.routes[{}] \
                 ({host}) and run axocoatl network reload; the route then {gives_up}.",
                self.index
            ),
            RouteOrigin::Loadout => format!(
                "{compatible} Or set provider_fetch: allow on the loadout's route for {host}; \
                 the route then {gives_up}."
            ),
        }
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
        let rule = format!("{{methods: [{method}], path: \"{shown}\"}}");
        let hint = match (&self.upstream, &self.origin) {
            (RouteUpstream::HostLoopback { .. }, _) => {
                "The route to Ollama on this computer allows only Ollama's inference and model \
                 listing (GET /api/tags, /api/ps, /api/version, /api/status and /v1/models; POST \
                 /api/chat, /api/generate, /api/embed, /api/embeddings, /api/show and \
                 /v1/chat/completions, /v1/completions, /v1/embeddings, /v1/responses and \
                 /v1/messages). Pulling, pushing, creating, copying and deleting models, its \
                 account endpoints and its web search and fetch are refused: run ollama pull \
                 on this computer instead."
                    .into()
            }
            (RouteUpstream::Tls, RouteOrigin::Configured) => format!(
                "Add a rule to sandbox.egress.routes[{}] such as {rule}, \
                 then run axocoatl network reload.",
                self.index
            ),
            (RouteUpstream::Tls, RouteOrigin::Loadout) => format!(
                "This route is the loadout's: add a rule such as {rule} to the loadout's route \
                 for {}, then run the loadout again.",
                self.host
            ),
            (RouteUpstream::Tls, RouteOrigin::ExternalWriter { runtime }) => format!(
                "This route is the one Axocoatl adds for the loadout's {runtime} writer, and it \
                 allows only the program's model calls. To allow more, list a route for {} with \
                 a rule such as {rule} in the loadout's routes, which then replaces this one, and \
                 run the loadout again.",
                self.host
            ),
        };
        RuleDecision::Denied {
            reason: format!("no rule of {label} ({}) allows {method} {shown}", self.host),
            hint,
        }
    }

    /// What to change when a connection to this route comes from a kind of
    /// process its `for:` list does not name.
    pub fn not_for_binding_hint(&self, port: u16) -> String {
        let host = &self.host;
        match &self.origin {
            RouteOrigin::Configured => format!(
                "{host}:{port} is an egress route that does not serve this kind of process. Add \
                 the kind to the route's for: list in sandbox.egress.routes."
            ),
            RouteOrigin::Loadout => format!(
                "{host}:{port} is a route of the loadout that does not serve this kind of \
                 process. Add the kind to the route's for: list in the loadout's routes, then \
                 run the loadout again."
            ),
            RouteOrigin::ExternalWriter { runtime } => format!(
                "{host}:{port} is the route Axocoatl adds for the loadout's {runtime} writer, \
                 and it serves only Agents' processes, such as the writer's program."
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
        Self::compile_with_host_ollama(routes, credentials, workspaces, None)
    }

    /// Compile `sandbox.egress.routes` and, when set,
    /// `sandbox.egress.host_ollama` after them. A configured route can never
    /// claim the host route's name.
    pub fn compile_with_host_ollama(
        routes: &[EgressRouteYaml],
        credentials: &BTreeMap<String, CredentialSourceYaml>,
        workspaces: &[PathBuf],
        host_ollama: Option<&HostOllamaRouteYaml>,
    ) -> Result<Self, String> {
        let mut compiled: Vec<Arc<Route>> = routes
            .iter()
            .enumerate()
            .map(|(index, route)| {
                Route::compile(index, route, credentials, workspaces).map(Arc::new)
            })
            .collect::<Result<_, String>>()?;
        if let Some(route) = compiled
            .iter()
            .find(|route| axocoatl_config::egress_host_ollama::is_reserved_route_host(&route.host))
        {
            return Err(format!(
                "sandbox.egress.routes[{}].host: {} is a name Axocoatl reserves",
                route.index, route.host
            ));
        }
        if let Some(route) = host_ollama {
            compiled.push(Arc::new(Route::host_ollama(compiled.len(), route)?));
        }
        Ok(Self { routes: compiled })
    }

    /// The table with each route's origin set from `origins`, by position;
    /// a route past its end is configured.
    pub fn with_origins(mut self, origins: &[RouteOrigin]) -> Self {
        for (route, origin) in self.routes.iter_mut().zip(origins) {
            Arc::make_mut(route).origin = origin.clone();
        }
        self
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
