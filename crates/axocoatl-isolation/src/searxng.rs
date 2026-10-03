//! A SearXNG metasearch instance that Axocoatl runs for `web_search`.
//!
//! The container starts on the first search, serialized by a mutex, and is
//! removed when the daemon shuts down. It publishes its HTTP port on host
//! loopback only. Its settings file is written owner-only under the data root
//! and copied into the container with `podman cp`; nothing on the host is
//! mounted into it.
//!
//! SearXNG queries public search engines from the Podman default network.
//! That is host-side egress outside any Session's egress proxy, and the
//! queries come from the model.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use axocoatl_core::SecureDir;
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::error::IsolationError;

/// The pinned SearXNG image: the multi-arch index for
/// `2026.10.2-19ffbcd30` (linux/amd64, linux/arm64, linux/arm/v7).
pub const SEARXNG_IMAGE: &str = "docker.io/searxng/searxng:2026.10.2-19ffbcd30@sha256:c642712fcedcdaa78fac44f71eada86aff510745826ba1bd1a368211fea2ce7f";
/// `io.axocoatl.role` value for the SearXNG container.
pub const SEARXNG_ROLE: &str = "searxng";
/// Label naming the daemon data root that owns a container.
pub const RUNTIME_AUTHORITY_LABEL: &str = "io.axocoatl.runtime-authority";
/// Label naming what an Axocoatl container is for.
pub const ROLE_LABEL: &str = "io.axocoatl.role";
/// The port SearXNG's server (granian) listens on in the pinned image.
pub const SEARXNG_CONTAINER_PORT: u16 = 8080;
/// Where the pinned image reads its settings (`__SEARXNG_SETTINGS_PATH`).
pub const SEARXNG_SETTINGS_PATH: &str = "/etc/searxng/settings.yml";

const PODMAN: &str = "podman";
const SETTINGS_DIR: &str = "searxng";
const SETTINGS_FILE: &str = "settings.yml";
const SECRET_FILE: &str = "secret";
/// Ordinary Podman calls.
const PODMAN_TIMEOUT: Duration = Duration::from_secs(60);
/// `podman create` may pull the image on first use.
const CREATE_TIMEOUT: Duration = Duration::from_secs(600);
/// Time for SearXNG to answer `/healthz` after start.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(30);
/// A quick liveness probe before reusing a running instance.
const LIVENESS_TIMEOUT: Duration = Duration::from_secs(3);
const OUTPUT_MAX_BYTES: usize = 64 * 1024;

/// What a managed instance searches with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearxngSettings {
    /// Keep only these engines. Empty keeps SearXNG's defaults.
    pub engines: Vec<String>,
    pub language: String,
    /// 0 (off), 1 (moderate) or 2 (strict).
    pub safesearch: u8,
    /// 1-60 seconds; also the per-engine request timeout, capped at 10.
    pub timeout_secs: u64,
}

impl Default for SearxngSettings {
    fn default() -> Self {
        Self {
            engines: Vec::new(),
            language: "all".into(),
            safesearch: 0,
            timeout_secs: 15,
        }
    }
}

#[derive(Debug, Clone)]
struct Endpoint {
    base_url: String,
}

/// A managed SearXNG container, shared daemon-wide.
pub struct SearxngService {
    runtime_authority: String,
    image: String,
    settings: SearxngSettings,
    secret_key: String,
    directory: SecureDir,
    extra_labels: Vec<(String, String)>,
    client: reqwest::Client,
    state: tokio::sync::Mutex<Option<Endpoint>>,
}

impl std::fmt::Debug for SearxngService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SearxngService")
            .field("image", &self.image)
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

/// Output of one bounded Podman call.
#[derive(Debug)]
struct PodmanOutput {
    success: bool,
    stdout: String,
    stderr: String,
}

impl PodmanOutput {
    fn detail(&self) -> String {
        let text = if self.stderr.trim().is_empty() {
            self.stdout.trim()
        } else {
            self.stderr.trim()
        };
        let mut text = text.to_string();
        if text.len() > 1024 {
            let mut end = 1024;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
        }
        text
    }
}

