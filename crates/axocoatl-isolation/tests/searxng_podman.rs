//! Opt-in checks of the managed SearXNG container against real Podman.
//!
//! ```text
//! CONTAINER_CONNECTION=<connection> cargo test -p axocoatl-isolation \
//!     --test searxng_podman -- --ignored --test-threads=1
//! ```
//!
//! Every container the tests create carries `io.axocoatl.test=web-<pid>` and
//! is removed by that label at the end. `AXOCOATL_LIVE_WEB=1` also asserts
//! that a real query returns at least one result, which needs the internet.
#![cfg(unix)]

use std::time::Duration;

use axocoatl_core::SecureDir;
use axocoatl_isolation::searxng::{container_name, SearxngService, SearxngSettings, SEARXNG_IMAGE};

const TEST_LABEL: &str = "io.axocoatl.test";

fn test_label() -> String {
    format!("web-{}", std::process::id())
}

fn podman(args: &[&str]) -> std::process::Output {
    std::process::Command::new("podman")
        .args(args)
        .output()
        .expect("podman runs")
}

fn podman_text(args: &[&str]) -> String {
    let output = podman(args);
    assert!(
        output.status.success(),
        "podman {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn exists(name: &str) -> bool {
    podman(&["container", "exists", name]).status.success()
}

/// Remove only what this test process created.
fn cleanup() {
    let label = format!("label={TEST_LABEL}={}", test_label());
    let names = podman_text(&["ps", "-a", "--filter", &label, "--format", "{{.Names}}"]);
    for name in names.lines().filter(|name| !name.trim().is_empty()) {
        let _ = podman(&["rm", "--force", "--volumes", "--time", "0", name.trim()]);
    }
}

struct Fixture {
    _directory: tempfile::TempDir,
    root: SecureDir,
    authority: String,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Runs on success and on a failed assertion alike.
        cleanup();
    }
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::Builder::new()
            .prefix("axo-searxng-test-")
            .tempdir()
            .unwrap();
        let root = SecureDir::open(directory.path().canonicalize().unwrap()).unwrap();
        let authority = format!(
            "searxng-test-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        );
        Self {
            _directory: directory,
            root,
            authority,
        }
    }

    fn service(&self) -> SearxngService {
        SearxngService::new(
            self.authority.clone(),
            SEARXNG_IMAGE.into(),
            SearxngSettings::default(),
            &self.root,
        )
        .unwrap()
        .with_labels(vec![(TEST_LABEL.into(), test_label())])
    }
}

async fn search(base: &str, query: &str) -> serde_json::Value {
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let response = client
        .get(format!("{base}/search"))
        .query(&[
            ("q", query),
            ("format", "json"),
            ("pageno", "1"),
            ("language", "all"),
            ("safesearch", "0"),
        ])
        .header("Accept", "application/json")
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    response.json().await.unwrap()
}

