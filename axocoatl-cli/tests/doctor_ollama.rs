//! `axocoatl doctor` against a stand-in Ollama server: a server native
//! Sessions refuse must fail doctor, with the setting that fixes it. A shim
//! Podman stands in for a ready rootless runtime, and on Linux doctor reads
//! the user's subordinate id ranges from a test directory that gives the
//! test user one (`AXOCOATL_TEST_ID_ROOT`), not from this computer's
//! `/etc/subuid`, so only Ollama decides. Only a debug build of axocoatl
//! (what `cargo test` builds) reads that variable, so on Linux these tests
//! are ignored under `cargo test --release`.

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
    // What loading a model without generating answers, the loaded model's
    // 32768-token context, and the contract probes.
    Mock::given(method("POST"))
        .and(path("/api/generate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "model": "qwen3-coder:30b", "created_at": "2026-10-19T00:00:00Z",
            "response": "", "done": true, "done_reason": "load"})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/ps"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models": [{
            "name": "qwen3-coder:30b", "model": "qwen3-coder:30b", "digest": "0".repeat(64),
            "details": {"format": "gguf"}, "context_length": 32768, "runner": "llamacpp"}]})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(|request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            let content = body["messages"][0]["content"].as_str().unwrap_or_default();
            if content.starts_with("x x ") {
                ResponseTemplate::new(400)
                    .set_body_json(json!({"error": "the prompt is longer than the context"}))
            } else {
                ResponseTemplate::new(200).set_body_raw(
                    format!(
                        "{}\n",
                        json!({"model": "qwen3-coder:30b", "message": {"role": "assistant",
                            "content": "ready"}, "done": true, "done_reason": "length",
                            "prompt_eval_count": 14, "eval_count": 1})
                    ),
                    "application/x-ndjson",
                )
            }
        })
        .mount(&server)
        .await;
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

/// A Podman that answers as a ready rootless runtime, logging nothing and
/// changing nothing.
pub fn ready_podman(root: &Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let podman = bin.join("podman");
    std::fs::write(
        &podman,
        r#"#!/bin/sh
case "$*" in
  "--version") echo "podman version 5.8.2" ;;
  "info --format json") echo '{"host":{"security":{"rootless":true},"idMappings":{"uidmap":[{"container_id":0,"host_id":1000,"size":1},{"container_id":1,"host_id":100000,"size":65536}],"gidmap":[{"container_id":0,"host_id":1000,"size":1},{"container_id":1,"host_id":100000,"size":65536}]}},"version":{"Version":"5.8.2"}}' ;;
  "machine list --format json") echo '[{"Name":"podman-machine-default","Default":true,"Running":true,"VMType":"applehv","CPUs":6,"Memory":"8589934592"}]' ;;
  "machine inspect podman-machine-default") echo '[{"Name":"podman-machine-default","Rootful":false,"Resources":{"CPUs":6,"Memory":8192}}]' ;;
  "machine info --format json") echo '{"Host":{"VMType":"applehv"}}' ;;
  *) exit 125 ;;
esac
"#,
    )
    .unwrap();
    std::fs::set_permissions(&podman, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

/// The user doctor runs as in these tests (`USER`), whatever runs them.
const TEST_USER: &str = "axocoatl-doctor-test";

/// A directory standing in for `/` where doctor reads `etc/subuid` and
/// `etc/subgid`, giving [`TEST_USER`] `lines` (none: empty files).
fn id_root(root: &Path, lines: &str) -> std::path::PathBuf {
    let ids = root.join("ids");
    std::fs::create_dir_all(ids.join("etc")).unwrap();
    for name in ["subuid", "subgid"] {
        std::fs::write(ids.join("etc").join(name), lines).unwrap();
    }
    ids
}

/// Run doctor on an explicit configuration naming `base_url`, isolated from
/// the user's configuration, data, daemon socket, Podman and subordinate id
/// ranges.
fn doctor(root: &Path, base_url: &str) -> (bool, String) {
    doctor_with_model(root, base_url, "qwen3-coder:30b")
}

/// [`doctor`] with the Lead Agent on `model`.
fn doctor_with_model(root: &Path, base_url: &str, model: &str) -> (bool, String) {
    let ids = id_root(root, &format!("{TEST_USER}:100000:65536\n"));
    let output = doctor_command(root, base_url, model)
        .env("AXOCOATL_TEST_ID_ROOT", ids)
        .output()
        .unwrap();
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
    )
}

