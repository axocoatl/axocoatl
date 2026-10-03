//! Validation for `sandbox.egress.routes` and `credentials`, and the check
//! that keeps `${...}` substitution out of them.
//!
//! A route names a host whose HTTPS connections the daemon ends itself, so
//! it can check each request's method, path and query against the route's
//! rules and add a credential. A credential names where the daemon reads its
//! value (an environment variable of the daemon or an owner-only file); the
//! config never holds the value. [`refuse_substitution`] runs on the raw
//! YAML, before `${VAR}` substitution, so a value cannot reach the config
//! that way either.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::path::{Component, Path, PathBuf};

use axocoatl_core::netaddr;

use crate::egress::{validate_ports, ConfigWarning, HostPattern};
use crate::egress_presets::preset;
use crate::error::ConfigError;
use crate::types::{
    AxocoatlConfig, CredentialSourceYaml, EgressAllowYaml, EgressRouteYaml, RouteForYaml,
    RouteInjectYaml, RouteRuleYaml,
};

/// Most routes one config may list.
pub const MAX_ROUTES: usize = 64;
/// Most rules one route may list.
pub const MAX_ROUTE_RULES: usize = 64;
/// Most methods one rule may list.
pub const MAX_RULE_METHODS: usize = 16;
/// Most query parameters one rule may require.
pub const MAX_RULE_QUERY: usize = 16;
/// Most environment placeholders one route may set.
pub const MAX_ENV_PLACEHOLDERS: usize = 16;
/// Most entries under `credentials`.
pub const MAX_CREDENTIALS: usize = 64;
/// Longest path glob, in characters.
pub const MAX_PATH_GLOB_CHARS: usize = 512;
/// Longest required query value, in characters.
pub const MAX_QUERY_VALUE_CHARS: usize = 512;
/// Smallest and largest `max_request_bytes`.
pub const MIN_MAX_REQUEST_BYTES: u64 = 1024;
pub const MAX_MAX_REQUEST_BYTES: u64 = 1 << 40;
/// Methods `access: read-only` allows.
pub const READ_ONLY_METHODS: [&str; 3] = ["GET", "HEAD", "OPTIONS"];
/// Largest credential file read.
pub const MAX_SECRET_FILE_BYTES: u64 = 64 * 1024;
/// Largest `upstream_ca` file read.
pub const MAX_CA_FILE_BYTES: u64 = 1024 * 1024;
/// Longest credential file or `upstream_ca` path.
const MAX_PATH_BYTES: usize = 4096;

/// Environment variables a route's `env_placeholders` cannot set: the
/// proxy and trust variables Axocoatl sets itself, and the basics of a
/// process environment.
const RESERVED_ENV: &[&str] = &[
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "CURL_CA_BUNDLE",
    "REQUESTS_CA_BUNDLE",
    "PIP_CERT",
    "GIT_SSL_CAINFO",
    "CARGO_HTTP_CAINFO",
    "NODE_EXTRA_CA_CERTS",
    "DENO_CERT",
    "HOME",
    "PATH",
    "USER",
    "SHELL",
    "TMPDIR",
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
];

/// Request headers an `inject.header` cannot name: framing, connection and
/// proxy headers, and the ones the broker sets itself.
const RESERVED_HEADERS: &[&str] = &[
    "host",
    "content-length",
    "transfer-encoding",
    "connection",
    "keep-alive",
    "upgrade",
    "te",
    "trailer",
    "expect",
    "proxy-authorization",
    "proxy-authenticate",
    "proxy-connection",
    "accept-encoding",
    "range",
    "if-range",
];

const BROAD_CREDENTIAL_WARNING: &str =
    "the credential is added to requests for every path on this host; list the paths it is for";
const ENCODED_WARNING: &str = "compressed responses pass without the check that the credential \
                               is not sent back to the container";

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

/// An environment variable name a credential or placeholder may use.
pub fn is_valid_env_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 128
        && (bytes[0].is_ascii_uppercase() || bytes[0] == b'_')
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || *byte == b'_')
}

/// Whether `env_placeholders` may not set `name`.
pub fn is_reserved_env(name: &str) -> bool {
    RESERVED_ENV.contains(&name) || name.starts_with("AXOCOATL_")
}

