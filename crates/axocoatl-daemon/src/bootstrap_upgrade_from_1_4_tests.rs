//! A data root Axocoatl 1.4.0 wrote, as a person upgrading from 1.4 brings
//! it (`tests/fixtures/data-root-1.4.0`, which the released 1.4.0 binary
//! made with Ollama 0.20.6; its README says how). This daemon starts on a copy of it,
//! keeps every Session, turn and run, and a Team reviewed on Ollama 0.20.6
//! that is applied again unchanged is observed again on the Ollama that runs
//! now, its Agents and the helpers they delegate to, so its turns and its
//! helpers stop asking for the review (applied as it was, it kept the 0.20.6
//! profiles and they kept refusing).
//!
//! The daemon runs in a child process (bootstrap reads `AXOCOATL_DATA_DIR`
//! from the process environment) with a fake Podman, so no container starts.
use super::session_team::{SessionTeamApply, SessionTeamEdit};
use super::*;
use crate::child_daemon_test_support::{
    audited_ollama, log_warnings_to_stderr, run_child, succeeded,
};
use std::os::unix::fs::PermissionsExt;
use wiremock::MockServer;

const CHILD: &str = "AXOCOATL_UPGRADE_FROM_1_4_TEST_CHILD";
const TEST: &str =
    "bootstrap::upgrade_from_1_4_tests::a_1_4_0_data_root_starts_keeps_its_history_and_reviews_its_team_again";
/// The fixture's `axocoatl run fix` (1.4.0, pass) and its Session.
const RUN: &str = "run-5613f837-f29e-4407-8308-a338348907c0";
const RUN_SESSION: &str = "ses-d14ac217-9a2c-440f-b903-dd0e8bb1dbe1";
/// A workbench Session with an applied Team and one turn; one whose lead
/// delegated to a helper in its turn; and one never used.
const WORKBENCH_SESSION: &str = "ses-774ef092-f6b4-495e-851d-ba160ce5ecd5";
const HELPERS_SESSION: &str = "ses-4efe1aa0-b8ba-40c6-ae14-292136a1a0ff";
const EMPTY_SESSION: &str = "ses-c466c04c-83bb-4a00-a584-9015c7c40830";

/// How many times `model` was loaded (each observation loads it once).
async fn loads(server: &MockServer, model: &str) -> usize {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| {
            request.url.path() == "/api/generate"
                && request.body_json::<serde_json::Value>().unwrap()["model"] == model
        })
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

/// Apply the Team of `session` again unchanged, as `command`, and return
/// its lead's change: `(kind, history)`.
async fn apply_unchanged(
    daemon: &AxocoatlDaemon,
    session: &str,
    command: &str,
) -> (String, String) {
    let view = daemon.session_team(session).await.unwrap();
    let edit = unchanged_edit(&view, command);
    let preview = daemon
        .preview_session_team(session, edit.clone())
        .await
        .unwrap_or_else(|error| panic!("{session}: {error}"));
    let lead = preview
        .changes
        .iter()
        .find(|change| change.slot_id == view.slots[0].slot_id)
        .unwrap();
    let change = (lead.kind.clone(), lead.history.clone());
    daemon
        .apply_session_team(
            session,
            SessionTeamApply {
                edit,
                review_digest: preview.review_digest,
            },
        )
        .await
        .unwrap_or_else(|error| panic!("{session}: {error}"));
    change
}