/// The doctor command [`doctor_with_model`] runs, before its id root is set.
fn doctor_command(root: &Path, base_url: &str, model: &str) -> Command {
    let config = root.join("axocoatl.yaml");
    let bin = ready_podman(root);
    // Doctor also looks for the ollama CLI in /opt/homebrew/bin and
    // /usr/local/bin; this one, found first, answers no version, as if this
    // computer had none, so the host's own Ollama never decides.
    {
        use std::os::unix::fs::PermissionsExt;
        let ollama = bin.join("ollama");
        std::fs::write(&ollama, "#!/bin/sh\nexit 127\n").unwrap();
        std::fs::set_permissions(&ollama, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
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
server:
  port: {port}
"#
        ),
    )
    .unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_axocoatl"));
    command
        .arg("doctor")
        .arg("--config")
        .arg(&config)
        .current_dir(root)
        .env("HOME", root)
        .env("USER", TEST_USER)
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("AXOCOATL_DATA_DIR", root.join("data"))
        .env("AXOCOATL_SOCKET_PATH", root.join("no-daemon.sock"))
        .env_remove("CONTAINER_CONNECTION")
        .env_remove("CONTAINER_HOST");
    command
}

#[tokio::test]
#[cfg_attr(
    all(target_os = "linux", not(debug_assertions)),
    ignore = "AXOCOATL_TEST_ID_ROOT is read only by a debug build of axocoatl"
)]
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
        stdout.contains(&format!(
            "[FAIL] Ollama at {url} has cloud models enabled; native Sessions refuse it"
        )),
        "{stdout}"
    );
    assert!(stdout.contains(r#"{"disable_ollama_cloud": true} to ~/.ollama/server.json"#));
    assert!(stdout.contains("OLLAMA_NO_CLOUD=1 ollama serve"));
    assert!(stdout.contains("set providers.ollama.base_url to another Ollama server"));
    assert!(!stdout.contains("cloud models disabled"), "{stdout}");
    assert!(!stdout.contains("launchctl"), "{stdout}");
    assert!(stdout.contains("Not ready yet"), "{stdout}");
    assert!(stdout.ends_with("Ready: no\n"), "{stdout}");
}

#[tokio::test]
#[cfg_attr(
    all(target_os = "linux", not(debug_assertions)),
    ignore = "AXOCOATL_TEST_ID_ROOT is read only by a debug build of axocoatl"
)]
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
            "[ OK ] qwen3-coder:30b: GGUF; 18.6 GB, fits in memory; 32768-token context, contract probes passed"
        ) || stdout.contains(
            "[WARN] qwen3-coder:30b: GGUF; 18.6 GB, too large for this computer's"
        ),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!(
            "[ OK ] Ollama cloud models disabled at {url} (OLLAMA_NO_CLOUD)"
        )),
        "{stdout}"
    );
    assert!(!stdout.contains("[FAIL]"), "{stdout}");
    assert!(stdout.contains("Ready for: workbench Sessions"), "{stdout}");
    assert!(stdout.ends_with("Ready: yes\n"), "{stdout}");
    // `--config` names a configuration that is not the default one
    // (HOME's): no fix offers onboarding, which writes only the default.
    assert!(
        !stdout.contains("axocoatl onboard --update")
            && !stdout.contains("axocoatl onboard --replace"),
        "{stdout}"
    );
    if stdout.contains("too large for this computer's") {
        assert!(
            stdout.contains("(`axocoatl onboard` writes only the default configuration, "),
            "{stdout}"
        );
    }
}

#[tokio::test]
#[cfg_attr(
    all(target_os = "linux", not(debug_assertions)),
    ignore = "AXOCOATL_TEST_ID_ROOT is read only by a debug build of axocoatl"
)]
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
#[cfg_attr(
    all(target_os = "linux", not(debug_assertions)),
    ignore = "AXOCOATL_TEST_ID_ROOT is read only by a debug build of axocoatl"
)]
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
    // No ollama CLI here: upgrade, the way this platform installs it.
    if cfg!(target_os = "macos") {
        assert!(stdout.contains("brew upgrade ollama"), "{stdout}");
    } else {
        assert!(
            stdout.contains("curl -fsSL https://ollama.com/install.sh | OLLAMA_VERSION=0.40.6 sh"),
            "{stdout}"
        );
        assert!(!stdout.contains("brew"), "{stdout}");
    }
}