/// A credential name: 1-64 letters, digits, `_`, `.` or `-`, starting with
/// a letter or digit. It is recorded with each request that used it.
pub fn is_valid_credential_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
}

fn is_tchar(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
                | b'-'
        )
}

/// A header name an `inject.header` may use.
pub fn is_injectable_header(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 64 || !name.bytes().all(is_tchar) {
        return Err("a header name is 1-64 letters, digits or !#$%&'*+.^_`|~-".into());
    }
    let lower = name.to_ascii_lowercase();
    if RESERVED_HEADERS.contains(&lower.as_str()) || lower.starts_with("x-axocoatl") {
        return Err(format!(
            "{name} is a header Axocoatl or HTTP itself controls; it cannot carry a credential"
        ));
    }
    Ok(())
}

/// A method name a rule may list: an uppercase HTTP token.
fn check_method(method: &str) -> Result<(), String> {
    if method.is_empty()
        || method.len() > 32
        || !method
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'-')
        || !method.as_bytes()[0].is_ascii_uppercase()
    {
        return Err(
            "methods are uppercase names such as GET or POST; HTTP methods are case-sensitive"
                .into(),
        );
    }
    if method == "CONNECT" {
        return Err("CONNECT is never forwarded on a route".into());
    }
    Ok(())
}

/// One segment of a path glob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GlobSegment {
    /// The segment itself, with percent-escapes in uppercase hex.
    Literal(String),
    /// `*`: exactly one non-empty segment.
    One,
    /// `**`: any number of segments, including none.
    Any,
}

/// A parsed path glob, such as `/acme/*/info/refs` or `/v2/**`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathGlob {
    pub segments: Vec<GlobSegment>,
}

impl PathGlob {
    /// Whether a canonical path's segments match. `segments` are the parts
    /// of the path after its leading `/`, split on `/`, with percent-escapes
    /// in uppercase hex (a trailing `/` gives a last empty segment).
    pub fn matches(&self, segments: &[&str]) -> bool {
        fn walk(glob: &[GlobSegment], path: &[&str]) -> bool {
            match glob.split_first() {
                None => path.is_empty(),
                Some((GlobSegment::Any, rest)) => {
                    (0..=path.len()).any(|skip| walk(rest, &path[skip..]))
                }
                Some((GlobSegment::One, rest)) => path
                    .split_first()
                    .is_some_and(|(first, tail)| !first.is_empty() && walk(rest, tail)),
                Some((GlobSegment::Literal(literal), rest)) => path
                    .split_first()
                    .is_some_and(|(first, tail)| first == literal && walk(rest, tail)),
            }
        }
        walk(&self.segments, segments)
    }

    /// Whether the first segment is `**`, so the glob covers every path.
    pub fn starts_with_any(&self) -> bool {
        matches!(self.segments.first(), Some(GlobSegment::Any))
    }
}

impl fmt::Display for PathGlob {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for segment in &self.segments {
            formatter.write_str("/")?;
            match segment {
                GlobSegment::Literal(literal) => formatter.write_str(literal)?,
                GlobSegment::One => formatter.write_str("*")?,
                GlobSegment::Any => formatter.write_str("**")?,
            }
        }
        Ok(())
    }
}

/// Bytes a path segment may hold besides percent-escapes (RFC 3986 `pchar`).
pub fn is_path_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'-' | b'.'
                | b'_'
                | b'~'
                | b'!'
                | b'$'
                | b'&'
                | b'\''
                | b'('
                | b')'
                | b'+'
                | b','
                | b';'
                | b'='
                | b':'
                | b'@'
        )
}

/// Whether two hex digits (already uppercase) escape `/`, `\` or `.`.
fn escapes_separator_or_dot(high: u8, low: u8) -> bool {
    matches!((high, low), (b'2', b'F') | (b'5', b'C') | (b'2', b'E'))
}

