//! Axocoatl's secret store: owner-only files under
//! `{data root}/secrets/<name>`, written by `axocoatl secret set <name>`
//! from stdin (never argv) and read by the egress route broker per request
//! (`CredentialSource::File`). A value never enters a container, a record or
//! a log. Owner: workstream `agents`.
//!
//! Every operation goes through [`SecureDir`]: no symbolic link is followed,
//! the directory is `0700` and each file `0600`, and a value is replaced
//! atomically (a temporary file in the same directory, renamed over the old
//! one). Names follow the egress route credential rule
//! ([`is_valid_credential_name`]); values follow the broker's: 1 to
//! [`MAX_SECRET_BYTES`] bytes of UTF-8 without control characters, after one
//! trailing line break is removed. Nothing here formats a value into an
//! error, a log line or a `Debug` string.

use std::io::Read;
use std::path::{Path, PathBuf};

use axocoatl_config::egress_routes::is_valid_credential_name;
use axocoatl_config::CredentialSourceYaml;
use axocoatl_core::{SecureDir, SecureEntryType};
use zeroize::Zeroizing;

/// Directory under the data root.
pub const SECRETS_DIR: &str = "secrets";

/// Longest value, the broker's `MAX_SECRET_BYTES`.
pub const MAX_SECRET_BYTES: usize = crate::egress_broker::rules::MAX_SECRET_BYTES;

/// The most secrets one data root holds (the config's credential limit).
pub const MAX_SECRETS: usize = axocoatl_config::egress_routes::MAX_CREDENTIALS;