#[tokio::test]
#[cfg_attr(
    all(target_os = "linux", not(debug_assertions)),
    ignore = "AXOCOATL_TEST_ID_ROOT is read only by a debug build of axocoatl"
)]
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
#[cfg_attr(
    all(target_os = "linux", not(debug_assertions)),
    ignore = "AXOCOATL_TEST_ID_ROOT is read only by a debug build of axocoatl"
)]
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
#[cfg_attr(
    all(target_os = "linux", not(debug_assertions)),
    ignore = "AXOCOATL_TEST_ID_ROOT is read only by a debug build of axocoatl"
)]
async fn doctor_fails_an_untagged_model_that_only_another_tag_satisfies() {
    let server = ollama(
        json!({"version": AUDITED_OLLAMA_VERSION}),
        Some(json!({"cloud": {"disabled": true, "source": "env"}})),
    )
    .await;
    let root = tempfile::tempdir().unwrap();
    let (ok, stdout) = doctor_with_model(root.path(), &server.uri(), "qwen3-coder");
    assert!(!ok, "doctor must fail:\n{stdout}");
    let hint = "ollama pull qwen3-coder, or use the installed qwen3-coder:30b by its exact name \
                (Ollama reads qwen3-coder as qwen3-coder:latest)";
    assert!(
        stdout.contains(&format!(
            "[FAIL] qwen3-coder is not pulled\n         → {hint}\n"
        )),
        "{stdout}"
    );
    // The step that is left says the same.
    assert!(
        stdout.contains(&format!("1) qwen3-coder is not pulled: {hint}\n")),
        "{stdout}"
    );
    assert!(stdout.ends_with("Ready: no\n"), "{stdout}");
    // The installed tag is never checked in its place.
    assert!(!stdout.contains("[ OK ] qwen3-coder"), "{stdout}");
}

/// A newer Ollama minor passes doctor with a warning once the contract
/// probes pass on the model; it is no longer refused.
#[tokio::test]
#[cfg_attr(
    all(target_os = "linux", not(debug_assertions)),
    ignore = "AXOCOATL_TEST_ID_ROOT is read only by a debug build of axocoatl"
)]
async fn doctor_warns_about_a_newer_minor_and_passes_it_through_the_probes() {
    let server = ollama(
        json!({"version": "0.41.0"}),
        Some(json!({"cloud": {"disabled": true, "source": "config"}})),
    )
    .await;
    let root = tempfile::tempdir().unwrap();
    let (ok, stdout) = doctor(root.path(), &server.uri());
    assert!(ok, "doctor must pass:\n{stdout}");
    assert!(
        stdout.contains(&format!(
            "[WARN] Ollama 0.41.0 at {}, newer than the audited 0.40.6",
            server.uri()
        )),
        "{stdout}"
    );
    assert!(stdout.contains("contract probes passed"), "{stdout}");
}

/// Linux: the ranges doctor judges come from the id root the tests give it,
/// not from this computer: with no line for the test user there, doctor
/// FAILs "User ID ranges" with the usermod fix. With CONTAINER_HOST set,
/// this computer's files say nothing about that Podman, so the same empty
/// files are not judged and its `podman info` (a range) decides.
#[cfg(target_os = "linux")]
#[tokio::test]
#[cfg_attr(
    all(target_os = "linux", not(debug_assertions)),
    ignore = "AXOCOATL_TEST_ID_ROOT is read only by a debug build of axocoatl"
)]
async fn doctor_reads_the_test_id_root_and_not_for_a_podman_elsewhere() {
    let server = ollama(
        json!({"version": AUDITED_OLLAMA_VERSION}),
        Some(json!({"cloud": {"disabled": true, "source": "env"}})),
    )
    .await;
    let root = tempfile::tempdir().unwrap();
    let ids = id_root(root.path(), "");
    let run = |container_host: Option<&str>| {
        let mut command = doctor_command(root.path(), &server.uri(), "qwen3-coder:30b");
        command.env("AXOCOATL_TEST_ID_ROOT", &ids);
        if let Some(host) = container_host {
            command.env("CONTAINER_HOST", host);
        }
        let output = command.output().unwrap();
        (
            output.status.success(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
        )
    };
    // Root needs no range, and doctor does not judge it from the files.
    let root_user = Command::new("id")
        .arg("-u")
        .output()
        .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).trim() == "0");
    if !root_user {
        let (ok, stdout) = run(None);
        assert!(!ok, "doctor must fail:\n{stdout}");
        assert!(
            stdout.contains(&format!(
                "[FAIL] User ID ranges: {TEST_USER} has no range of subordinate user and group \
                 IDs in /etc/subuid and /etc/subgid"
            )),
            "{stdout}"
        );
        assert!(
            stdout.contains(&format!(
                "sudo usermod --add-subuids 100000-165535 --add-subgids 100000-165535 \
                 {TEST_USER} && podman system migrate"
            )),
            "{stdout}"
        );
    }
    let (ok, stdout) = run(Some("unix:///run/user/4242/podman/podman.sock"));
    assert!(ok, "doctor must pass:\n{stdout}");
    assert!(!stdout.contains("User ID ranges"), "{stdout}");
}