/// Check one path segment's characters and percent-escapes and return it with
/// escapes in uppercase hex. An escaped `/`, `\` or `.` is refused: a server
/// may decode it into a separator or a dot segment. So is one escaped twice
/// (`%252F`), for servers that decode twice.
///
/// Servlet containers such as Tomcat drop path parameters (from a `;` to the
/// end of the segment) before they read dot segments, so `..;x` is `..` to
/// them. The part before the first `;` (or `%3B`) is therefore checked too: it
/// cannot be `.` or `..`, and cannot be empty when a parameter follows.
pub fn canonical_segment(segment: &str) -> Result<String, String> {
    let bytes = segment.as_bytes();
    let mut out = String::with_capacity(segment.len());
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'%' {
            let (Some(high), Some(low)) = (bytes.get(index + 1), bytes.get(index + 2)) else {
                return Err("'%' must start a two-digit escape such as %20".into());
            };
            if !high.is_ascii_hexdigit() || !low.is_ascii_hexdigit() {
                return Err("'%' must start a two-digit escape such as %20".into());
            }
            let high = high.to_ascii_uppercase();
            let low = low.to_ascii_uppercase();
            if escapes_separator_or_dot(high, low) {
                return Err("an escaped '/', '\\' or '.' (%2F, %5C, %2E) is refused".into());
            }
            if (high, low) == (b'2', b'5') {
                if let (Some(next_high), Some(next_low)) =
                    (bytes.get(index + 3), bytes.get(index + 4))
                {
                    if escapes_separator_or_dot(
                        next_high.to_ascii_uppercase(),
                        next_low.to_ascii_uppercase(),
                    ) {
                        return Err(
                            "a twice-escaped '/', '\\' or '.' (%252F, %255C, %252E) is refused"
                                .into(),
                        );
                    }
                }
            }
            out.push('%');
            out.push(high as char);
            out.push(low as char);
            index += 3;
            continue;
        }
        if !is_path_char(byte) {
            return Err(format!(
                "{:?} is not allowed in a path; escape it as %{byte:02X}",
                byte as char
            ));
        }
        out.push(byte as char);
        index += 1;
    }
    if out == "." || out == ".." {
        return Err("'.' and '..' segments are refused".into());
    }
    let name_end = [out.find(';'), out.find("%3B")]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(out.len());
    if name_end < out.len() {
        let name = &out[..name_end];
        if name == "." || name == ".." {
            return Err(
                "a '.' or '..' segment with a ';' parameter is refused: servers that drop path parameters read it as '.' or '..'"
                    .into(),
            );
        }
        if name.is_empty() {
            return Err(
                "a segment that starts with ';' is refused: servers that drop path parameters read it as an empty segment"
                    .into(),
            );
        }
    }
    Ok(out)
}

/// Parse a rule's path glob.
pub fn parse_path_glob(glob: &str) -> Result<PathGlob, String> {
    if glob.chars().count() > MAX_PATH_GLOB_CHARS {
        return Err(format!("at most {MAX_PATH_GLOB_CHARS} characters"));
    }
    let Some(rest) = glob.strip_prefix('/') else {
        return Err("a path starts with '/'".into());
    };
    let parts: Vec<&str> = rest.split('/').collect();
    let last = parts.len() - 1;
    let mut segments = Vec::with_capacity(parts.len());
    for (index, part) in parts.iter().enumerate() {
        if part.is_empty() && index != last {
            return Err("'//' never matches; requests with it are refused".into());
        }
        match *part {
            "*" => segments.push(GlobSegment::One),
            "**" => segments.push(GlobSegment::Any),
            part if part.contains('*') => {
                return Err(
                    "'*' and '**' must be whole segments, such as /repos/*/info/refs".into(),
                )
            }
            part => segments.push(GlobSegment::Literal(canonical_segment(part)?)),
        }
    }
    Ok(PathGlob { segments })
}

fn check_query_part(part: &str, what: &str) -> Result<(), String> {
    if part.is_empty() || part.chars().count() > MAX_QUERY_VALUE_CHARS {
        return Err(format!(
            "a query {what} is 1-{MAX_QUERY_VALUE_CHARS} characters"
        ));
    }
    if part
        .bytes()
        .any(|byte| !(0x21..=0x7e).contains(&byte) || matches!(byte, b'&' | b'=' | b'#'))
    {
        return Err(format!(
            "a query {what} is printable ASCII without spaces, '&', '=' or '#', as it reads after decoding"
        ));
    }
    Ok(())
}