#[tokio::test]
#[ignore = "requires Podman: CONTAINER_CONNECTION=axocoatl-ci-pr74 cargo test -p axocoatl-isolation --test searxng_podman -- --ignored --test-threads=1"]
async fn managed_searxng_starts_answers_json_and_stops() {
    let fixture = Fixture::new();
    let service = fixture.service();
    let name = container_name(&fixture.authority);
    let started = std::time::Instant::now();
    let base = service.base_url().await.expect("managed SearXNG starts");
    eprintln!(
        "searxng: started {name} at {base} in {} ms",
        started.elapsed().as_millis()
    );
    assert!(base.starts_with("http://127.0.0.1:"), "{base}");
    assert!(service.is_running().await);
    // A second call reuses the running instance.
    assert_eq!(service.base_url().await.unwrap(), base);

    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let health = client.get(format!("{base}/healthz")).send().await.unwrap();
    assert_eq!(health.status().as_u16(), 200);

    let body = search(&base, "rust programming language").await;
    assert_eq!(body["query"], "rust programming language");
    let results = body["results"].as_array().expect("results is an array");
    assert!(body["unresponsive_engines"].is_array(), "{body}");
    for result in results {
        assert!(result["url"].is_string(), "{result}");
        assert!(result["title"].is_string(), "{result}");
    }
    eprintln!(
        "searxng: {} results, unresponsive: {}",
        results.len(),
        body["unresponsive_engines"]
    );
    if std::env::var("AXOCOATL_LIVE_WEB").as_deref() == Ok("1") {
        assert!(!results.is_empty(), "live search returned nothing: {body}");
    }

    // Least privilege as created.
    let inspect = podman_text(&[
        "container",
        "inspect",
        "--format",
        "{{index .Config.Labels \"io.axocoatl.role\"}} {{index .Config.Labels \"io.axocoatl.runtime-authority\"}} {{json .EffectiveCaps}} {{json .HostConfig.PortBindings}} {{json .HostConfig.Binds}} {{json .HostConfig.SecurityOpt}}",
        &name,
    ]);
    assert!(
        inspect.starts_with(&format!("searxng {} ", fixture.authority)),
        "{inspect}"
    );
    assert!(
        inspect.contains("[]") || inspect.contains("null"),
        "capabilities: {inspect}"
    );
    assert!(inspect.contains("\"HostIp\":\"127.0.0.1\""), "{inspect}");
    assert!(inspect.contains("no-new-privileges"), "{inspect}");
    // The image's declared volumes stay in the container layer: no host
    // directory and no anonymous volume.
    let mounts = podman_text(&[
        "container",
        "inspect",
        "--format",
        "{{json .Mounts}}",
        &name,
    ]);
    assert_eq!(mounts, "[]", "{mounts}");

    service.stop().await;
    assert!(!service.is_running().await);
    assert!(!exists(&name), "{name} still exists after stop");
    cleanup();
}

#[tokio::test]
#[ignore = "requires Podman: CONTAINER_CONNECTION=axocoatl-ci-pr74 cargo test -p axocoatl-isolation --test searxng_podman -- --ignored --test-threads=1"]
async fn orphaned_searxng_is_reaped_by_authority_and_restarts_after_removal() {
    let fixture = Fixture::new();
    let name = container_name(&fixture.authority);
    let service = fixture.service();
    let first = service.base_url().await.unwrap();
    // The container disappears behind the service's back (a crash, a manual
    // removal): the next call starts it again.
    podman_text(&["rm", "--force", "--volumes", "--time", "0", &name]);
    let second = service.base_url().await.unwrap();
    assert!(exists(&name));
    eprintln!("searxng: restarted after removal, {first} -> {second}");
    // A daemon that exits without stopping leaves an orphan; startup cleanup
    // by authority removes it, and only it.
    drop(service);
    assert!(exists(&name));
    assert_eq!(
        SearxngService::reap_orphans(&format!("{}-other", fixture.authority)).await,
        0
    );
    assert!(exists(&name));
    assert_eq!(SearxngService::reap_orphans(&fixture.authority).await, 1);
    assert!(!exists(&name));
    cleanup();
}

#[tokio::test]
#[ignore = "requires Podman: CONTAINER_CONNECTION=axocoatl-ci-pr74 cargo test -p axocoatl-isolation --test searxng_podman -- --ignored --test-threads=1"]
async fn a_same_named_container_from_elsewhere_is_never_removed() {
    let fixture = Fixture::new();
    let name = container_name(&fixture.authority);
    let label = format!("{TEST_LABEL}={}", test_label());
    podman_text(&[
        "create",
        "--name",
        &name,
        "--label",
        &label,
        "docker.io/library/alpine:3.20",
        "true",
    ]);
    let service = fixture.service();
    let error = service.base_url().await.unwrap_err().to_string();
    assert!(
        error.contains("was not created by this Axocoatl data root"),
        "{error}"
    );
    assert!(exists(&name));
    service.stop().await;
    assert!(exists(&name), "stop removed a container it does not own");
    cleanup();
    assert!(!exists(&name));
}