async fn podman(args: &[String], timeout: Duration) -> Result<PodmanOutput, IsolationError> {
    let mut command = Command::new(PODMAN);
    command
        .args(args)
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|error| {
        IsolationError::OciSetupFailed(format!("running podman for SearXNG: {error}"))
    })?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let read = async {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut stdout_limited = (&mut stdout).take(OUTPUT_MAX_BYTES as u64);
        let mut stderr_limited = (&mut stderr).take(OUTPUT_MAX_BYTES as u64);
        let (a, b) = tokio::join!(
            stdout_limited.read_to_end(&mut out),
            stderr_limited.read_to_end(&mut err),
        );
        a?;
        b?;
        let status = child.wait().await?;
        Ok::<_, std::io::Error>((status, out, err))
    };
    match tokio::time::timeout(timeout, read).await {
        Ok(Ok((status, out, err))) => Ok(PodmanOutput {
            success: status.success(),
            stdout: String::from_utf8_lossy(&out).into_owned(),
            stderr: String::from_utf8_lossy(&err).into_owned(),
        }),
        Ok(Err(error)) => Err(IsolationError::Io(error)),
        Err(_) => Err(IsolationError::Timeout(timeout)),
    }
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| arg.to_string()).collect()
}

/// `axo-searxng-` plus the first 12 hex digits of the SHA-256 of the runtime
/// authority, so each daemon data root owns one name.
pub fn container_name(runtime_authority: &str) -> String {
    let digest = format!("{:x}", Sha256::digest(runtime_authority.as_bytes()));
    format!("axo-searxng-{}", &digest[..12])
}

fn yaml_string(value: &str) -> String {
    // JSON strings are valid YAML double-quoted scalars.
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".into())
}

fn random_secret() -> Result<String, IsolationError> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| {
        IsolationError::OciSetupFailed(format!("generating the SearXNG secret: {error}"))
    })?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