/// Expand a leading `~/` with `$HOME`. The result must be absolute and free
/// of `.` and `..` components.
pub fn expand_user_path(path: &str) -> Result<PathBuf, String> {
    if path.is_empty() || path.len() > MAX_PATH_BYTES || path.contains('\0') {
        return Err(format!("a path is 1-{MAX_PATH_BYTES} bytes"));
    }
    let expanded = if let Some(rest) = path.strip_prefix("~/") {
        let home = std::env::var_os("HOME").ok_or("HOME is not set, so ~ cannot be expanded")?;
        PathBuf::from(home).join(rest)
    } else {
        PathBuf::from(path)
    };
    if !expanded.is_absolute() {
        return Err("write an absolute path or one starting with ~/".into());
    }
    if expanded
        .components()
        .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err("'.' and '..' are not allowed in the path".into());
    }
    Ok(expanded)
}

/// Check that `path` is a regular file, not a symbolic link, that only its
/// owner can read or write, at most `max_bytes` long.
pub fn check_owner_only_file(path: &Path, max_bytes: u64) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("{} cannot be read: {error}", path.display()))?;
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "{} is a symbolic link; name the file itself",
            path.display()
        ));
    }
    if !metadata.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(format!(
                "{} has mode {mode:o}; other users can read or change it. Run chmod 600 on it",
                path.display()
            ));
        }
    }
    if metadata.len() > max_bytes {
        return Err(format!(
            "{} is larger than {max_bytes} bytes",
            path.display()
        ));
    }
    Ok(())
}

/// Validate `credentials`. Values are not read here: they are read when a
/// request needs them, so a missing variable or file is reported by
/// `doctor`, not refused.
pub fn validate_credentials(config: &AxocoatlConfig) -> Result<Vec<ConfigWarning>, ConfigError> {
    if config.credentials.len() > MAX_CREDENTIALS {
        return Err(invalid(
            "credentials".into(),
            config.credentials.len(),
            format!("at most {MAX_CREDENTIALS} credentials"),
            "Remove credentials no route uses.",
        ));
    }
    let mut warnings = Vec::new();
    let used: HashSet<&str> = config
        .sandbox
        .egress
        .iter()
        .flat_map(|egress| egress.routes.iter())
        .filter_map(|route| route.credential.as_deref())
        .collect();
    for (name, source) in &config.credentials {
        let field = format!("credentials.{name}");
        if !is_valid_credential_name(name) {
            return Err(invalid(
                field,
                name,
                "a credential name is 1-64 letters, digits, '_', '.' or '-', starting with a letter or digit",
                "Rename it, for example github or registry.",
            ));
        }
        check_credential_source(&field, source)?;
        if !used.contains(name.as_str()) {
            warnings.push(ConfigWarning {
                field,
                message: "no route uses this credential".into(),
            });
        }
    }
    warnings.extend(inherited_credential_warnings(config));
    Ok(warnings)
}

/// A stdio MCP server that inherits the daemon's environment also gets every
/// credential read from it, and a tool of that server can hand it to an
/// Agent. One warning per such server.
fn inherited_credential_warnings(config: &AxocoatlConfig) -> Vec<ConfigWarning> {
    let variables: BTreeSet<&str> = config
        .credentials
        .values()
        .filter_map(|source| source.env.as_deref())
        .collect();
    let variables: Vec<&str> = variables.into_iter().collect();
    if variables.is_empty() {
        return Vec::new();
    }
    config
        .mcp_servers
        .iter()
        .filter(|server| server.transport == "stdio" && server.inherit_env)
        .map(|server| ConfigWarning {
            field: format!("mcp_servers[{}].inherit_env", server.name),
            message: format!(
                "this stdio MCP server starts with the daemon's whole environment, including the \
                 credential variables {}, and its tools can return them to Agents; set \
                 inherit_env: false, or keep those credentials in files",
                variables.join(", ")
            ),
        })
        .collect()
}

