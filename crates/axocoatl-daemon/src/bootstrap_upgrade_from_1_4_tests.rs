//! A data root Axocoatl 1.4.0 wrote, as a person upgrading from 1.4 brings
//! it (`tests/fixtures/data-root-1.4.0`, which the 1.4.0 binary made with
//! Ollama 0.20.6; its README says how). This daemon starts on a copy of it,
//! keeps every Session, turn and run, and a Team reviewed on Ollama 0.20.6
//! that is applied again unchanged is observed again on the Ollama that runs
//! now, so its turns stop asking for the review (applied as it was, it kept
//! the 0.20.6 profile and the turns kept refusing).
//!
//! The daemon runs in a child process (bootstrap reads `AXOCOATL_DATA_DIR`
//! from the process environment) with a fake Podman, so no container starts.
use super::session_team::{SessionTeamApply, SessionTeamEdit};
use super::*;
use std::os::unix::fs::PermissionsExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CHILD: &str = "AXOCOATL_UPGRADE_FROM_1_4_TEST_CHILD";
const TEST: &str =
    "bootstrap::upgrade_from_1_4_tests::a_1_4_0_data_root_starts_keeps_its_history_and_reviews_its_team_again";
/// The fixture's `axocoatl run fix` (1.4.0, pass) and its Session.
const RUN: &str = "run-cbbb39b5-8605-4483-93fa-4a4e7560c58e";
const RUN_SESSION: &str = "ses-d4a64148-0070-4a0d-b8ce-3c6a2a6d6945";
/// A workbench Session with an applied Team and one turn, and one never used.
const WORKBENCH_SESSION: &str = "ses-ab45e515-4cef-4d72-9495-33336a4a4745";
const EMPTY_SESSION: &str = "ses-6628f8ac-ccfe-4bd0-a7cd-e1afc1596148";

/// Ollama 0.40.6 with the fixture's two models (new digests, as 0.40's
/// registry serves them).
async fn ollama() -> MockServer {
    let server = MockServer::start().await;
    crate::ollama_contract_test_support::mount_contract_probes(&server).await;
    let models = ["writer-model:latest", "reviewer-model:latest"];
    let digest = |index: usize| format!("{:064x}", index + 0x40_06);
    for (verb, route, body) in [
        (
            "GET",
            "/api/version",
            serde_json::json!({"version": axocoatl_llm_ollama::AUDITED_OLLAMA_VERSION}),
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
            serde_json::json!({"models": models.iter().enumerate().map(|(index, name)| {
                serde_json::json!({"name": name, "model": name, "digest": digest(index)})
            }).collect::<Vec<_>>()}),
        ),
        (
            "GET",
            "/api/ps",
            serde_json::json!({"models": models.iter().enumerate().map(|(index, name)| {
                serde_json::json!({"name": name, "model": name, "digest": digest(index),
                    "details": {"format": "gguf"}, "context_length": 32768, "runner": "llamacpp"})
            }).collect::<Vec<_>>()}),
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
        .respond_with(|request: &wiremock::Request| {
            let body: serde_json::Value = request.body_json().unwrap_or_default();
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "model": body["model"], "created_at": "2026-10-23T00:00:00Z",
                "response": "", "done": true, "done_reason": "load"
            }))
        })
        .mount(&server)
        .await;
    server
}

/// How many times a model was loaded (each observation loads it once).
async fn loads(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.url.path() == "/api/generate")
        .count()
}

/// The Team as it is, applied again the way the app does it (Edit, Preview,
/// Apply with nothing changed). The fixture's budget expiry has passed, so
/// each slot gets a new one, which is not a change to the Agent.
fn unchanged_edit(view: &super::session_team::SessionTeamView, command: &str) -> SessionTeamEdit {
    let expires_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 24 * 60 * 60 * 1000;
    let mut slots = view.slots.clone();
    for slot in &mut slots {
        slot.expires_at_ms = Some(expires_at_ms);
    }
    SessionTeamEdit {
        command_id: command.into(),
        expected_configuration_revision: view.configuration_revision,
        slots,
        dependencies: view.dependencies.clone(),
        layout: view.layout.clone(),
        required_checks: view.required_checks.clone(),
        required_review: view.required_review.clone(),
        check_options: vec![],
    }
}