impl SearxngService {
    /// A service for `image`, keeping its settings and secret under
    /// `<data_root>/searxng/`. Nothing runs until [`Self::base_url`].
    pub fn new(
        runtime_authority: String,
        image: String,
        settings: SearxngSettings,
        data_root: &SecureDir,
    ) -> Result<Self, IsolationError> {
        if runtime_authority.is_empty() {
            return Err(IsolationError::OciSetupFailed(
                "a managed SearXNG needs the daemon's runtime authority".into(),
            ));
        }
        let directory = data_root.child(SETTINGS_DIR)?;
        directory.restrict_owner_only()?;
        let secret_key = match directory.read_limited(SECRET_FILE, 256) {
            Ok(bytes) => {
                let secret = String::from_utf8_lossy(&bytes).trim().to_string();
                if secret.len() == 64 && secret.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    secret
                } else {
                    let secret = random_secret()?;
                    directory.atomic_write(SECRET_FILE, secret.as_bytes())?;
                    secret
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let secret = random_secret()?;
                directory.atomic_write(SECRET_FILE, secret.as_bytes())?;
                secret
            }
            Err(error) => return Err(error.into()),
        };
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| {
                IsolationError::OciSetupFailed(format!("building the SearXNG client: {error}"))
            })?;
        Ok(Self {
            runtime_authority,
            image,
            settings,
            secret_key,
            directory,
            extra_labels: Vec::new(),
            client,
            state: tokio::sync::Mutex::new(None),
        })
    }

    /// Add labels to the container, such as a test-ownership label.
    pub fn with_labels(mut self, labels: Vec<(String, String)>) -> Self {
        self.extra_labels = labels;
        self
    }

    /// This daemon's container name.
    pub fn name(&self) -> String {
        container_name(&self.runtime_authority)
    }

    /// `podman create` arguments for the container. Pure.
    pub fn build_run_args(name: &str, authority: &str, image: &str) -> Vec<String> {
        Self::create_args(name, authority, image, &[], true)
    }

    fn create_args(
        name: &str,
        authority: &str,
        image: &str,
        extra_labels: &[(String, String)],
        with_limits: bool,
    ) -> Vec<String> {
        let mut args = strings(&["create", "--name", name]);
        args.push("--label".into());
        args.push(format!("{RUNTIME_AUTHORITY_LABEL}={authority}"));
        args.push("--label".into());
        args.push(format!("{ROLE_LABEL}={SEARXNG_ROLE}"));
        for (key, value) in extra_labels {
            args.push("--label".into());
            args.push(format!("{key}={value}"));
        }
        // The pinned image starts as root, finds its config and cache
        // directories already owned by its searxng user, refreshes the CA
        // bundle it owns, and serves on 8080. None of that needs a
        // capability, so every capability is dropped.
        // The image declares volumes for its config and cache directories.
        // Ignoring them keeps both in the container's own layer, so removing
        // the container leaves no anonymous volume behind.
        args.extend(strings(&[
            "--cap-drop=ALL",
            "--security-opt=no-new-privileges",
            "--http-proxy=false",
            "--no-healthcheck",
            "--image-volume=ignore",
        ]));
        if with_limits {
            args.extend(strings(&["--memory", "512m", "--pids-limit", "256"]));
        }
        args.push("-p".into());
        args.push(format!("127.0.0.1::{SEARXNG_CONTAINER_PORT}"));
        args.push(image.into());
        args
    }

    /// The settings file. Pure.
    pub fn render_settings(settings: &SearxngSettings, secret_key: &str) -> String {
        let mut out = String::new();
        if settings.engines.is_empty() {
            out.push_str("use_default_settings: true\n");
        } else {
            out.push_str("use_default_settings:\n  engines:\n    keep_only:\n");
            for engine in &settings.engines {
                out.push_str(&format!("      - {}\n", yaml_string(engine)));
            }
        }
        out.push_str("general:\n  instance_name: \"axocoatl-local\"\n  enable_metrics: false\n");
        out.push_str(&format!(
            "server:\n  secret_key: {}\n  limiter: false\n  public_instance: false\n  image_proxy: false\n  method: \"GET\"\n",
            yaml_string(secret_key)
        ));
        // The language travels with each request (`language=`), so the
        // settings keep SearXNG's default.
        out.push_str(&format!(
            "search:\n  safe_search: {}\n  formats:\n    - html\n    - json\n",
            settings.safesearch.min(2)
        ));
        let timeout = settings.timeout_secs.clamp(1, 60);
        out.push_str(&format!(
            "outgoing:\n  request_timeout: {}.0\n  max_request_timeout: {}.0\n",
            timeout.min(10),
            timeout
        ));
        out
    }

    /// The instance's base URL, starting it on first use and again if it
    /// stopped answering.
    pub async fn base_url(&self) -> Result<String, IsolationError> {
        let mut state = self.state.lock().await;
        if let Some(endpoint) = state.as_ref() {
            if self.healthy(&endpoint.base_url, LIVENESS_TIMEOUT).await {
                return Ok(endpoint.base_url.clone());
            }
            tracing::warn!(
                container = %self.name(),
                "managed SearXNG stopped answering; starting it again"
            );
            *state = None;
        }
        let endpoint = self.start().await?;
        let base = endpoint.base_url.clone();
        *state = Some(endpoint);
        Ok(base)
    }

    /// Whether this service has started a container that has not been
    /// stopped.
    pub async fn is_running(&self) -> bool {
        self.state.lock().await.is_some()
    }

    /// Remove the container this service started. Does nothing, and runs no
    /// Podman command, when it never started one; a container left by an
    /// earlier run is [`Self::reap_orphans`]'s to remove.
    pub async fn stop(&self) {
        let mut state = self.state.lock().await;
        if state.take().is_none() {
            return;
        }
        if let Err(error) = self.remove_own_container().await {
            tracing::warn!(container = %self.name(), %error, "removing the managed SearXNG failed");
        }
    }

    async fn healthy(&self, base: &str, timeout: Duration) -> bool {
        matches!(
            self.client
                .get(format!("{base}/healthz"))
                .timeout(timeout)
                .send()
                .await,
            Ok(response) if response.status().as_u16() == 200
        )
    }

    /// Remove this daemon's container if one exists. A container with this
    /// name that does not carry this daemon's authority label is never
    /// touched.
    async fn remove_own_container(&self) -> Result<(), IsolationError> {
        let name = self.name();
        let inspect = podman(
            &[
                "container".to_string(),
                "inspect".to_string(),
                "--format".to_string(),
                format!("{{{{index .Config.Labels \"{RUNTIME_AUTHORITY_LABEL}\"}}}}"),
                name.clone(),
            ],
            PODMAN_TIMEOUT,
        )
        .await?;
        if !inspect.success {
            // No such container.
            return Ok(());
        }
        if inspect.stdout.trim() != self.runtime_authority {
            return Err(IsolationError::OciSetupFailed(format!(
                "a container named {name} exists but was not created by this Axocoatl data root; \
                 remove or rename it"
            )));
        }
        let removed = podman(
            &strings(&[
                "rm",
                "--force",
                "--volumes",
                "--time",
                "0",
                "--ignore",
                &name,
            ]),
            PODMAN_TIMEOUT,
        )
        .await?;
        if !removed.success {
            return Err(IsolationError::OciContainerFailed(format!(
                "removing {name}: {}",
                removed.detail()
            )));
        }
        Ok(())
    }

    fn settings_path(&self) -> PathBuf {
        self.directory.path().join(SETTINGS_FILE)
    }

    async fn start(&self) -> Result<Endpoint, IsolationError> {
        let name = self.name();
        crate::podman::ensure_ready().await?;
        self.directory.atomic_write(
            SETTINGS_FILE,
            Self::render_settings(&self.settings, &self.secret_key).as_bytes(),
        )?;
        self.remove_own_container().await?;

        let mut created = podman(
            &Self::create_args(
                &name,
                &self.runtime_authority,
                &self.image,
                &self.extra_labels,
                true,
            ),
            CREATE_TIMEOUT,
        )
        .await?;
        if !created.success && created.detail().contains("cgroup") {
            tracing::warn!(
                container = %name,
                detail = %created.detail(),
                "SearXNG memory and process limits are unavailable (no cgroup delegation); starting without them"
            );
            created = podman(
                &Self::create_args(
                    &name,
                    &self.runtime_authority,
                    &self.image,
                    &self.extra_labels,
                    false,
                ),
                CREATE_TIMEOUT,
            )
            .await?;
        }
        if !created.success {
            return Err(IsolationError::OciSetupFailed(format!(
                "creating the SearXNG container from {}: {}",
                self.image,
                created.detail()
            )));
        }

        let result = self.configure_and_wait(&name).await;
        if result.is_err() {
            let _ = self.remove_own_container().await;
        }
        result
    }

    async fn configure_and_wait(&self, name: &str) -> Result<Endpoint, IsolationError> {
        let settings = self.settings_path();
        let copied = podman(
            &[
                "cp".to_string(),
                settings.display().to_string(),
                format!("{name}:{SEARXNG_SETTINGS_PATH}"),
            ],
            PODMAN_TIMEOUT,
        )
        .await?;
        if !copied.success {
            return Err(IsolationError::OciSetupFailed(format!(
                "copying SearXNG settings into {name}: {}",
                copied.detail()
            )));
        }
        let started = podman(&strings(&["start", name]), PODMAN_TIMEOUT).await?;
        if !started.success {
            return Err(IsolationError::OciSetupFailed(format!(
                "starting {name}: {}",
                started.detail()
            )));
        }
        let port = podman(
            &[
                "port".to_string(),
                name.to_string(),
                format!("{SEARXNG_CONTAINER_PORT}/tcp"),
            ],
            PODMAN_TIMEOUT,
        )
        .await?;
        let host_port = parse_host_port(&port.stdout).ok_or_else(|| {
            IsolationError::OciSetupFailed(format!(
                "SearXNG port {SEARXNG_CONTAINER_PORT} is not published on loopback: {}",
                port.detail()
            ))
        })?;
        let base_url = format!("http://127.0.0.1:{host_port}");
        let deadline = tokio::time::Instant::now() + HEALTH_TIMEOUT;
        loop {
            if self.healthy(&base_url, Duration::from_secs(2)).await {
                tracing::info!(container = %name, %base_url, "managed SearXNG is ready");
                return Ok(Endpoint { base_url });
            }
            if tokio::time::Instant::now() >= deadline {
                let logs = podman(&strings(&["logs", "--tail", "20", name]), PODMAN_TIMEOUT)
                    .await
                    .map(|output| output.detail())
                    .unwrap_or_default();
                return Err(IsolationError::OciSetupFailed(format!(
                    "SearXNG did not answer /healthz within {} s; last log lines: {logs}",
                    HEALTH_TIMEOUT.as_secs()
                )));
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    /// Remove every SearXNG container carrying this runtime authority.
    /// Best-effort: without Podman, or with its machine stopped, this does
    /// nothing.
    pub async fn reap_orphans(runtime_authority: &str) -> usize {
        if runtime_authority.is_empty() {
            return 0;
        }
        let listed = podman(
            &[
                "ps".to_string(),
                "-a".to_string(),
                "--filter".to_string(),
                format!("label={RUNTIME_AUTHORITY_LABEL}={runtime_authority}"),
                "--filter".to_string(),
                format!("label={ROLE_LABEL}={SEARXNG_ROLE}"),
                "--format".to_string(),
                "{{.Names}}".to_string(),
            ],
            PODMAN_TIMEOUT,
        )
        .await;
        let Ok(listed) = listed else {
            return 0;
        };
        if !listed.success {
            return 0;
        }
        let mut removed = 0;
        for name in listed
            .stdout
            .lines()
            .map(str::trim)
            .filter(|name| name.starts_with("axo-searxng-"))
        {
            match podman(
                &strings(&[
                    "rm",
                    "--force",
                    "--volumes",
                    "--time",
                    "0",
                    "--ignore",
                    name,
                ]),
                PODMAN_TIMEOUT,
            )
            .await
            {
                Ok(output) if output.success => removed += 1,
                Ok(output) => {
                    tracing::warn!(container = name, detail = %output.detail(), "removing an orphaned SearXNG failed")
                }
                Err(error) => {
                    tracing::warn!(container = name, %error, "removing an orphaned SearXNG failed")
                }
            }
        }
        removed
    }
}

/// The host port in `podman port <name> 8080/tcp` output, such as
/// `127.0.0.1:43117`. Only a loopback binding is accepted.
fn parse_host_port(stdout: &str) -> Option<u16> {
    stdout.lines().find_map(|line| {
        let line = line.trim();
        let line = line.rsplit(" -> ").next().unwrap_or(line);
        let (host, port) = line.rsplit_once(':')?;
        let host = host.trim_start_matches('[').trim_end_matches(']');
        matches!(host, "127.0.0.1" | "::1" | "localhost")
            .then(|| port.parse::<u16>().ok())
            .flatten()
            .filter(|port| *port > 0)
    })
}

/// Shared handle used by the daemon.
pub type SharedSearxng = Arc<SearxngService>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_name_is_scoped_to_the_runtime_authority() {
        let a = container_name("authority-a");
        let b = container_name("authority-b");
        assert!(a.starts_with("axo-searxng-"));
        assert_eq!(a.len(), "axo-searxng-".len() + 12);
        assert_ne!(a, b);
        assert_eq!(a, container_name("authority-a"));
    }

    #[test]
    fn create_args_are_least_privilege_and_loopback_only() {
        let args = SearxngService::build_run_args("axo-searxng-x", "auth", SEARXNG_IMAGE);
        let joined = args.join(" ");
        assert_eq!(args[0], "create");
        for required in [
            "--name axo-searxng-x",
            "--label io.axocoatl.runtime-authority=auth",
            "--label io.axocoatl.role=searxng",
            "--cap-drop=ALL",
            "--security-opt=no-new-privileges",
            "--http-proxy=false",
            "--no-healthcheck",
            "--image-volume=ignore",
            "--memory 512m",
            "--pids-limit 256",
            "-p 127.0.0.1::8080",
        ] {
            assert!(
                joined.contains(required),
                "{required} missing from {joined}"
            );
        }
        assert_eq!(args.last().map(String::as_str), Some(SEARXNG_IMAGE));
        // No host mounts, environment or extra capabilities.
        for refused in [
            "-v",
            "--volume",
            "--mount",
            "-e",
            "--env",
            "--cap-add",
            "--privileged",
            "--network",
        ] {
            assert!(
                !args.iter().any(|arg| arg == refused),
                "{refused} in {joined}"
            );
        }
        let unlimited = SearxngService::create_args(
            "n",
            "a",
            "i",
            &[("io.axocoatl.test".into(), "web-1".into())],
            false,
        );
        assert!(!unlimited.iter().any(|arg| arg == "--memory"));
        assert!(unlimited
            .join(" ")
            .contains("--label io.axocoatl.test=web-1"));
    }

    #[test]
    fn settings_enable_json_and_disable_public_features() {
        let rendered = SearxngService::render_settings(
            &SearxngSettings {
                engines: vec!["duckduckgo".into(), "wikipedia".into()],
                language: "en".into(),
                safesearch: 1,
                timeout_secs: 30,
            },
            &"ab".repeat(32),
        );
        assert_eq!(
            rendered,
            format!(
                "use_default_settings:\n  engines:\n    keep_only:\n      - \"duckduckgo\"\n      - \"wikipedia\"\n\
                 general:\n  instance_name: \"axocoatl-local\"\n  enable_metrics: false\n\
                 server:\n  secret_key: \"{}\"\n  limiter: false\n  public_instance: false\n  image_proxy: false\n  method: \"GET\"\n\
                 search:\n  safe_search: 1\n  formats:\n    - html\n    - json\n\
                 outgoing:\n  request_timeout: 10.0\n  max_request_timeout: 30.0\n",
                "ab".repeat(32)
            )
        );
        let defaults = SearxngService::render_settings(&SearxngSettings::default(), "k");
        assert!(defaults.starts_with("use_default_settings: true\n"));
        assert!(defaults.contains("request_timeout: 10.0\n  max_request_timeout: 15.0\n"));
        // Engine names are quoted, so YAML syntax in one cannot add settings.
        let hostile = SearxngService::render_settings(
            &SearxngSettings {
                engines: vec!["x\"\nserver:\n  limiter: true".into()],
                ..SearxngSettings::default()
            },
            "k",
        );
        assert!(hostile.contains("- \"x\\\"\\nserver:\\n  limiter: true\""));
    }

    #[test]
    fn port_output_must_be_loopback() {
        assert_eq!(parse_host_port("127.0.0.1:43117\n"), Some(43117));
        assert_eq!(parse_host_port("8080/tcp -> 127.0.0.1:43118"), Some(43118));
        assert_eq!(parse_host_port("[::1]:43119"), Some(43119));
        assert_eq!(parse_host_port("0.0.0.0:43120"), None);
        assert_eq!(parse_host_port(""), None);
        assert_eq!(parse_host_port("127.0.0.1:0"), None);
    }

    #[test]
    fn secret_is_generated_once_and_kept_owner_only() {
        let temp = tempfile::tempdir().unwrap();
        let root = SecureDir::open(temp.path()).unwrap();
        let first = SearxngService::new(
            "authority".into(),
            SEARXNG_IMAGE.into(),
            SearxngSettings::default(),
            &root,
        )
        .unwrap();
        let second = SearxngService::new(
            "authority".into(),
            SEARXNG_IMAGE.into(),
            SearxngSettings::default(),
            &root,
        )
        .unwrap();
        assert_eq!(first.secret_key, second.secret_key);
        assert_eq!(first.secret_key.len(), 64);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(temp.path().join("searxng/secret"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
            let directory = std::fs::metadata(temp.path().join("searxng"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(directory & 0o077, 0);
        }
        assert!(SearxngService::new(
            String::new(),
            SEARXNG_IMAGE.into(),
            SearxngSettings::default(),
            &root
        )
        .is_err());
    }
}