fn check_credential_source(field: &str, source: &CredentialSourceYaml) -> Result<(), ConfigError> {
    match (&source.env, &source.file) {
        (Some(variable), None) => {
            if !is_valid_env_name(variable) {
                // Not repeated: a value here is likely the credential.
                return Err(invalid(
                    format!("{field}.env"),
                    format_args!("<{} characters, not shown>", variable.chars().count()),
                    "an environment variable name is 1-128 uppercase letters, digits or '_', not starting with a digit",
                    "Write the variable's name, such as env: GITHUB_TOKEN, not its value.",
                ));
            }
            Ok(())
        }
        (None, Some(path)) => {
            expand_user_path(path).map_err(|reason| {
                invalid(
                    format!("{field}.file"),
                    path,
                    reason,
                    "Write an absolute path or ~/..., such as ~/.config/axocoatl/credentials/github.",
                )
            })?;
            Ok(())
        }
        _ => Err(invalid(
            field.to_string(),
            "{..}",
            "a credential names exactly one source: env or file",
            "Write {env: VARIABLE} or {file: ~/.config/axocoatl/credentials/NAME}.",
        )),
    }
}

/// Validate `sandbox.egress.routes` and `credentials`. Returns warnings for
/// routes that weaken the policy.
pub fn validate_egress_routes(config: &AxocoatlConfig) -> Result<Vec<ConfigWarning>, ConfigError> {
    let mut warnings = validate_credentials(config)?;
    let Some(egress) = &config.sandbox.egress else {
        return Ok(warnings);
    };
    let routes = &egress.routes;
    if routes.len() > MAX_ROUTES {
        return Err(invalid(
            "sandbox.egress.routes".into(),
            routes.len(),
            format!("at most {MAX_ROUTES} routes"),
            "Merge routes for the same host.",
        ));
    }
    let mut seen: HashMap<(String, u16), usize> = HashMap::new();
    let mut placeholders: HashMap<&str, usize> = HashMap::new();
    for (index, route) in routes.iter().enumerate() {
        let field = format!("sandbox.egress.routes[{index}]");
        let host = check_route(&field, route, &config.credentials, &mut warnings)?;
        let ports = validate_ports(route.ports.as_deref()).map_err(|reason| {
            invalid(
                format!("{field}.ports"),
                &route.ports,
                reason,
                "List ports 1-65535 once each, such as [443].",
            )
        })?;
        for port in ports {
            if let Some(other) = seen.insert((host.clone(), port), index) {
                return Err(invalid(
                    format!("{field}.host"),
                    &route.host,
                    format!("routes[{other}] already covers {host}:{port}"),
                    "List each host and port in one route.",
                ));
            }
        }
        for name in &route.env_placeholders {
            if let Some(other) = placeholders.insert(name.as_str(), index) {
                return Err(invalid(
                    format!("{field}.env_placeholders"),
                    name,
                    format!("routes[{other}] already sets {name}"),
                    "Set each placeholder in one route.",
                ));
            }
        }
        if let Some(allowed) = allowed_elsewhere(&host, &egress.allow) {
            warnings.push(ConfigWarning {
                field: format!("{field}.host"),
                message: format!(
                    "{host} is also allowed by sandbox.egress.allow ({allowed}); the route decides for its ports"
                ),
            });
        }
    }
    Ok(warnings)
}

/// The allow entry that also covers `host`, if any.
fn allowed_elsewhere(host: &str, allow: &[EgressAllowYaml]) -> Option<String> {
    allow.iter().find_map(|entry| match entry {
        EgressAllowYaml::Preset(name) => preset(name)
            .filter(|found| found.hosts.iter().any(|(listed, _)| *listed == host))
            .map(|_| format!("preset {name}")),
        EgressAllowYaml::Host(rule) => crate::egress::parse_host_pattern(&rule.host)
            .ok()
            .filter(|pattern| pattern.matches(host))
            .map(|pattern: HostPattern| pattern.to_string()),
        EgressAllowYaml::Cidr(_) => None,
    })
}

