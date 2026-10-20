//! `axocoatl doctor` against a stand-in Ollama server: a server native
//! Sessions refuse must fail doctor, with the setting that fixes it.

use std::path::Path;
use std::process::Command;

use axocoatl_llm_ollama::AUDITED_OLLAMA_VERSION;
use serde_json::{json, Value};
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

/// What Ollama 0.40.6 shows for qwen3-coder:30b, a legacy GGUF manifest.
fn gguf_with_tools() -> Value {
    json!({"details": {"format": "gguf", "family": "qwen3moe"},
        "capabilities": ["completion", "tools"]})
}

async fn ollama(version: Value, status: Option<Value>) -> MockServer {
    ollama_showing(version, status, gguf_with_tools()).await
}

async fn ollama_showing(version: Value, status: Option<Value>, show: Value) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/show"))
        .respond_with(ResponseTemplate::new(200).set_body_json(show))
        .mount(&server)
        .await;
    let tags = json!({"models": [{
        "name": "qwen3-coder:30b",
        "model": "qwen3-coder:30b",
        "size": 18556700761u64,
        "digest": "0".repeat(64),
        "details": {"format": "gguf", "family": "qwen3moe", "parameter_size": "30.5B"},
    }]});
    let status = match status {
        Some(status) => ResponseTemplate::new(200).set_body_json(status),
        None => ResponseTemplate::new(404).set_body_string("404 page not found"),
    };
    for (route, response) in [
        ("/api/tags", ResponseTemplate::new(200).set_body_json(tags)),
        (
            "/api/version",
            ResponseTemplate::new(200).set_body_json(version),
        ),
        ("/api/status", status),
    ] {
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(response)
            .mount(&server)
            .await;
    }
    server
}

/// Run doctor on an explicit configuration naming `base_url`, isolated from
/// the user's configuration, data, daemon socket and Podman.
fn doctor(root: &Path, base_url: &str) -> (bool, String) {
    doctor_with_model(root, base_url, "qwen3-coder:30b")
}

/// [`doctor`] with the Lead Agent on `model`.
fn doctor_with_model(root: &Path, base_url: &str, model: &str) -> (bool, String) {
    let config = root.join("axocoatl.yaml");
    std::fs::write(
        &config,
        format!(
            r#"agents:
  - id: lead
    name: Lead
    provider: ollama
    model: "{model}"
    tools: [read_file]
providers:
  ollama:
    base_url: "{base_url}"
"#
        ),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_axocoatl"))
        .arg("doctor")
        .arg("--config")
        .arg(&config)
        .current_dir(root)
        .env("HOME", root)
        .env("PATH", "/usr/bin:/bin")
        .env("AXOCOATL_DATA_DIR", root.join("data"))
        .env("AXOCOATL_SOCKET_PATH", root.join("no-daemon.sock"))
        .env_remove("CONTAINER_CONNECTION")
        .env_remove("CONTAINER_HOST")
        .output()
        .unwrap();
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
    )
}

#[tokio::test]
async fn doctor_fails_an_ollama_server_with_cloud_models_enabled() {
    // What an Ollama server started without OLLAMA_NO_CLOUD reports.
    let server = ollama(
        json!({"version": AUDITED_OLLAMA_VERSION}),
        Some(json!({"cloud": {"disabled": false, "source": "none"}})),
    )
    .await;
    let root = tempfile::tempdir().unwrap();
    let (ok, stdout) = doctor(root.path(), &server.uri());

    assert!(!ok, "doctor must fail:\n{stdout}");
    let url = server.uri();
    assert!(
        stdout.contains(&format!("[ OK ] Ollama reachable at {url}")),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!(
            "[FAIL] Ollama at {url} has cloud models enabled; native Sessions refuse it"
        )),
        "{stdout}"
    );
    assert!(stdout.contains(r#"{"disable_ollama_cloud": true} to ~/.ollama/server.json"#));
    assert!(stdout.contains("OLLAMA_NO_CLOUD=1 ollama serve"));
    assert!(stdout.contains("set providers.ollama.base_url to another Ollama server"));
    assert!(!stdout.contains("cloud models disabled"), "{stdout}");
    assert!(stdout.contains("Some required checks FAILED"));
}

#[tokio::test]
async fn doctor_passes_a_cloud_disabled_audited_server() {
    let server = ollama(
        json!({"version": AUDITED_OLLAMA_VERSION}),
        Some(json!({"cloud": {"disabled": true, "source": "env"}})),
    )
    .await;
    let root = tempfile::tempdir().unwrap();
    let (ok, stdout) = doctor(root.path(), &server.uri());

    assert!(ok, "doctor must pass:\n{stdout}");
    let url = server.uri();
    assert!(
        stdout.contains(&format!(
            "[ OK ] Ollama {AUDITED_OLLAMA_VERSION} at {url}, a version native Sessions accept"
        )),
        "{stdout}"
    );
    assert!(
        stdout.contains(
            "[ OK ] Model 'qwen3-coder:30b' is a local GGUF model native Sessions can run (completion, tools)"
        ),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!(
            "[ OK ] Ollama cloud models disabled at {url} (OLLAMA_NO_CLOUD)"
        )),
        "{stdout}"
    );
    assert!(stdout.contains("[ OK ] Model 'qwen3-coder:30b' is pulled"));
    assert!(!stdout.contains("[FAIL]"), "{stdout}");
}