async fn child_body() {
    // The parent reads what the daemon logs.
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
    let server = ollama().await;
    let config = format!(
        r#"
agents:
  - id: lead
    name: "Lead"
    provider: ollama
    model: "writer-model"
    role: autonomous
    tools: [read_file, list_dir, grep, glob, write_file, edit_file, bash]
    sampling:
      max_tokens: 4096
providers:
  ollama:
    base_url: {}
consolidation:
  enabled: false
"#,
        server.uri()
    );
    let config =
        axocoatl_config::parse_config(&config, std::path::Path::new("upgrade.yaml")).unwrap();
    let daemon = AxocoatlDaemon::bootstrap_headless(config).await.unwrap();
    assert!(daemon.uses_native_session_history());

    // Every Session 1.4.0 made is there, with its History.
    let listed: Vec<String> = daemon
        .list_sessions()
        .await
        .into_iter()
        .map(|session| session.id)
        .collect();
    for session in [RUN_SESSION, WORKBENCH_SESSION, EMPTY_SESSION] {
        assert!(
            listed.iter().any(|id| id == session),
            "{session}: {listed:?}"
        );
    }
    for (session, input) in [
        (RUN_SESSION, "Create fixed.txt saying fixed"),
        (WORKBENCH_SESSION, "Add notes.txt"),
    ] {
        let history = daemon
            .versioned_session_history_snapshot(session)
            .await
            .unwrap();
        let turns = history.entries(axocoatl_session::session_history::HistoryVisibility::Visible);
        assert_eq!(turns.len(), 1, "{session}");
        let axocoatl_session::session_history::SessionHistoryEntry::ExecutionV2(turn) = turns[0]
        else {
            panic!("{session}: 1.4.0 recorded native execution");
        };
        assert_eq!(
            turn.state,
            axocoatl_session::turn_contract::LogicalTurnState::Completed,
            "{session}"
        );
        let axocoatl_session::execution_content::ContentResolution::Available {
            content: request,
            ..
        } = &turn.request
        else {
            panic!("{session}: its request is kept");
        };
        assert!(request.display_input.contains(input), "{request:?}");
    }
    assert!(daemon
        .versioned_session_history_snapshot(EMPTY_SESSION)
        .await
        .unwrap()
        .entries(axocoatl_session::session_history::HistoryVisibility::Visible)
        .is_empty());
    // The run, its Outcome and its verdict.
    let run = daemon.loadout_run(RUN).await.unwrap();
    assert_eq!(run.session_id, RUN_SESSION);
    let outcome = run.outcome.expect("the 1.4.0 run's Outcome");
    assert_eq!(outcome.exit_code, 0);

    // The run's Team was reviewed on Ollama 0.20.6. Applied again exactly as
    // it is, its writer is observed again on 0.40.6 and starts a new
    // conversation; applied once more, the new profile is kept.
    let view = daemon.session_team(RUN_SESSION).await.unwrap();
    assert!(view.approved);
    assert_eq!(view.slots.len(), 1);
    assert_eq!(
        (
            view.slots[0].provider.as_str(),
            view.slots[0].model.as_str()
        ),
        ("ollama", "writer-model")
    );
    let before = loads(&server).await;
    let edit = unchanged_edit(&view, "reapply-after-upgrade");
    let preview = daemon
        .preview_session_team(RUN_SESSION, edit.clone())
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    let writer = preview
        .changes
        .iter()
        .find(|change| change.slot_id == view.slots[0].slot_id)
        .unwrap();
    assert_eq!(
        (writer.kind.as_str(), writer.history.as_str()),
        ("replaced", "new conversation")
    );
    assert!(
        loads(&server).await > before,
        "the writer is observed again"
    );
    daemon
        .apply_session_team(
            RUN_SESSION,
            SessionTeamApply {
                edit,
                review_digest: preview.review_digest,
            },
        )
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    let view = daemon.session_team(RUN_SESSION).await.unwrap();
    let preview = daemon
        .preview_session_team(RUN_SESSION, unchanged_edit(&view, "reapply-again"))
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    let writer = preview
        .changes
        .iter()
        .find(|change| change.slot_id == view.slots[0].slot_id)
        .unwrap();
    assert_eq!(
        (writer.kind.as_str(), writer.history.as_str()),
        ("retained", "preserved")
    );
    daemon.shutdown().await.unwrap();
}

/// The fixture as it is in the repository: `dot-axocoatl-daemon.lock` is the
/// data root's `.axocoatl-daemon.lock` (which `.gitignore` keeps out), and
/// Git keeps no permissions, so the copy gets the data root's own.
fn copy_fixture(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir(to).unwrap();
    std::fs::set_permissions(to, std::fs::Permissions::from_mode(0o700)).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        let name = if name == "dot-axocoatl-daemon.lock" {
            std::ffi::OsString::from(".axocoatl-daemon.lock")
        } else {
            name
        };
        let target = to.join(name);
        if entry.file_type().unwrap().is_dir() {
            copy_fixture(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }
}

#[tokio::test]
async fn a_1_4_0_data_root_starts_keeps_its_history_and_reviews_its_team_again() {
    if std::env::var_os(CHILD).is_some() {
        child_body().await;
        return;
    }
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/data-root-1.4.0/root");
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    let data = root.join("data");
    copy_fixture(&fixture, &data);
    let bin = root.join("bin");
    std::fs::create_dir(&bin).unwrap();
    let podman = bin.join("podman");
    std::fs::write(
        &podman,
        r#"#!/bin/sh
case "$*" in
  --version) printf 'podman version 5.0.0\n' ;;
  'machine list --format json') printf '[{"Running":true}]\n' ;;
  'info --format json') printf '{}\n' ;;
  'info --format {{.Host.Security.Rootless}}') printf 'true\n' ;;
  'ps '*) ;;
  'volume ls '*) ;;
  'rm '*|'volume rm '*|'network rm '*) ;;
  *) printf 'unexpected Podman command: %s\n' "$*" >&2; exit 1 ;;
esac
"#,
    )
    .unwrap();
    std::fs::set_permissions(&podman, std::fs::Permissions::from_mode(0o700)).unwrap();
    let output = tokio::time::timeout(
        Duration::from_secs(300),
        tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env(CHILD, "1")
            .env("AXOCOATL_DATA_DIR", &data)
            .env("AXOCOATL_SOCKET_PATH", root.join("ipc/daemon.sock"))
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .current_dir(&root)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "{text}");
    // A copy of a 1.4.0 root (a restored backup, a migrated Mac) is taken
    // as one, not refused.
    assert!(
        text.contains("was copied or moved since its ownership was recorded"),
        "{text}"
    );
}
