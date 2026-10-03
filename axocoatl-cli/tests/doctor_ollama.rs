//! `axocoatl doctor` against a stand-in Ollama server: a server native
//! Sessions refuse must fail doctor, with the setting that fixes it.

use std::path::Path;
use std::process::Command;

use serde_json::{json, Value};
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

async fn ollama(version: Value, status: Option<Value>) -> MockServer {
    let server = MockServer::start().await;
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
    let config = root.join("axocoatl.yaml");
    std::fs::write(
        &config,
        format!(
            r#"agents:
  - id: lead
    name: Lead
    provider: ollama
    model: "qwen3-coder:30b"
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
    // What an Ollama 0.20.6 server started without OLLAMA_NO_CLOUD reports.
    let server = ollama(
        json!({"version": "0.20.6"}),
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
        json!({"version": "0.20.6"}),
        Some(json!({"cloud": {"disabled": true, "source": "env"}})),
    )
    .await;
    let root = tempfile::tempdir().unwrap();
    let (ok, stdout) = doctor(root.path(), &server.uri());

    assert!(ok, "doctor must pass:\n{stdout}");
    let url = server.uri();
    assert!(stdout.contains(&format!(
        "[ OK ] Ollama 0.20.6 at {url}, the version native Sessions accept"
    )));
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
        stdout.contains("reports version 0.12.3; native Sessions accept only Ollama 0.20.6"),
        "{stdout}"
    );
    assert!(
        stdout.contains("does not report its cloud mode (GET /api/status)"),
        "{stdout}"
    );
}