#[tokio::test]
async fn doctor_fails_a_server_without_cloud_status_or_the_audited_version() {
    let server = ollama(json!({"version": "0.12.3"}), None).await;
    let root = tempfile::tempdir().unwrap();
    let (ok, stdout) = doctor(root.path(), &server.uri());

    assert!(!ok, "doctor must fail:\n{stdout}");
    assert!(
        stdout.contains("reports version 0.12.3; native Sessions refuse it"),
        "{stdout}"
    );
    assert!(
        stdout.contains("upgrade Ollama to 0.40.6 or later"),
        "{stdout}"
    );
    assert!(
        stdout.contains("does not report its cloud mode (GET /api/status)"),
        "{stdout}"
    );
}

#[tokio::test]
async fn doctor_refuses_ollama_0_20_6_and_says_why() {
    let server = ollama(
        json!({"version": "0.20.6"}),
        Some(json!({"cloud": {"disabled": true, "source": "config"}})),
    )
    .await;
    let root = tempfile::tempdir().unwrap();
    let (ok, stdout) = doctor(root.path(), &server.uri());
    assert!(!ok, "doctor must fail:\n{stdout}");
    assert!(
        stdout.contains("reports version 0.20.6; native Sessions refuse it"),
        "{stdout}"
    );
    assert!(stdout.contains("0.20.6 is no longer supported"), "{stdout}");
    assert!(stdout.contains("brew upgrade ollama"), "{stdout}");
}

#[tokio::test]
async fn doctor_fails_an_mlx_model_and_names_the_gguf_pull() {
    let server = ollama_showing(
        json!({"version": AUDITED_OLLAMA_VERSION}),
        Some(json!({"cloud": {"disabled": true, "source": "env"}})),
        json!({"details": {"format": "safetensors", "runner": "mlx"},
            "capabilities": ["completion", "tools"]}),
    )
    .await;
    let root = tempfile::tempdir().unwrap();
    let (ok, stdout) = doctor(root.path(), &server.uri());
    assert!(!ok, "doctor must fail:\n{stdout}");
    assert!(
        stdout.contains("[FAIL] Model 'qwen3-coder:30b' cannot run in native Sessions"),
        "{stdout}"
    );
    assert!(stdout.contains("MLX runner"), "{stdout}");
    assert!(
        stdout.contains("ollama pull --runner llamacpp qwen3-coder:30b"),
        "{stdout}"
    );
}

#[tokio::test]
async fn doctor_fails_a_model_without_tools_for_an_agent_with_tools() {
    let server = ollama_showing(
        json!({"version": AUDITED_OLLAMA_VERSION}),
        Some(json!({"cloud": {"disabled": true, "source": "env"}})),
        json!({"details": {"format": "gguf"}, "capabilities": ["completion"]}),
    )
    .await;
    let root = tempfile::tempdir().unwrap();
    let (ok, stdout) = doctor(root.path(), &server.uri());
    assert!(!ok, "doctor must fail:\n{stdout}");
    assert!(
        stdout.contains("[FAIL] Model 'qwen3-coder:30b' does not report tool calling"),
        "{stdout}"
    );
}

/// An untagged name is the `:latest` tag to Ollama, so another tag of the
/// same model does not count as pulled: doctor fails it and names the tag
/// that is installed, instead of passing a model Session start then refuses.
#[tokio::test]
async fn doctor_fails_an_untagged_model_that_only_another_tag_satisfies() {
    let server = ollama(
        json!({"version": AUDITED_OLLAMA_VERSION}),
        Some(json!({"cloud": {"disabled": true, "source": "env"}})),
    )
    .await;
    let root = tempfile::tempdir().unwrap();
    let (ok, stdout) = doctor_with_model(root.path(), &server.uri(), "qwen3-coder");
    assert!(!ok, "doctor must fail:\n{stdout}");
    assert!(
        stdout.contains("[FAIL] Model 'qwen3-coder' not pulled"),
        "{stdout}"
    );
    assert!(
        stdout.contains(
            "Run: ollama pull qwen3-coder, or use the installed qwen3-coder:30b by its exact \
             name (Ollama reads qwen3-coder as qwen3-coder:latest)"
        ),
        "{stdout}"
    );
    assert!(
        !stdout.contains("[ OK ] Model 'qwen3-coder' is pulled"),
        "{stdout}"
    );
}