/// Check one route and return its normalized host.
fn check_route(
    field: &str,
    route: &EgressRouteYaml,
    credentials: &BTreeMap<String, CredentialSourceYaml>,
    warnings: &mut Vec<ConfigWarning>,
) -> Result<String, ConfigError> {
    let host_error = |reason: String| {
        invalid(
            format!("{field}.host"),
            &route.host,
            reason,
            "Write one exact host name, such as api.github.com.",
        )
    };
    if route.host.contains('*') {
        return Err(host_error(
            "a route names one exact host; wildcards are not allowed, because its certificate and Host checks are per host".into(),
        ));
    }
    if netaddr::parse_ip_literal(&route.host).is_some() {
        return Err(host_error("a route names a host, not an IP address".into()));
    }
    let host =
        netaddr::normalize_host_name(&route.host).map_err(|error| host_error(error.to_string()))?;

    let credentialed = match (&route.credential, &route.inject) {
        (Some(name), Some(inject)) => {
            if !credentials.contains_key(name) {
                return Err(invalid(
                    format!("{field}.credential"),
                    name,
                    "no credentials entry has this name",
                    "Add it under credentials:, for example credentials: {github: {env: GITHUB_TOKEN}}.",
                ));
            }
            check_inject(field, inject)?;
            true
        }
        (Some(_), None) => {
            return Err(invalid(
                format!("{field}.inject"),
                "missing",
                "a route with a credential says how to add it",
                "Add inject: {header: Authorization, format: \"Bearer {}\"} or inject: {basic: {username: NAME}}.",
            ))
        }
        (None, Some(_)) => {
            return Err(invalid(
                format!("{field}.inject"),
                "set",
                "inject needs a credential to add",
                "Add credential: NAME, or remove inject.",
            ))
        }
        (None, None) => false,
    };

    if let Some(bindings) = &route.bindings {
        if bindings.is_empty() {
            return Err(invalid(
                format!("{field}.for"),
                bindings,
                "for cannot be empty; omit it for [agent]",
                "List agent, terminal or setup.",
            ));
        }
        let unique: HashSet<RouteForYaml> = bindings.iter().copied().collect();
        if unique.len() != bindings.len() {
            return Err(invalid(
                format!("{field}.for"),
                bindings,
                "a kind is listed twice",
                "List each of agent, terminal and setup at most once.",
            ));
        }
    }

    match (&route.access, route.rules.is_empty()) {
        (Some(_), false) => {
            return Err(invalid(
                format!("{field}.access"),
                route.access,
                "access and rules cannot both be set",
                "Use access for a preset, or rules for exact requests.",
            ))
        }
        (None, true) => {
            return Err(invalid(
                format!("{field}.rules"),
                "[]",
                "a route needs rules or access; without them it refuses every request",
                "Add rules: [{methods: [GET], path: /**}] or access: read-only.",
            ))
        }
        // `read-only` also adds the credential to a GET of every path.
        (Some(_), true) if credentialed => warnings.push(ConfigWarning {
            field: format!("{field}.access"),
            message: BROAD_CREDENTIAL_WARNING.into(),
        }),
        _ => {}
    }
    if route.rules.len() > MAX_ROUTE_RULES {
        return Err(invalid(
            format!("{field}.rules"),
            route.rules.len(),
            format!("at most {MAX_ROUTE_RULES} rules"),
            "Use path globs to cover several paths in one rule.",
        ));
    }
    for (rule_index, rule) in route.rules.iter().enumerate() {
        let rule_field = format!("{field}.rules[{rule_index}]");
        let glob = check_rule(&rule_field, rule)?;
        if credentialed && glob.starts_with_any() {
            warnings.push(ConfigWarning {
                field: format!("{rule_field}.path"),
                message: BROAD_CREDENTIAL_WARNING.into(),
            });
        }
    }

    if let Some(path) = &route.upstream_ca {
        let expanded = expand_user_path(path).map_err(|reason| {
            invalid(
                format!("{field}.upstream_ca"),
                path,
                reason,
                "Write an absolute path or ~/... to a PEM file.",
            )
        })?;
        check_ca_file(&expanded).map_err(|reason| {
            invalid(
                format!("{field}.upstream_ca"),
                path,
                reason,
                "Point it at an owner-only (chmod 600) PEM file with the upstream's CA certificate.",
            )
        })?;
    }

    if route.env_placeholders.len() > MAX_ENV_PLACEHOLDERS {
        return Err(invalid(
            format!("{field}.env_placeholders"),
            route.env_placeholders.len(),
            format!("at most {MAX_ENV_PLACEHOLDERS} placeholders"),
            "List only the variables a tool checks for.",
        ));
    }
    for name in &route.env_placeholders {
        if !is_valid_env_name(name) || is_reserved_env(name) {
            return Err(invalid(
                format!("{field}.env_placeholders"),
                name,
                "a placeholder is an uppercase variable name that Axocoatl does not set itself (not a proxy, certificate, AXOCOATL_ or basic process variable)",
                "Use the variable your tool looks for, such as GITHUB_TOKEN.",
            ));
        }
    }

    if !(MIN_MAX_REQUEST_BYTES..=MAX_MAX_REQUEST_BYTES).contains(&route.max_request_bytes) {
        return Err(invalid(
            format!("{field}.max_request_bytes"),
            route.max_request_bytes,
            format!("must be {MIN_MAX_REQUEST_BYTES}-{MAX_MAX_REQUEST_BYTES}"),
            "Omit it for the default 1 GiB.",
        ));
    }
    if credentialed && route.allow_encoded_responses {
        warnings.push(ConfigWarning {
            field: format!("{field}.allow_encoded_responses"),
            message: ENCODED_WARNING.into(),
        });
    }
    Ok(host)
}

