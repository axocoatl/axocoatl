//! `axocoatl run` against a real daemon and a real Podman-backed loadout
//! Session: the built-in `fix` loadout with a scripted local model (writer
//! and reviewer on the same model, so admission warns), `--keep branch`,
//! `--junit` and `--record`. The record file must be the bundle the API
//! serves after Keep, each warning prints once, and a refused run never
//! says it started. Run explicitly:
//!
//! ```text
//! CONTAINER_CONNECTION=<machine> cargo test -p axocoatl-cli \
//!   --test run_podman -- --ignored --nocapture
//! ```
//!
//! Set AXOCOATL_E2E_MODEL_CACHE to a directory with the all-MiniLM-L6-v2
//! files to avoid downloading the embedding model.
#![cfg(unix)]

mod support;

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use support::{axocoatl, client, free_port, Daemon};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const MODEL: &str = "stub-coder:latest";
const RUN_TIMEOUT: Duration = Duration::from_secs(600);
const MODEL_FILES: [&str; 3] = ["config.json", "tokenizer.json", "model.safetensors"];

/// The writer writes `fixed.txt` and then says so; the reviewer approves.
struct ScriptedModel;

impl Respond for ScriptedModel {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: serde_json::Value = request.body_json().unwrap_or_default();
        let writer = body["tools"].as_array().is_some_and(|tools| {
            tools
                .iter()
                .any(|tool| tool["function"]["name"] == "write_file")
        });
        let answered = body["messages"]
            .as_array()
            .is_some_and(|messages| messages.iter().any(|message| message["role"] == "tool"));
        let message = match (writer, answered) {
            (true, false) => serde_json::json!({"role": "assistant", "content": "",
                "tool_calls": [{"id": "call_write", "function": {"index": 0,
                    "name": "write_file",
                    "arguments": {"path": "fixed.txt", "content": "fixed\n"}}}]}),
            (true, true) => {
                serde_json::json!({"role": "assistant", "content": "I wrote fixed.txt."})
            }
            (false, _) => serde_json::json!({"role": "assistant",
                "content": "VERDICT: APPROVE\nNo findings."}),
        };
        let response = serde_json::json!({
            "model": MODEL, "message": message, "done": true, "done_reason": "stop",
            "prompt_eval_count": 10, "eval_count": 2
        });
        ResponseTemplate::new(200).set_body_raw(format!("{response}\n"), "application/x-ndjson")
    }
}

/// A local Ollama the native provider admits, answering with
/// [`ScriptedModel`].
async fn model_server() -> MockServer {
    let server = MockServer::start().await;
    let digest = "a80c4f17acd55265feec403c7aef86be0c25983ab279d83f3bcd3abbcb5b8b72";
    for (verb, route, body) in [
        (
            "GET",
            "/api/version",
            serde_json::json!({"version": "0.20.6"}),
        ),
        (
            "GET",
            "/api/status",
            serde_json::json!({"cloud": {"disabled": true}}),
        ),
        (
            "POST",
            "/api/show",
            serde_json::json!({"details": {"format": "gguf"}, "capabilities": ["completion", "tools"]}),
        ),
        (
            "GET",
            "/api/tags",
            serde_json::json!({"models": [{"name": MODEL, "model": MODEL, "digest": digest}]}),
        ),
        (
            "GET",
            "/api/ps",
            serde_json::json!({"models": [{"name": MODEL, "model": MODEL, "digest": digest,
                "details": {"format": "gguf"}, "context_length": 32768}]}),
        ),
    ] {
        Mock::given(method(verb))
            .and(path(route))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
    }
    Mock::given(method("POST"))
        .and(path("/api/generate"))
        .respond_with(|request: &Request| {
            let body: serde_json::Value = request.body_json().unwrap_or_default();
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "model": body["model"], "created_at": "2026-10-07T00:00:00Z",
                "response": "", "done": true, "done_reason": "load"
            }))
        })
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ScriptedModel)
        .mount(&server)
        .await;
    server
}

fn write_config(root: &Path, port: u16, model_url: &str) -> PathBuf {
    let config = root.join("axocoatl.yaml");
    std::fs::write(
        &config,
        format!(
            r#"agents:
  - id: conversation
    name: Conversation
    provider: ollama
    model: {MODEL}
    tools: [read_file]
providers:
  ollama:
    base_url: "{model_url}"
server:
  host: 127.0.0.1
  port: {port}
sandbox:
  backend: podman
  network: bridge
  require_resource_limits: false
consolidation:
  enabled: false
"#
        ),
    )
    .unwrap();
    config
}