async fn child_body() {
    // The parent reads what the daemon logs.
    log_warnings_to_stderr();
    // Ollama 0.40.6 with the fixture's models (new digests, as 0.40's
    // registry serves them).
    let server = audited_ollama(&[
        "writer-model:latest",
        "reviewer-model:latest",
        "helper-model:latest",
    ])
    .await;
    // The fixture's configuration: the lead and its read-only helpers.
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
  - id: reviewer
    name: "Reviewer"
    provider: ollama
    model: "reviewer-model"
    role: worker
    tools: [read_file, list_dir, grep, glob, bash]
    writes: []
    sampling:
      max_tokens: 4096
  - id: scout
    name: "Scout"
    provider: ollama
    model: "helper-model"
    role: worker
    tools: [read_file, list_dir, grep, glob]
    writes: []
    sampling:
      max_tokens: 2048
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
    for session in [
        RUN_SESSION,
        WORKBENCH_SESSION,
        HELPERS_SESSION,
        EMPTY_SESSION,
    ] {
        assert!(
            listed.iter().any(|id| id == session),
            "{session}: {listed:?}"
        );
    }
    // Each turn's work, by the Agent that did it: the run's writer and its
    // reviewer, Lead, and Lead with the helper it delegated to.
    for (session, input, activations) in [
        (
            RUN_SESSION,
            "Create fixed.txt saying fixed",
            &["writer", "reviewer"][..],
        ),
        (WORKBENCH_SESSION, "Add notes.txt", &["Lead"][..]),
        (
            HELPERS_SESSION,
            "Ask Scout what this repository is for",
            &["Lead", "Scout"][..],
        ),
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
        let names: Vec<&str> = turn
            .activations
            .iter()
            .map(|activation| match &activation.definition_name {
                axocoatl_session::execution_content::ContentResolution::Available {
                    content,
                    ..
                } => content.as_str(),
                _ => "",
            })
            .collect();
        assert_eq!(names, activations, "{session}");
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
    let before = loads(&server, "writer-model:latest").await;
    assert_eq!(
        apply_unchanged(&daemon, RUN_SESSION, "reapply-after-upgrade").await,
        ("replaced".into(), "new conversation".into())
    );
    assert!(
        loads(&server, "writer-model:latest").await > before,
        "the writer is observed again"
    );
    assert_eq!(
        apply_unchanged(&daemon, RUN_SESSION, "reapply-again").await,
        ("retained".into(), "preserved".into())
    );

    // The helpers 1.4.0 approved, Reviewer and Scout, hold profiles recorded
    // on Ollama 0.20.6, which cannot connect: each refuses delegated work.
    let view = daemon.session_team(HELPERS_SESSION).await.unwrap();
    let delegation = view.slots[0].delegation.as_ref().expect("approved helpers");
    let mut approved: Vec<&str> = delegation
        .workers
        .iter()
        .map(|worker| worker.template_id.as_str())
        .collect();
    approved.sort_unstable();
    assert_eq!(approved, ["reviewer", "scout"]);
    let helpers = daemon.connect_approved_helpers(HELPERS_SESSION).await;
    assert_eq!(helpers.len(), 2);
    for (template, _, version, connected) in &helpers {
        assert_eq!(version, "0.20.6", "{template}");
        assert!(connected.is_err(), "{template}: {connected:?}");
    }
    // Applied again unchanged, the lead and each helper are observed again
    // on 0.40.6, and each helper connects.
    let before = (
        loads(&server, "reviewer-model:latest").await,
        loads(&server, "helper-model:latest").await,
    );
    assert_eq!(
        apply_unchanged(&daemon, HELPERS_SESSION, "helpers-after-upgrade").await,
        ("replaced".into(), "new conversation".into())
    );
    assert!(loads(&server, "reviewer-model:latest").await > before.0);
    assert!(loads(&server, "helper-model:latest").await > before.1);
    let observed = daemon.connect_approved_helpers(HELPERS_SESSION).await;
    assert_eq!(observed.len(), 2);
    for (template, definition, version, connected) in &observed {
        assert_eq!(
            version,
            axocoatl_llm_ollama::AUDITED_OLLAMA_VERSION,
            "{template}"
        );
        assert!(connected.is_ok(), "{template}: {connected:?}");
        let (_, old, ..) = helpers.iter().find(|(id, ..)| id == template).unwrap();
        assert_ne!(definition, old, "{template} is observed again");
    }
    // Applied once more, they are kept as they are: nothing is loaded.
    let before = (
        loads(&server, "reviewer-model:latest").await,
        loads(&server, "helper-model:latest").await,
    );
    assert_eq!(
        apply_unchanged(&daemon, HELPERS_SESSION, "helpers-again").await,
        ("retained".into(), "preserved".into())
    );
    assert_eq!(
        (
            loads(&server, "reviewer-model:latest").await,
            loads(&server, "helper-model:latest").await,
        ),
        before
    );
    let kept = daemon.connect_approved_helpers(HELPERS_SESSION).await;
    assert_eq!(kept.len(), 2);
    for (template, definition, _, connected) in &kept {
        assert!(connected.is_ok(), "{template}: {connected:?}");
        let (_, observed, ..) = observed.iter().find(|(id, ..)| id == template).unwrap();
        assert_eq!(definition, observed, "{template} is kept");
    }
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
    let text = succeeded(&run_child(TEST, CHILD, "1", &root, &data, &[]).await);
    // A copy of a 1.4.0 root (a restored backup, a migrated Mac) is taken
    // as one, not refused.
    assert!(
        text.contains("was copied or moved since its ownership was recorded"),
        "{text}"
    );
}
