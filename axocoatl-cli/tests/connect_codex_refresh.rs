//! `axocoatl connect codex --refresh` and `--status` end to end: the built
//! binary asks a daemon (bootstrapped in this process, with a fake `podman`
//! on `PATH`, serving the real API router with the local API token on a
//! loopback port) to refresh the stored ChatGPT sign-in through
//! `POST /api/connect/codex/refresh`. The daemon's broker refreshes at a
//! fake OpenAI token endpoint on loopback
//! (`use_chatgpt_token_endpoint_for_tests`), so nothing here reaches OpenAI
//! or a real account. It checks the rotation and its atomic write, the
//! printed times, every exit code, the remembered refusal, `--status`, and
//! that no token reaches the output or the daemon log.
//!
//! One test per process: it sets the process's environment (data root,
//! `HOME`, `PATH`) before the daemon starts, and owns the process's log.
#![cfg(unix)]

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

use axocoatl_daemon::external_agent::codex_chatgpt::{ChatgptSignIn, CLIENT_ID};
use base64::Engine as _;
use wiremock::matchers::{body_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use zeroize::Zeroizing;

const BIN: &str = env!("CARGO_BIN_EXE_axocoatl");

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// An unsigned JWT for `account` expiring `in_seconds` from now.
fn access_token(account: &str, in_seconds: u64) -> String {
    let encode = |value: serde_json::Value| {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value.to_string())
    };
    format!(
        "{}.{}.{}",
        encode(serde_json::json!({"alg": "RS256", "typ": "JWT"})),
        encode(serde_json::json!({
            "exp": now_ms() / 1000 + in_seconds,
            "jti": uuid::Uuid::new_v4().to_string(),
            "https://api.openai.com/auth": {"chatgpt_account_id": account},
        })),
        uuid::Uuid::new_v4().simple()
    )
}

fn refresh_token() -> String {
    format!("rt_{}", uuid::Uuid::new_v4().simple())
}

fn refresh_request(refresh: &str) -> serde_json::Value {
    serde_json::json!({"client_id": CLIENT_ID, "grant_type": "refresh_token",
        "refresh_token": refresh})
}

fn refreshed(access: &str, refresh: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "access_token": access, "refresh_token": refresh, "token_type": "Bearer",
        "expires_in": 7200}))
}

/// `ms` as the binary prints it under `TZ=UTC`.
fn utc(ms: u64) -> String {
    chrono::DateTime::from_timestamp_millis(ms as i64)
        .unwrap()
        .format("%Y-%m-%d %H:%M:%S +00:00")
        .to_string()
}

/// A log writer that keeps what the daemon logs.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Captured {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

struct Paths {
    root: PathBuf,
    data: PathBuf,
    home: PathBuf,
    config: PathBuf,
}

impl Paths {
    fn secret(&self) -> PathBuf {
        self.data.join("secrets/codex-chatgpt")
    }

    fn stored(&self) -> ChatgptSignIn {
        ChatgptSignIn::decode(&std::fs::read(self.secret()).unwrap()).unwrap()
    }

    fn store(&self, access: &str, refresh: &str, account: &str) {
        let sign_in = ChatgptSignIn::new(
            Zeroizing::new(access.to_string()),
            Zeroizing::new(refresh.to_string()),
            Some(account.to_string()),
            now_ms(),
        )
        .unwrap();
        axocoatl_daemon::secret_store::set_secret(
            &self.data,
            "codex-chatgpt",
            &sign_in.encode().unwrap(),
        )
        .unwrap();
    }

    /// `axocoatl connect codex ARGS -c CONFIG`, with the data root, a
    /// private `HOME` and UTC.
    fn connect(&self, args: &[&str], token: Option<&str>) -> Output {
        let mut command = Command::new(BIN);
        command
            .args(["connect", "codex"])
            .args(args)
            .arg("-c")
            .arg(&self.config)
            .current_dir(&self.root)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &self.home)
            .env("AXOCOATL_DATA_DIR", &self.data)
            .env("TZ", "UTC");
        if let Some(token) = token {
            command.env("AXOCOATL_TOKEN", token);
        }
        command.output().unwrap()
    }
}