fn copy_model_cache(data: &Path) {
    let Some(cache) = std::env::var_os("AXOCOATL_E2E_MODEL_CACHE").map(PathBuf::from) else {
        return;
    };
    if !MODEL_FILES.iter().all(|name| cache.join(name).is_file()) {
        return;
    }
    let destination = data.join("models").join("all-MiniLM-L6-v2");
    std::fs::create_dir_all(&destination).unwrap();
    for name in MODEL_FILES {
        std::fs::copy(cache.join(name), destination.join(name)).unwrap();
    }
}

fn git(directory: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(directory)
        .stderr(Stdio::inherit())
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}");
    String::from_utf8(output.stdout).unwrap()
}

/// Removes what the daemon created for these Sessions if it did not.
struct SessionGuard(Vec<String>);

impl Drop for SessionGuard {
    fn drop(&mut self) {
        for id in &self.0 {
            let names: Vec<String> = ["axo-ses-", "axo-egr-", "axo-brw-", "axo-pvw-", "axo-svc-"]
                .iter()
                .map(|prefix| format!("{prefix}{id}"))
                .collect();
            let _ = Command::new("podman")
                .args(["rm", "--force", "--ignore"])
                .args(&names)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            for kind in ["volume", "network"] {
                let Ok(listed) = Command::new("podman")
                    .args([kind, "ls", "--format", "{{.Name}}"])
                    .output()
                else {
                    continue;
                };
                for name in String::from_utf8_lossy(&listed.stdout)
                    .lines()
                    .filter(|name| name.contains(id.as_str()))
                {
                    let _ = Command::new("podman")
                        .args([kind, "rm", "--force", name])
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status();
                }
            }
        }
    }
}

