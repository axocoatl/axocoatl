//! The per-daemon local API token, end to end: a real `axocoatl serve` on a
//! loopback port with no configured credentials, and `axocoatl url`.
//!
//! The daemon starts with no agents, so it needs no model download. It still
//! probes the configured Podman connection at startup; on a Mac, set
//! CONTAINER_CONNECTION to a running machine.
#![cfg(unix)]

mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Output;

use support::{axocoatl, client, free_port, Daemon};

fn write_config(root: &Path, port: u16, auth: &str) -> PathBuf {
    let config = root.join("axocoatl.yaml");
    std::fs::write(
        &config,
        format!(
            "agents: []\nserver:\n  host: 127.0.0.1\n  port: {port}\n{auth}sandbox:\n  network: none\nconsolidation:\n  enabled: false\n"
        ),
    )
    .unwrap();
    config
}

fn url_command(root: &Path, config: &Path) -> Output {
    axocoatl(root)
        .args(["url", "--config"])
        .arg(config)
        .output()
        .unwrap()
}

#[tokio::test]
async fn local_api_requires_the_per_daemon_token() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let port = free_port();
    let config = write_config(&root, port, "");
    let mut daemon = Daemon::start(&root, &config, port, 0).await;

    let token_path = root.join("data").join("local-api-token");
    let metadata = std::fs::metadata(&token_path).unwrap();
    assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    let token = std::fs::read_to_string(&token_path).unwrap();
    assert_eq!(token.len(), 43);
    assert!(token
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')));

    let client = client(port);
    let base = format!("http://127.0.0.1:{port}");
    let cookie_name = format!("axocoatl-token-{port}");
    let status = |request: reqwest::RequestBuilder| async move {
        request.send().await.unwrap().status().as_u16()
    };

    // Missing and wrong credentials.
    for path in ["/api/agents", "/api/sessions", "/.well-known/agent.json"] {
        assert_eq!(
            status(client.get(format!("{base}{path}"))).await,
            401,
            "{path}"
        );
    }
    assert_eq!(
        status(client.post(format!("{base}/a2a/tasks")).body("{}")).await,
        401
    );
    for request in [
        client
            .get(format!("{base}/api/agents"))
            .bearer_auth("wrong"),
        client
            .get(format!("{base}/api/agents"))
            .header("x-api-key", "wrong"),
        client
            .get(format!("{base}/api/agents"))
            .header("cookie", format!("{cookie_name}=wrong")),
        client
            .get(format!("{base}/api/agents"))
            .header("cookie", format!("axocoatl-token-1={token}")),
    ] {
        assert_eq!(status(request).await, 401);
    }

    // Valid credentials, as a header or the sign-in cookie.
    for request in [
        client.get(format!("{base}/api/agents")).bearer_auth(&token),
        client
            .get(format!("{base}/api/agents"))
            .header("x-api-key", &token),
        client
            .get(format!("{base}/api/agents"))
            .header("cookie", format!("{cookie_name}={token}")),
    ] {
        assert_eq!(status(request).await, 200);
    }

    // Public paths.
    for path in ["/health", "/health/live", "/ui/tokens.css"] {
        assert_eq!(
            status(client.get(format!("{base}{path}"))).await,
            200,
            "{path}"
        );
    }

    // The dashboard without a cookie explains how to sign in.
    let page = client
        .get(format!("http://localhost:{port}/"))
        .send()
        .await
        .unwrap();
    assert_eq!(page.status().as_u16(), 401);
    assert!(page.text().await.unwrap().contains("axocoatl url"));

    // Browser sign-in: 303 to the same page without the secret.
    let sign_in = client
        .get(format!("http://localhost:{port}/?token={token}&session=s1"))
        .send()
        .await
        .unwrap();
    assert_eq!(sign_in.status().as_u16(), 303);
    let location = sign_in.headers()["location"].to_str().unwrap();
    assert_eq!(location, "/?session=s1");
    assert!(!location.contains(&token));
    assert_eq!(sign_in.headers()["referrer-policy"], "no-referrer");
    let set_cookie = sign_in.headers()["set-cookie"].to_str().unwrap();
    assert!(set_cookie.starts_with(&format!("{cookie_name}={token};")));
    assert!(set_cookie.contains("HttpOnly"));
    assert!(set_cookie.contains("SameSite=Strict"));
    assert!(set_cookie.contains("Path=/"));
    let shell = client
        .get(format!("http://localhost:{port}/?session=s1"))
        .header("cookie", format!("{cookie_name}={token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(shell.status().as_u16(), 200);
    assert!(shell
        .text()
        .await
        .unwrap()
        .contains("<title>Axocoatl</title>"));

    // The link printed by `axocoatl url`.
    let url = url_command(&root, &config);
    assert!(url.status.success(), "{url:?}");
    assert_eq!(
        String::from_utf8(url.stdout).unwrap(),
        format!("http://localhost:{port}/?token={token}\n")
    );

    // The token survives a restart and keeps working.
    daemon.stop();
    let logs = daemon.logs();
    assert!(logs.contains("HTTP request"), "{logs}");
    assert!(logs.contains("token=REDACTED"), "{logs}");
    assert!(logs.contains("axocoatl url --config"), "{logs}");
    assert!(!logs.contains(&token), "the token reached the logs");
    drop(daemon);

    let mut daemon = Daemon::start(&root, &config, port, 1).await;
    assert_eq!(std::fs::read_to_string(&token_path).unwrap(), token);
    assert_eq!(
        status(client.get(format!("{base}/api/agents")).bearer_auth(&token)).await,
        200
    );
    assert_eq!(status(client.get(format!("{base}/api/agents"))).await, 401);
    daemon.stop();
    assert!(!daemon.logs().contains(&token));
}

#[test]
fn url_without_a_token_creates_nothing() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let config = write_config(&root, free_port(), "");

    let url = url_command(&root, &config);
    assert_eq!(url.status.code(), Some(1), "{url:?}");
    assert!(url.stdout.is_empty());
    assert!(String::from_utf8(url.stderr)
        .unwrap()
        .contains("No sign-in token yet"));
    assert!(!root.join("data").exists());

    std::fs::create_dir(root.join("data")).unwrap();
    std::fs::set_permissions(root.join("data"), std::fs::Permissions::from_mode(0o700)).unwrap();
    let url = url_command(&root, &config);
    assert_eq!(url.status.code(), Some(1), "{url:?}");
    assert!(!root.join("data").join("local-api-token").exists());
}

#[test]
fn url_with_a_shared_token_file_fails_closed() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let config = write_config(&root, free_port(), "");
    let data = root.join("data");
    std::fs::create_dir(&data).unwrap();
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).unwrap();
    let token = "A".repeat(43);
    std::fs::write(data.join("local-api-token"), &token).unwrap();
    std::fs::set_permissions(
        data.join("local-api-token"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();

    let url = url_command(&root, &config);
    assert_eq!(url.status.code(), Some(1), "{url:?}");
    assert!(url.stdout.is_empty());
    assert!(String::from_utf8(url.stderr).unwrap().contains("chmod 600"));
}

#[test]
fn url_with_configured_credentials_prints_the_plain_url() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let port = free_port();
    let config = write_config(&root, port, "  auth:\n    api_keys: [\"operator-key\"]\n");

    let url = url_command(&root, &config);
    assert!(url.status.success(), "{url:?}");
    assert_eq!(
        String::from_utf8(url.stdout).unwrap(),
        format!("http://127.0.0.1:{port}/\n")
    );
    assert!(!root.join("data").exists());
}