fn text(output: &Output) -> String {
    format!(
        "exit {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn assert_no_token(output: &Output, tokens: &[&str]) {
    let shown = text(output);
    for token in tokens {
        assert!(!shown.contains(token), "{shown}");
    }
}

#[test]
fn connect_codex_refresh_asks_the_daemon_and_status_shows_the_sign_in() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let (bin, home, data) = (root.join("bin"), root.join("home"), root.join("data"));
    for directory in [&bin, &home, &data] {
        std::fs::create_dir(directory).unwrap();
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    // The fake `podman` the daemon probes at startup: no container is run.
    std::fs::write(
        bin.join("podman"),
        r#"#!/bin/sh
case "$*" in
  --version) printf 'podman version 5.0.0\n' ;;
  'machine list --format json') printf '[{"Running":true}]\n' ;;
  'info --format json') printf '{}\n' ;;
  'ps '*) ;;
  *) printf 'unexpected Podman command: %s\n' "$*" >&2; exit 1 ;;
esac
"#,
    )
    .unwrap();
    std::fs::set_permissions(bin.join("podman"), std::fs::Permissions::from_mode(0o700)).unwrap();
    // Before any thread starts: this process's environment is the daemon's.
    std::env::set_var("AXOCOATL_DATA_DIR", &data);
    std::env::set_var("AXOCOATL_SOCKET_PATH", root.join("ipc/daemon.sock"));
    std::env::set_var("HOME", &home);
    std::env::set_var("PATH", &bin);
    for variable in [
        "CONTAINER_CONNECTION",
        "AXOCOATL_URL",
        "AXOCOATL_TOKEN",
        "RUST_LOG",
    ] {
        std::env::remove_var(variable);
    }
    let log = Captured::default();
    {
        let log = log.clone();
        tracing_subscriber::fmt()
            .with_writer(move || log.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .init();
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(run(&root, data, home, log));
}

async fn run(root: &Path, data: PathBuf, home: PathBuf, log: Captured) {
    let mut config = axocoatl_config::AxocoatlConfig::default();
    config.agents.clear();
    config.consolidation.enabled = false;
    let daemon = axocoatl_daemon::AxocoatlDaemon::bootstrap_headless(config)
        .await
        .unwrap();
    let token_endpoint = MockServer::start().await;
    daemon
        .use_chatgpt_token_endpoint_for_tests(&format!("{}/oauth/token", token_endpoint.uri()))
        .unwrap();
    // A token endpoint off this computer is refused.
    assert!(daemon
        .use_chatgpt_token_endpoint_for_tests("https://auth.openai.com/oauth/token")
        .is_err());
    let data_root = axocoatl_core::SecureDir::open_existing_all(&data).unwrap();
    let local_token = axocoatl_server::auth::load_or_create_local_token(&data_root).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let state: axocoatl_server::AppState = Arc::new(tokio::sync::RwLock::new(daemon));
    let router = axocoatl_server::build_router(
        state.clone(),
        axocoatl_server::auth::AuthConfig::default().with_local_token(local_token, port),
        Vec::new(),
        Arc::new(axocoatl_server::middleware::RateLimiter::new(
            Default::default(),
        )),
    );
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
    });
    let config = root.join("axocoatl.yaml");
    std::fs::write(
        &config,
        format!(
            "agents: []\nserver:\n  host: 127.0.0.1\n  port: {port}\nsandbox:\n  network: none\nconsolidation:\n  enabled: false\n"
        ),
    )
    .unwrap();
    let paths = Arc::new(Paths {
        root: root.to_path_buf(),
        data,
        home,
        config,
    });
    let connect = |args: &'static [&'static str], token: Option<&'static str>| {
        let paths = paths.clone();
        async move {
            tokio::task::spawn_blocking(move || paths.connect(args, token))
                .await
                .unwrap()
        }
    };

    // 1. No sign-in stored: exit 2 for both, nothing sent to the token
    //    endpoint.
    let output = connect(&["--refresh"], None).await;
    assert_eq!(output.status.code(), Some(2), "{}", text(&output));
    assert!(
        text(&output).contains("no ChatGPT sign-in is stored as codex-chatgpt"),
        "{}",
        text(&output)
    );
    let output = connect(&["--status"], None).await;
    assert_eq!(output.status.code(), Some(2), "{}", text(&output));
    assert!(
        text(&output).contains("No ChatGPT sign-in is stored"),
        "{}",
        text(&output)
    );
    assert!(token_endpoint.received_requests().await.unwrap().is_empty());

    // 2. A sign-in an hour from expiry is refreshed on request: the stored
    //    refresh token goes to the endpoint, the rotated sign-in is stored
    //    (0600), and the times are printed; no token is shown.
    let (access, refresh) = (access_token("acct-e2e-4d2f", 3600), refresh_token());
    paths.store(&access, &refresh, "acct-e2e-4d2f");
    let (new_access, new_refresh) = (access_token("acct-e2e-4d2f", 7200), refresh_token());
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_json(refresh_request(&refresh)))
        .respond_with(refreshed(&new_access, &new_refresh))
        .expect(1)
        .mount(&token_endpoint)
        .await;
    let output = connect(&["--refresh"], None).await;
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    let stored = paths.stored();
    assert_eq!(stored.access_token(), new_access);
    assert_eq!(stored.refresh_token(), new_refresh);
    assert_eq!(stored.account_id(), Some("acct-e2e-4d2f"));
    assert_eq!(
        std::fs::metadata(paths.secret())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let shown = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        shown.contains(&format!(
            "Refreshed the ChatGPT sign-in at {} and stored it as secret codex-chatgpt (no \
             token was shown). The new access token expires at {} (in ",
            utc(stored.refreshed_at_ms()),
            utc(stored.expires_at_ms().unwrap())
        )),
        "{}",
        text(&output)
    );
    assert_no_token(&output, &[&access, &refresh, &new_access, &new_refresh]);

    // `--status` shows the stored sign-in, without a token or the whole
    // account id.
    let output = connect(&["--status"], None).await;
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    let shown = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(shown.contains("Account: …4d2f\n"), "{shown}");
    assert!(!shown.contains("acct-e2e"), "{shown}");
    assert!(
        shown.contains(&format!(
            "Access token: expires at {} (in ",
            utc(stored.expires_at_ms().unwrap())
        )),
        "{shown}"
    );
    assert!(
        shown.contains(&format!(
            "Last refreshed or signed in: {} (",
            utc(stored.refreshed_at_ms())
        )),
        "{shown}"
    );
    assert_no_token(&output, &[&new_access, &new_refresh]);

    // Again: the rotated refresh token goes, never the used one.
    let (third_access, third_refresh) = (access_token("acct-e2e-4d2f", 7200), refresh_token());
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_json(refresh_request(&new_refresh)))
        .respond_with(refreshed(&third_access, &third_refresh))
        .expect(1)
        .mount(&token_endpoint)
        .await;
    let output = connect(&["--refresh"], None).await;
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    assert_eq!(paths.stored().refresh_token(), third_refresh);
    token_endpoint.verify().await;
    token_endpoint.reset().await;

    // 3. OpenAI refuses the refresh token: exit 1, nothing stored; asked
    //    again, the daemon does not send it again.
    let before = std::fs::read(paths.secret()).unwrap();
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_json(refresh_request(&third_refresh)))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "error": "invalid_grant", "error_description": "refresh token is invalid"})))
        .expect(1)
        .mount(&token_endpoint)
        .await;
    let output = connect(&["--refresh"], None).await;
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
    assert!(
        text(&output).contains("OpenAI refused to refresh the ChatGPT sign-in (HTTP 400, invalid_grant); connect again with `axocoatl connect codex`"),
        "{}",
        text(&output)
    );
    let output = connect(&["--refresh"], None).await;
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
    assert!(
        text(&output).contains("was not sent again"),
        "{}",
        text(&output)
    );
    assert_no_token(&output, &[&third_access, &third_refresh]);
    token_endpoint.verify().await;
    assert_eq!(std::fs::read(paths.secret()).unwrap(), before);

    // 4. A sign-in that is not one: exit 5, nothing sent.
    axocoatl_daemon::secret_store::set_secret(&paths.data, "codex-chatgpt", b"not-a-sign-in")
        .unwrap();
    token_endpoint.reset().await;
    let output = connect(&["--refresh"], None).await;
    assert_eq!(output.status.code(), Some(5), "{}", text(&output));
    assert!(
        text(&output).contains("cannot be used"),
        "{}",
        text(&output)
    );
    let output = connect(&["--status"], None).await;
    assert_eq!(output.status.code(), Some(5), "{}", text(&output));
    assert!(token_endpoint.received_requests().await.unwrap().is_empty());

    // 5. The token endpoint cannot be reached: exit 4, the sign-in is
    //    unchanged.
    let (access, refresh) = (access_token("acct-e2e-4d2f", 3600), refresh_token());
    paths.store(&access, &refresh, "acct-e2e-4d2f");
    let before = std::fs::read(paths.secret()).unwrap();
    let closed = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    state
        .read()
        .await
        .use_chatgpt_token_endpoint_for_tests(&format!("http://127.0.0.1:{closed}/oauth/token"))
        .unwrap();
    let output = connect(&["--refresh"], None).await;
    assert_eq!(output.status.code(), Some(4), "{}", text(&output));
    assert!(
        text(&output).contains("the token endpoint could not be reached"),
        "{}",
        text(&output)
    );
    assert!(text(&output).contains("unchanged"), "{}", text(&output));
    assert_eq!(std::fs::read(paths.secret()).unwrap(), before);

    // 6. A wrong API token: refused by the daemon, exit 5.
    let output = connect(&["--refresh"], Some("wrong-token")).await;
    assert_eq!(output.status.code(), Some(5), "{}", text(&output));
    assert!(
        text(&output).contains("refused the local API token"),
        "{}",
        text(&output)
    );

    // 7. Usage errors exit 3: a time out of bounds, --url without
    //    --refresh, --refresh with a sign-in option.
    for args in [
        &["--sign-in-timeout", "0"][..],
        &["--sign-in-timeout", "61"],
        &["--url", "http://127.0.0.1:9"],
        &["--refresh", "--device-auth"],
        &["--status", "--refresh"],
    ] {
        let output = paths.connect(args, None);
        assert_eq!(output.status.code(), Some(3), "{args:?}: {}", text(&output));
    }

    // The daemon log recorded each refresh asked for and how it ended,
    // without a token.
    let logged = log.text();
    assert!(
        logged.contains(
            "refreshing the ChatGPT sign-in on request (axocoatl connect codex --refresh)"
        ),
        "{logged}"
    );
    for outcome in ["refreshed", "refused", "no_sign_in", "unusable", "failed"] {
        assert!(
            logged.contains(&format!("outcome=\"{outcome}\""))
                || logged.contains(&format!("outcome={outcome}")),
            "{outcome}: {logged}"
        );
    }
    for token in [
        &access,
        &refresh,
        &new_access,
        &new_refresh,
        &third_access,
        &third_refresh,
    ] {
        assert!(!logged.contains(token.as_str()), "{logged}");
    }

    // 8. No daemon: exit 7.
    server.abort();
    let _ = server.await;
    state.read().await.shutdown().await.unwrap();
    let output = connect(&["--refresh"], None).await;
    assert_eq!(output.status.code(), Some(7), "{}", text(&output));
    assert!(
        text(&output).contains("axocoatl serve"),
        "{}",
        text(&output)
    );
}