/// Every Session the test daemon has: all of them are this test's.
async fn session_ids(base: &str, port: u16, token: &str) -> Vec<String> {
    let sessions: serde_json::Value = client(port)
        .get(format!("{base}/api/sessions"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    sessions
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|session| session["id"].as_str().map(str::to_string))
        .collect()
}

async fn run_cli(root: &Path, args: &[&std::ffi::OsStr]) -> Option<Output> {
    let mut command = tokio::process::Command::from(axocoatl(root));
    command.args(args).stdin(Stdio::null()).kill_on_drop(true);
    tokio::time::timeout(RUN_TIMEOUT, command.output())
        .await
        .ok()
        .map(Result::unwrap)
}

/// The bundle's header with its download time cleared (each download
/// stamps its own) and its section lines; the end line is left out, since
/// its digest covers the header.
fn sections(bundle: &[u8]) -> (serde_json::Value, Vec<String>) {
    let text = String::from_utf8(bundle.to_vec()).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines.len() > 2, "{text}");
    let mut header: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    header["created_at_ms"] = 0.into();
    let body = lines[1..lines.len() - 1]
        .iter()
        .map(|line| line.to_string())
        .collect();
    (header, body)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Podman; run this test explicitly"]
async fn a_kept_fix_run_writes_the_api_bundle_and_prints_each_warning_once() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let data = root.join("data");
    std::fs::create_dir_all(&data).unwrap();
    copy_model_cache(&data);
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("README.md"), "keep probe\n").unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    for (key, value) in [
        ("user.name", "Axocoatl Test"),
        ("user.email", "test@example.invalid"),
        ("commit.gpgsign", "false"),
    ] {
        git(&repo, &["config", key, value]);
    }
    git(&repo, &["add", "README.md"]);
    git(&repo, &["commit", "-q", "-m", "init"]);

    let model = model_server().await;
    let port = free_port();
    let config = write_config(&root, port, &model.uri());
    // Loadout runs need native Session history: upgrade the new data root
    // before the daemon starts.
    let upgraded = axocoatl(&root)
        .args(["session", "upgrade", "--confirm", "-c"])
        .arg(&config)
        .output()
        .unwrap();
    assert!(
        upgraded.status.success(),
        "{}{}",
        String::from_utf8_lossy(&upgraded.stdout),
        String::from_utf8_lossy(&upgraded.stderr)
    );
    let mut daemon = Daemon::start(&root, &config, port, 0).await;
    let token = std::fs::read_to_string(data.join("local-api-token"))
        .unwrap()
        .trim()
        .to_string();
    let base = format!("http://127.0.0.1:{port}");
    let record = root.join("out").join("run.axorecord.jsonl");
    let junit = root.join("out").join("junit.xml");
    std::fs::create_dir_all(record.parent().unwrap()).unwrap();

    // A refused run (an unknown loadout) prints its reason only.
    let refused = run_cli(
        &root,
        &[
            "run".as_ref(),
            "no-such-loadout".as_ref(),
            "--task".as_ref(),
            "t".as_ref(),
            "--repo".as_ref(),
            repo.as_os_str(),
            "--url".as_ref(),
            base.as_ref(),
            "-c".as_ref(),
            config.as_os_str(),
        ],
    )
    .await
    .expect("the refused run ended");
    let refused_err = String::from_utf8_lossy(&refused.stderr);
    assert_eq!(refused.status.code(), Some(3), "{refused_err}");
    assert!(refused.stdout.is_empty());
    assert!(!refused_err.contains("starting loadout"), "{refused_err}");
    assert!(!refused_err.contains("· run "), "{refused_err}");

    let writer = format!("writer=ollama:{MODEL}");
    let reviewer = format!("reviewer=ollama:{MODEL}");
    let ran = run_cli(
        &root,
        &[
            "run".as_ref(),
            "fix".as_ref(),
            "--task".as_ref(),
            "Write fixed.txt with the word fixed.".as_ref(),
            "--repo".as_ref(),
            repo.as_os_str(),
            "--model".as_ref(),
            writer.as_ref(),
            "--model".as_ref(),
            reviewer.as_ref(),
            "--check".as_ref(),
            "true".as_ref(),
            "--keep".as_ref(),
            "branch".as_ref(),
            "--junit".as_ref(),
            junit.as_os_str(),
            "--record".as_ref(),
            record.as_os_str(),
            "--url".as_ref(),
            base.as_ref(),
            "-c".as_ref(),
            config.as_os_str(),
        ],
    )
    .await;
    // Whatever happens next, this test's Sessions are removed.
    let _guard = SessionGuard(session_ids(&base, port, &token).await);
    let ran = ran.unwrap_or_else(|| panic!("the run did not end\n{}", daemon.logs()));
    let out = String::from_utf8_lossy(&ran.stdout).to_string();
    let err = String::from_utf8_lossy(&ran.stderr).to_string();
    assert_eq!(
        ran.status.code(),
        Some(0),
        "stdout:\n{out}\nstderr:\n{err}\ndaemon:\n{}",
        daemon.logs()
    );

    // Admission returned the same-model warning and recorded it as a run
    // event; it is printed once.
    assert_eq!(
        err.matches("! warning same_model_reviewer: ").count(),
        1,
        "{err}"
    );
    let starting = err.find("· starting loadout fix in ").expect(&err);
    let run_line = err.find("· run run-").expect(&err);
    assert!(starting < run_line, "{err}");
    assert!(out.starts_with("Verdict: pass (exit 0)\n"), "{out}");
    let kept = out
        .lines()
        .find_map(|line| line.strip_prefix("Kept: branch "))
        .unwrap_or_else(|| panic!("no Kept line in\n{out}"));
    let branch = kept.split(" at ").next().unwrap();
    assert!(
        git(&repo, &["show", "--name-only", "--format=", branch])
            .lines()
            .any(|line| line == "fixed.txt"),
        "{branch}"
    );

    // The file is the bundle `GET /api/runs/{id}/record` serves after Keep.
    let written = std::fs::read(&record).unwrap();
    let header: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&written).lines().next().unwrap()).unwrap();
    let run_id = header["run_id"].as_str().unwrap();
    let served = client(port)
        .get(format!("{base}/api/runs/{run_id}/record"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .bytes()
        .await
        .unwrap();
    let (written_header, written_sections) = sections(&written);
    let (served_header, served_sections) = sections(&served);
    assert_eq!(written_header, served_header);
    let first_difference = written_sections
        .iter()
        .zip(&served_sections)
        .position(|(written, served)| written != served);
    assert!(
        first_difference.is_none() && written_sections.len() == served_sections.len(),
        "the written bundle has {} section lines, the API's {}; first different line: {:?}",
        written_sections.len(),
        served_sections.len(),
        first_difference
    );
    assert!(
        written_sections.iter().any(|line| {
            let line: serde_json::Value = serde_json::from_str(line).unwrap();
            line["section"] == "run_event" && line["data"]["event"]["phase"] == "keep"
        }),
        "no keep event in the written bundle"
    );
    let verified = Command::new(env!("CARGO_BIN_EXE_axocoatl"))
        .args(["record", "verify"])
        .arg(&record)
        .output()
        .unwrap();
    assert!(
        verified.status.success(),
        "{}",
        String::from_utf8_lossy(&verified.stdout)
    );
    let xml = std::fs::read_to_string(&junit).unwrap();
    assert!(
        xml.contains(r#"<property name="axocoatl.warning.same_model_reviewer""#),
        "{xml}"
    );

    daemon.stop();
}