fn check_inject(field: &str, inject: &RouteInjectYaml) -> Result<(), ConfigError> {
    let field = format!("{field}.inject");
    match (&inject.basic, &inject.header) {
        (Some(basic), None) => {
            if inject.format.is_some() {
                return Err(invalid(
                    format!("{field}.format"),
                    &inject.format,
                    "format applies only to inject.header",
                    "Remove format; basic sends username:credential.",
                ));
            }
            let username = &basic.username;
            if username.is_empty()
                || username.len() > 256
                || username.contains(':')
                || !username.bytes().all(|byte| (0x20..=0x7e).contains(&byte))
            {
                return Err(invalid(
                    format!("{field}.basic.username"),
                    username,
                    "a username is 1-256 printable ASCII characters without ':'",
                    "For GitHub use x-access-token.",
                ));
            }
            Ok(())
        }
        (None, Some(header)) => {
            is_injectable_header(header).map_err(|reason| {
                invalid(
                    format!("{field}.header"),
                    header,
                    reason,
                    "Use Authorization or the header your service reads, such as X-Api-Key.",
                )
            })?;
            if let Some(format) = &inject.format {
                if format.matches("{}").count() != 1
                    || format.len() > 256
                    || !format.bytes().all(|byte| (0x20..=0x7e).contains(&byte))
                {
                    return Err(invalid(
                        format!("{field}.format"),
                        format,
                        "format is printable ASCII, at most 256 characters, with exactly one {} where the credential goes",
                        "For a bearer token write format: \"Bearer {}\".",
                    ));
                }
            }
            Ok(())
        }
        _ => Err(invalid(
            field,
            inject,
            "inject names exactly one of basic and header",
            "Write inject: {header: Authorization, format: \"Bearer {}\"} or inject: {basic: {username: NAME}}.",
        )),
    }
}

fn check_rule(field: &str, rule: &RouteRuleYaml) -> Result<PathGlob, ConfigError> {
    if rule.methods.is_empty() || rule.methods.len() > MAX_RULE_METHODS {
        return Err(invalid(
            format!("{field}.methods"),
            &rule.methods,
            format!("list 1-{MAX_RULE_METHODS} methods"),
            "For example methods: [GET, HEAD].",
        ));
    }
    let mut methods = HashSet::new();
    for method in &rule.methods {
        check_method(method).map_err(|reason| {
            invalid(
                format!("{field}.methods"),
                method,
                reason,
                "For example methods: [GET, HEAD].",
            )
        })?;
        if !methods.insert(method.as_str()) {
            return Err(invalid(
                format!("{field}.methods"),
                method,
                "a method is listed twice",
                "List each method once.",
            ));
        }
    }
    let glob = parse_path_glob(&rule.path).map_err(|reason| {
        invalid(
            format!("{field}.path"),
            &rule.path,
            reason,
            "Write a path such as /acme/app.git/info/refs, /v2/* or /packages/**.",
        )
    })?;
    if rule.query.len() > MAX_RULE_QUERY {
        return Err(invalid(
            format!("{field}.query"),
            rule.query.len(),
            format!("at most {MAX_RULE_QUERY} parameters"),
            "Require only the parameters that decide what the request does.",
        ));
    }
    for (key, value) in &rule.query {
        check_query_part(key, "parameter name")
            .and_then(|()| {
                if value == "*" {
                    Ok(())
                } else {
                    check_query_part(value, "value")
                }
            })
            .map_err(|reason| {
                invalid(
                    format!("{field}.query.{key}"),
                    value,
                    reason,
                    "Write the decoded value, such as service: git-upload-pack, or * for any value.",
                )
            })?;
    }
    Ok(glob)
}