#[derive(Debug, thiserror::Error)]
pub enum SecretStoreError {
    #[error("secret store: {0}")]
    Invalid(String),
    #[error("secret store: {0}")]
    Io(#[from] std::io::Error),
}

/// The file that holds secret `name` under `data_dir`.
pub fn secret_path(data_dir: &Path, name: &str) -> PathBuf {
    data_dir.join(SECRETS_DIR).join(name)
}

fn check_name(name: &str) -> Result<(), SecretStoreError> {
    if is_valid_credential_name(name) {
        Ok(())
    } else {
        Err(SecretStoreError::Invalid(format!(
            "{name:?} is not a secret name: use 1-64 letters, digits, '_', '.' or '-', \
             starting with a letter or digit (for example claude-code-oauth)"
        )))
    }
}

/// The value as the broker will send it: one trailing `\n` or `\r\n`
/// removed, then 1 to [`MAX_SECRET_BYTES`] bytes of UTF-8 without control
/// characters. The error never contains the value.
pub fn normalize_secret_value(value: &[u8]) -> Result<Zeroizing<Vec<u8>>, SecretStoreError> {
    let trimmed = value
        .strip_suffix(b"\r\n")
        .or_else(|| value.strip_suffix(b"\n"))
        .unwrap_or(value);
    if trimmed.is_empty() {
        return Err(SecretStoreError::Invalid(
            "the value is empty; pipe it on stdin, for example: claude setup-token | axocoatl secret set claude-code-oauth"
                .into(),
        ));
    }
    if trimmed.len() > MAX_SECRET_BYTES {
        return Err(SecretStoreError::Invalid(format!(
            "the value is longer than {MAX_SECRET_BYTES} bytes"
        )));
    }
    if std::str::from_utf8(trimmed).is_err() {
        return Err(SecretStoreError::Invalid(
            "the value is not UTF-8 text".into(),
        ));
    }
    if trimmed.iter().any(|byte| byte.is_ascii_control()) {
        return Err(SecretStoreError::Invalid(
            "the value contains a line break or another control character; store one line".into(),
        ));
    }
    Ok(Zeroizing::new(trimmed.to_vec()))
}

/// Read a value from `reader` (stdin), at most [`MAX_SECRET_BYTES`] plus a
/// trailing line break, and normalize it. A longer input is refused without
/// reading it all.
pub fn read_secret_value(reader: impl Read) -> Result<Zeroizing<Vec<u8>>, SecretStoreError> {
    let ceiling = MAX_SECRET_BYTES as u64 + 3;
    let mut bytes = Zeroizing::new(Vec::with_capacity(1024));
    reader.take(ceiling).read_to_end(&mut bytes)?;
    if bytes.len() as u64 >= ceiling {
        return Err(SecretStoreError::Invalid(format!(
            "the value is longer than {MAX_SECRET_BYTES} bytes"
        )));
    }
    normalize_secret_value(&bytes)
}

fn open_data_root(data_dir: &Path) -> Result<SecureDir, SecretStoreError> {
    let root = SecureDir::open_existing_all(data_dir).map_err(|error| {
        SecretStoreError::Invalid(format!(
            "the data directory {} cannot be opened: {error}",
            data_dir.display()
        ))
    })?;
    #[cfg(unix)]
    {
        // SAFETY: geteuid has no preconditions and cannot fail.
        let uid = unsafe { libc::geteuid() };
        root.require_owner_and_private_writes(uid)?;
    }
    Ok(root)
}

/// The secrets directory, created `0700` when `create` is set.
fn secrets_dir(data_dir: &Path, create: bool) -> Result<Option<SecureDir>, SecretStoreError> {
    let root = open_data_root(data_dir)?;
    let directory = if create {
        root.child(SECRETS_DIR)?
    } else {
        match root.existing_child(SECRETS_DIR) {
            Ok(directory) => directory,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        }
    };
    #[cfg(unix)]
    {
        // SAFETY: geteuid has no preconditions and cannot fail.
        let uid = unsafe { libc::geteuid() };
        directory.require_owner_and_private_writes(uid)?;
    }
    directory.restrict_owner_only()?;
    Ok(Some(directory))
}

/// Store `value` as secret `name` (0600, atomic replace). `value` is
/// normalized first ([`normalize_secret_value`]).
pub fn set_secret(data_dir: &Path, name: &str, value: &[u8]) -> Result<(), SecretStoreError> {
    check_name(name)?;
    let value = normalize_secret_value(value)?;
    let directory = secrets_dir(data_dir, true)?.ok_or_else(|| {
        SecretStoreError::Invalid("the secrets directory could not be created".into())
    })?;
    let exists = directory.has_exact_file(name)?;
    if !exists && stored_names(&directory)?.len() >= MAX_SECRETS {
        return Err(SecretStoreError::Invalid(format!(
            "the store already holds {MAX_SECRETS} secrets; remove one first"
        )));
    }
    directory.atomic_write_with_mode(name, &value, 0o600)?;
    Ok(())
}

fn stored_names(directory: &SecureDir) -> Result<Vec<String>, SecretStoreError> {
    let mut names: Vec<String> = directory
        .entries_limited(MAX_SECRETS * 4 + 16)?
        .into_iter()
        .filter(|entry| entry.file_type == SecureEntryType::File)
        .filter_map(|entry| entry.name.into_string().ok())
        .filter(|name| is_valid_credential_name(name))
        .collect();
    names.sort();
    Ok(names)
}

/// Names of stored secrets (never values), sorted.
pub fn list_secrets(data_dir: &Path) -> Result<Vec<String>, SecretStoreError> {
    match secrets_dir(data_dir, false)? {
        Some(directory) => stored_names(&directory),
        None => Ok(Vec::new()),
    }
}

/// Remove secret `name`. `Ok(false)` when there was none.
pub fn remove_secret(data_dir: &Path, name: &str) -> Result<bool, SecretStoreError> {
    check_name(name)?;
    let Some(directory) = secrets_dir(data_dir, false)? else {
        return Ok(false);
    };
    if !directory.has_exact_file(name)? {
        return Ok(false);
    }
    directory.remove_file(name)?;
    directory.sync_all()?;
    Ok(true)
}

/// Whether secret `name` is stored (a regular file; never read here).
pub fn has_secret(data_dir: &Path, name: &str) -> Result<bool, SecretStoreError> {
    check_name(name)?;
    match secrets_dir(data_dir, false)? {
        Some(directory) => Ok(directory.has_exact_file(name)?),
        None => Ok(false),
    }
}

/// The credential source a route naming `name` uses when the config's
/// `credentials` block has no entry of that name: the stored secret's file,
/// read by the broker per request (`CredentialSource::File`). `None` when no
/// such secret is stored. Core's `loadout::egress::loadout_policy` adds it to
/// the Session's `credentials`.
pub fn credential_source(
    data_dir: &Path,
    name: &str,
) -> Result<Option<CredentialSourceYaml>, SecretStoreError> {
    if !has_secret(data_dir, name)? {
        return Ok(None);
    }
    Ok(Some(CredentialSourceYaml {
        env: None,
        file: Some(secret_path(data_dir, name).to_string_lossy().into_owned()),
    }))
}

/// [`credential_source`] for the secrets directory itself
/// (`{data root}/secrets`, `AxocoatlDaemon::secret_store_dir`), as
/// `loadout::egress::loadout_policy` receives it.
pub fn credential_source_in(
    secrets_dir: &Path,
    name: &str,
) -> Result<Option<CredentialSourceYaml>, SecretStoreError> {
    check_name(name)?;
    let directory = match SecureDir::open_existing_all(secrets_dir) {
        Ok(directory) => directory,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !directory.has_exact_file(name)? {
        return Ok(None);
    }
    Ok(Some(CredentialSourceYaml {
        env: None,
        file: Some(secrets_dir.join(name).to_string_lossy().into_owned()),
    }))
}

/// The names the external-agent routes for Claude Code add as their
/// credential (`claude-code-oauth`): an OAuth token from `claude setup-token`.
fn claude_code_route_secrets() -> Vec<String> {
    crate::external_agent::routes_for(axocoatl_config::loadout::AgentRuntime::ClaudeCode)
        .map(|routes| {
            routes
                .into_iter()
                .filter_map(|route| route.credential)
                .collect()
        })
        .unwrap_or_default()
}

/// The prefix of the OAuth token `claude setup-token` prints.
pub const CLAUDE_CODE_TOKEN_PREFIX: &str = "sk-ant-oat01-";

/// How to store secret `name` again: piping in only the token. For the
/// refusal of a credential a model API rejected and for `secret set`'s
/// warnings.
pub fn store_again_hint(name: &str) -> String {
    let token = if claude_code_route_secrets()
        .iter()
        .any(|secret| secret == name)
    {
        format!(
            " (the {CLAUDE_CODE_TOKEN_PREFIX}… token `claude setup-token` prints, with nothing \
             around it)"
        )
    } else if name == crate::external_agent::codex::CODEX_SECRET {
        " (the OpenAI API key, for example `printenv OPENAI_API_KEY | axocoatl secret set \
         codex-openai`)"
            .to_string()
    } else {
        String::new()
    };
    format!("store it again with `axocoatl secret set {name}`, piping in only the token{token}")
}

/// Why `value`, about to be stored as `name`, may not be the bare token its
/// route sends: whitespace inside it, a `Bearer ` prefix (the route adds the
/// scheme), JSON, several tokens, or, for a secret a Claude Code route sends,
/// no [`CLAUDE_CODE_TOKEN_PREFIX`]. Each reason is in words that never
/// contain the value; none of them stops the value from being stored.
pub fn value_warnings(name: &str, value: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(value);
    let text = text
        .strip_suffix("\r\n")
        .or_else(|| text.strip_suffix('\n'))
        .unwrap_or(&text);
    let mut warnings = Vec::new();
    let bearer = text
        .get(..7)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("bearer "));
    if bearer {
        warnings.push(
            "it starts with \"Bearer \": the route adds that itself, so store only the token"
                .to_string(),
        );
    }
    let token = if bearer { &text[7..] } else { text };
    let trimmed = token.trim();
    if trimmed.starts_with('{')
        || trimmed.starts_with('[')
        || (trimmed.len() > 1 && trimmed.starts_with('"') && trimmed.ends_with('"'))
    {
        warnings.push("it looks like JSON: store only the token, not what holds it".into());
    }
    if trimmed.chars().any(char::is_whitespace) {
        warnings.push("it has whitespace inside it, and a token has none".into());
    } else if token.len() != trimmed.len() {
        warnings.push("it starts or ends with whitespace".into());
    }
    let parts = trimmed
        .split(|c: char| c.is_whitespace() || matches!(c, ',' | ';'))
        .filter(|part| !part.is_empty())
        .count();
    let prefixes = trimmed.matches("sk-").count();
    if parts > 1 || prefixes > 1 {
        warnings.push(format!(
            "it looks like {} tokens or words, not one",
            parts.max(prefixes)
        ));
    }
    if claude_code_route_secrets()
        .iter()
        .any(|secret| secret == name)
        && !text.starts_with(CLAUDE_CODE_TOKEN_PREFIX)
    {
        warnings.push(format!(
            "Claude Code's route sends {name} as an OAuth token, which starts with \
             \"{CLAUDE_CODE_TOKEN_PREFIX}\" (the token `claude setup-token` prints), and this \
             value does not"
        ));
    }
    warnings
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A private data root, as the daemon and the CLI open it.
    fn data_root() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn set_list_remove_keeps_owner_only_files_and_never_echoes_a_value() {
        let root = data_root();
        assert_eq!(list_secrets(root.path()).unwrap(), Vec::<String>::new());
        set_secret(root.path(), "claude-code-oauth", b"sk-test-one\n").unwrap();
        set_secret(root.path(), "codex-openai", b"sk-test-two").unwrap();
        let path = secret_path(root.path(), "claude-code-oauth");
        assert_eq!(std::fs::read(&path).unwrap(), b"sk-test-one");
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(&root.path().join(SECRETS_DIR)), 0o700);
        assert_eq!(
            list_secrets(root.path()).unwrap(),
            ["claude-code-oauth", "codex-openai"]
        );
        // Replace in place, atomically: no temporary file is left behind.
        set_secret(root.path(), "claude-code-oauth", b"sk-test-three\r\n").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"sk-test-three");
        assert_eq!(mode(&path), 0o600);
        let entries: Vec<_> = std::fs::read_dir(root.path().join(SECRETS_DIR))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(entries.len(), 2, "{entries:?}");
        assert!(has_secret(root.path(), "codex-openai").unwrap());
        assert!(remove_secret(root.path(), "codex-openai").unwrap());
        assert!(!remove_secret(root.path(), "codex-openai").unwrap());
        assert_eq!(list_secrets(root.path()).unwrap(), ["claude-code-oauth"]);
        let source = credential_source(root.path(), "claude-code-oauth")
            .unwrap()
            .unwrap();
        assert_eq!(source.env, None);
        assert_eq!(source.file.as_deref(), Some(path.to_str().unwrap()));
        assert_eq!(
            credential_source(root.path(), "codex-openai").unwrap(),
            None
        );
        let secrets = root.path().join(SECRETS_DIR);
        assert_eq!(
            credential_source_in(&secrets, "claude-code-oauth").unwrap(),
            credential_source(root.path(), "claude-code-oauth").unwrap()
        );
        assert_eq!(
            credential_source_in(&secrets, "codex-openai").unwrap(),
            None
        );
        assert_eq!(
            credential_source_in(&root.path().join("missing"), "codex-openai").unwrap(),
            None
        );
    }

    #[test]
    fn bad_names_and_values_are_refused_without_the_value_in_the_error() {
        let root = data_root();
        for name in ["", "../x", "a/b", ".hidden", "-dash", &"a".repeat(65)] {
            assert!(set_secret(root.path(), name, b"value").is_err(), "{name:?}");
        }
        let long = vec![b'k'; MAX_SECRET_BYTES + 1];
        for value in [
            &b""[..],
            b"\n",
            b"two\nlines",
            b"tab\there",
            b"bell\x07",
            &[0xff, 0xfe][..],
            &long,
        ] {
            let error = set_secret(root.path(), "name", value)
                .unwrap_err()
                .to_string();
            if !value.is_empty() && value != b"\n" {
                let text = String::from_utf8_lossy(value);
                assert!(!error.contains(text.trim()), "{error}");
            }
        }
        // Exactly the limit is accepted, with or without one line break.
        let mut limit = vec![b'k'; MAX_SECRET_BYTES];
        set_secret(root.path(), "name", &limit).unwrap();
        limit.push(b'\n');
        set_secret(root.path(), "name", &limit).unwrap();
        assert_eq!(
            read_secret_value(&limit[..]).unwrap().len(),
            MAX_SECRET_BYTES
        );
        let too_long = vec![b'k'; MAX_SECRET_BYTES * 4];
        assert!(read_secret_value(&too_long[..]).is_err());
        assert_eq!(&*read_secret_value(&b"token\n"[..]).unwrap(), b"token");
    }

    #[test]
    fn a_link_or_a_shared_directory_is_refused() {
        let root = data_root();
        let elsewhere = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), root.path().join(SECRETS_DIR)).unwrap();
        assert!(set_secret(root.path(), "name", b"value").is_err());
        assert!(list_secrets(root.path()).is_err());
        assert_eq!(std::fs::read_dir(elsewhere.path()).unwrap().count(), 0);

        let root = data_root();
        set_secret(root.path(), "name", b"value").unwrap();
        let file = secret_path(root.path(), "name");
        std::fs::remove_file(&file).unwrap();
        std::os::unix::fs::symlink(elsewhere.path().join("target"), &file).unwrap();
        assert!(set_secret(root.path(), "name", b"value").is_err());
        assert!(!elsewhere.path().join("target").exists());

        let shared = tempfile::tempdir().unwrap();
        std::fs::set_permissions(shared.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(set_secret(shared.path(), "name", b"value").is_err());
    }

    /// The broker reads a stored secret as a route's `File` credential: it
    /// passes the owner-only checks, loses its trailing line break, and is
    /// refused once someone else can read it.
    #[test]
    fn the_broker_reads_a_stored_secret_as_a_file_credential() {
        use crate::egress_broker::CredentialSource;
        use secrecy::ExposeSecret;
        let root = data_root();
        set_secret(root.path(), "claude-code-oauth", b"sk-ant-oat01-test\n").unwrap();
        let source = credential_source(root.path(), "claude-code-oauth")
            .unwrap()
            .unwrap();
        let path = PathBuf::from(source.file.unwrap());
        let credential = CredentialSource::File(path.clone());
        let value = credential.read("claude-code-oauth", &[]).unwrap();
        assert_eq!(value.expose_secret(), "sk-ant-oat01-test");
        // A Workspace holding the data root would be refused.
        assert!(credential
            .read("claude-code-oauth", &[root.path().to_path_buf()])
            .is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let error = credential.read("claude-code-oauth", &[]).unwrap_err();
        assert!(!error.to_string().contains("sk-ant"), "{error}");
    }

    /// End to end through a Session's decision point and route broker: a
    /// route naming a stored secret (no `credentials` entry in the config)
    /// adds it upstream in place of the client's placeholder, and neither the
    /// env file nor the record holds it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_route_naming_a_stored_secret_injects_it_upstream() {
        use crate::egress_broker::UpstreamConnector;
        use crate::session_egress::route_tests::{loopback_is_public, RelaySidecar, Upstream};
        use crate::session_egress::tests::{FakeRecord, FakeResolver};
        use crate::session_egress::{EgressPolicyConfig, RouteSettings, SessionEgress};
        use axocoatl_isolation::egress::{EgressAuthority, GrantKind, GrantSpec, RequestKind};
        use axocoatl_session::network_record::{Decision, NetworkEvent};
        use http_body_util::{BodyExt, Empty};
        use std::sync::Arc;

        let root = data_root();
        let secret = format!("sk-ant-oat01-{}", uuid::Uuid::new_v4().simple());
        set_secret(
            root.path(),
            "claude-code-oauth",
            format!("{secret}\n").as_bytes(),
        )
        .unwrap();
        let upstream = Upstream::start("api.anthropic.test").await;
        let port = upstream.addr.port();
        let route: axocoatl_config::EgressRouteYaml = serde_yaml::from_str(&format!(
            "{{host: api.anthropic.test, ports: [{port}], credential: claude-code-oauth, \
              inject: {{header: Authorization, format: 'Bearer {{}}'}}, \
              env_placeholders: [CLAUDE_CODE_OAUTH_TOKEN], \
              rules: [{{methods: [GET], path: /allowed}}]}}"
        ))
        .unwrap();
        let mut credentials = std::collections::BTreeMap::new();
        credentials.insert(
            "claude-code-oauth".to_string(),
            credential_source(root.path(), "claude-code-oauth")
                .unwrap()
                .unwrap(),
        );
        let env_dir = tempfile::tempdir().unwrap();
        let record = Arc::new(FakeRecord::default());
        let egress = SessionEgress::open_session(
            "ses-secret-store",
            EgressPolicyConfig {
                routes: vec![route],
                credentials,
                ..EgressPolicyConfig::default()
            },
            record.clone(),
            FakeResolver::with(&[("api.anthropic.test", &["127.0.0.1"])]),
            Some(SecureDir::open(env_dir.path()).unwrap()),
            loopback_is_public,
            RouteSettings {
                upstream: Arc::new(UpstreamConnector::with_verifier(
                    upstream.verifier(),
                    Arc::new(|_| false),
                )),
                ..RouteSettings::default()
            },
        )
        .await
        .unwrap();
        let grant = egress
            .grant(GrantSpec {
                activation_id: Some("act-secret".into()),
                agent: Some("writer".into()),
                ..GrantSpec::new(GrantKind::Agent)
            })
            .await
            .unwrap();
        let env = std::fs::read_to_string(grant.env_file.as_ref().unwrap()).unwrap();
        assert!(!env.contains(&secret), "{env}");
        assert!(
            env.contains("CLAUDE_CODE_OAUTH_TOKEN=axocoatl-route:api.anthropic.test"),
            "{env}"
        );
        let token = env
            .lines()
            .find_map(|line| line.strip_prefix("HTTPS_PROXY=http://axo:"))
            .unwrap()
            .trim_end_matches("@127.0.0.1:3128")
            .to_string();
        let hash = axocoatl_exec::egress::protocol::credential_hash(&token);
        let sidecar = RelaySidecar::attach(&egress, 1).await;
        let pipe = sidecar
            .open(1, RequestKind::Connect, "api.anthropic.test", port, &hash)
            .await
            .unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(egress.authority_der().unwrap()).unwrap();
        let mut tls = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
        tls.alpn_protocols = vec![b"http/1.1".to_vec()];
        let stream = tokio_rustls::TlsConnector::from(Arc::new(tls))
            .connect(
                rustls::pki_types::ServerName::try_from("api.anthropic.test").unwrap(),
                pipe,
            )
            .await
            .unwrap();
        let (mut sender, connection) =
            hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
                .await
                .unwrap();
        let connection = tokio::spawn(connection);
        let response = sender
            .send_request(
                hyper::Request::get("/allowed")
                    .header(hyper::header::HOST, format!("api.anthropic.test:{port}"))
                    .header(
                        hyper::header::AUTHORIZATION,
                        "Bearer axocoatl-route:api.anthropic.test",
                    )
                    .body(Empty::<bytes::Bytes>::new())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), hyper::StatusCode::OK);
        let _ = response.into_body().collect().await;
        drop(sender);
        let _ = connection.await;
        let seen = upstream.seen();
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert_eq!(seen[0].authorization, [format!("Bearer {secret}")]);
        let events = record.events();
        assert!(
            events.iter().any(|event| matches!(event,
            NetworkEvent::Request { decision: Decision::Allow, credential: Some(name), path, .. }
                if name == "claude-code-oauth" && path == "/allowed")),
            "{events:#?}"
        );
        for event in &events {
            assert!(!serde_json::to_string(event).unwrap().contains(&secret));
        }
        drop(grant);
    }

    #[test]
    fn values_that_are_not_one_bare_token_are_warned_about_without_the_value() {
        let secret = "sk-ant-oat01-AbC_dEf-123";
        assert!(value_warnings("claude-code-oauth", secret.as_bytes()).is_empty());
        assert!(value_warnings("claude-code-oauth", format!("{secret}\n").as_bytes()).is_empty());
        assert!(value_warnings("example-token", b"ghp_plain").is_empty());
        for (name, value, expected) in [
            (
                "example-token",
                format!("Bearer {secret}"),
                &["\"Bearer \""][..],
            ),
            (
                "example-token",
                format!("bearer {secret}"),
                &["\"Bearer \""],
            ),
            (
                "example-token",
                format!("{{\"token\": \"{secret}\"}}"),
                &["looks like JSON", "whitespace inside", "2 tokens"],
            ),
            (
                "example-token",
                format!("\"{secret}\""),
                &["looks like JSON"],
            ),
            (
                "example-token",
                format!("token: {secret}"),
                &["whitespace inside", "2 tokens"],
            ),
            ("example-token", format!("{secret},{secret}"), &["2 tokens"]),
            ("example-token", format!("{secret}{secret}"), &["2 tokens"]),
            ("example-token", format!(" {secret}"), &["starts or ends"]),
            (
                "claude-code-oauth",
                "sk-ant-api03-key".to_string(),
                &["\"sk-ant-oat01-\""],
            ),
            (
                "claude-code-oauth",
                format!("Bearer {secret}"),
                &["\"Bearer \"", "\"sk-ant-oat01-\""],
            ),
        ] {
            let warnings = value_warnings(name, value.as_bytes());
            assert_eq!(warnings.len(), expected.len(), "{value:?}: {warnings:?}");
            for (warning, wanted) in warnings.iter().zip(expected) {
                assert!(warning.contains(wanted), "{value:?}: {warning}");
                assert!(!warning.contains(secret), "{warning}");
            }
        }
    }

    #[test]
    fn the_store_again_hint_says_to_pipe_only_the_token() {
        let hint = store_again_hint("claude-code-oauth");
        assert!(
            hint.starts_with(
                "store it again with `axocoatl secret set claude-code-oauth`, piping in only the token"
            ),
            "{hint}"
        );
        assert!(hint.contains("sk-ant-oat01-…"), "{hint}");
        assert!(store_again_hint("codex-openai").contains("OPENAI_API_KEY"));
        assert_eq!(
            store_again_hint("example-token"),
            "store it again with `axocoatl secret set example-token`, piping in only the token"
        );
    }
}