/// An `upstream_ca` file: owner-only, and holding at least one PEM
/// certificate. The daemon parses the certificates when it builds the route.
fn check_ca_file(path: &Path) -> Result<(), String> {
    check_owner_only_file(path, MAX_CA_FILE_BYTES)?;
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("{} cannot be read: {error}", path.display()))?;
    let begin = text.contains("-----BEGIN CERTIFICATE-----");
    let end = text.contains("-----END CERTIFICATE-----");
    if !(begin && end) {
        return Err(format!(
            "{} holds no PEM certificate (-----BEGIN CERTIFICATE-----)",
            path.display()
        ));
    }
    Ok(())
}

/// Refuse `${...}` in `credentials` and `sandbox.egress.routes` of the raw
/// YAML, before substitution: a credential is named by where it lives, and
/// substitution would copy its value into the config.
pub fn refuse_substitution(raw: &str) -> Result<(), ConfigError> {
    let refusal = |field: String, value: &str| {
        invalid(
            field,
            value,
            "${...} is not substituted here: credentials and routes name where a credential lives, and substitution would put its value into the config",
            "Write {env: VARIABLE} to read an environment variable when a request needs it, or {file: PATH}.",
        )
    };
    if let Some(found) = credentials_block_substitution(raw) {
        return Err(refusal("credentials".into(), &found));
    }
    // Aliases and flow styles: look at the parsed values too. A document
    // that only parses after substitution was covered by the line scan.
    let Ok(value) = serde_yaml::from_str::<serde_yaml::Value>(raw) else {
        return Ok(());
    };
    if let Some(credentials) = value.get("credentials") {
        if let Some(found) = find_substitution(credentials) {
            return Err(refusal("credentials".into(), &found));
        }
    }
    if let Some(routes) = value
        .get("sandbox")
        .and_then(|sandbox| sandbox.get("egress"))
        .and_then(|egress| egress.get("routes"))
    {
        if let Some(found) = find_substitution(routes) {
            return Err(refusal("sandbox.egress.routes".into(), &found));
        }
    }
    Ok(())
}

/// The first string holding `${` in a YAML value, keys included.
fn find_substitution(value: &serde_yaml::Value) -> Option<String> {
    match value {
        serde_yaml::Value::String(text) if text.contains("${") => Some(text.clone()),
        serde_yaml::Value::Sequence(items) => items.iter().find_map(find_substitution),
        serde_yaml::Value::Mapping(map) => map
            .iter()
            .find_map(|(key, item)| find_substitution(key).or_else(|| find_substitution(item))),
        serde_yaml::Value::Tagged(tagged) => find_substitution(&tagged.value),
        _ => None,
    }
}

/// The first line of a top-level `credentials:` block that holds `${`.
fn credentials_block_substitution(raw: &str) -> Option<String> {
    let mut inside = false;
    for line in raw.lines() {
        let top_level = !line.starts_with([' ', '\t']) && !line.trim().is_empty();
        if top_level && !line.trim_start().starts_with('#') {
            inside = line
                .strip_prefix("credentials")
                .is_some_and(|rest| rest.trim_start().starts_with(':'))
                || line
                    .strip_prefix("\"credentials\"")
                    .or_else(|| line.strip_prefix("'credentials'"))
                    .is_some_and(|rest| rest.trim_start().starts_with(':'));
        }
        if !inside || line.trim_start().starts_with('#') {
            continue;
        }
        // A '#' after whitespace starts a comment.
        let content = line.find(" #").map_or(line, |at| &line[..at]);
        if content.contains("${") {
            return Some(content.trim().to_string());
        }
    }
    None
}

#[cfg(test)]
#[path = "egress_routes_tests.rs"]
mod tests;
