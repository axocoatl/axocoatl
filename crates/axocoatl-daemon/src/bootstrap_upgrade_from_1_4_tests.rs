//! A data root Axocoatl 1.4.0 wrote, as a person upgrading from 1.4 brings
//! it (`tests/fixtures/data-root-1.4.0`, which the released 1.4.0 binary
//! made with Ollama 0.20.6; its README says how). This daemon starts on a
//! copy of it whose Sessions find their Workspace folder, as after an
//! upgrade in place, and keeps every Session, turn and run. Each profile of
//! a Team reviewed on Ollama 0.20.6, its Agents' and the helpers they
//! delegate to, refuses to connect with the 0.20.6 message (the step at which
//! a turn or a delegated helper connects its model) until the Team is
//! applied again; a request whose budget expiry has passed is refused before
//! that step (`expired_budget_tests`). Applied again the way Team and
//! budget's Edit drafts it (nothing changed but a new budget expiry where the
//! approved one has passed), its Agents and helpers are observed again on
//! the Ollama that runs now, Preview says why, and they connect; applied once
//! more, they are kept as they are.
//!
//! The daemon runs in a child process (bootstrap reads `AXOCOATL_DATA_DIR`
//! from the process environment) with a fake Podman, so no container starts.
use super::session_team::{
    SessionTeamApply, SessionTeamEdit, SessionTeamObservedAgain, SessionTeamPreview,
    SessionTeamView,
};
use super::*;
use crate::child_daemon_test_support::{
    audited_ollama, log_warnings_to_stderr, run_child, succeeded,
};
use std::os::unix::fs::PermissionsExt;
use wiremock::MockServer;

const CHILD: &str = "AXOCOATL_UPGRADE_FROM_1_4_TEST_CHILD";
const REPO: &str = "AXOCOATL_UPGRADE_FROM_1_4_REPO";
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
/// Where the fixture was made: the Workspace folder its Sessions name, and
/// the Ollama (`providers.ollama.base_url`) its profiles recorded.
const RECORDED_REPO: &str = "/private/tmp/axo140/repo";
const RECORDED_OLLAMA: &str = "http://127.0.0.1:8373";
/// Why Apply observes a profile 1.4.0 recorded again.
const ON_0_20_6: &str =
    "this Team was reviewed on Ollama 0.20.6, which native Sessions no longer accept";

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

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// The Team as it is, as `command`.
fn as_it_is(view: &SessionTeamView, command: &str) -> SessionTeamEdit {
    SessionTeamEdit {
        command_id: command.into(),
        expected_configuration_revision: view.configuration_revision,
        slots: view.slots.clone(),
        dependencies: view.dependencies.clone(),
        layout: view.layout.clone(),
        required_checks: view.required_checks.clone(),
        required_review: view.required_review.clone(),
        check_options: vec![],
    }
}

/// The Team as Team and budget's Edit drafts it: as it is, except that an
/// Agent whose budget expiry has passed gets a new one, the suggested
/// lifetime from now (`session-team.js`, which says so; the browser journey
/// `session-team-expired-budget` goes through it). Nothing else changes.
fn as_edit_drafts_it(view: &SessionTeamView, command: &str) -> SessionTeamEdit {
    let lifetime = view.suggested_budget.as_ref().unwrap().expires_in_ms;
    let mut edit = as_it_is(view, command);
    let now = now_ms();
    for slot in &mut edit.slots {
        if slot.expires_at_ms.is_some_and(|expiry| expiry <= now) {
            slot.expires_at_ms = Some(now + lifetime);
        }
    }
    edit
}

/// Preview and Apply the Team of `session` as Edit drafts it, as `command`,
/// and return the Preview.
async fn apply_again(daemon: &AxocoatlDaemon, session: &str, command: &str) -> SessionTeamPreview {
    let view = daemon.session_team(session).await.unwrap();
    let edit = as_edit_drafts_it(&view, command);
    let preview = daemon
        .preview_session_team(session, edit.clone())
        .await
        .unwrap_or_else(|error| panic!("{session}: {error}"));
    daemon
        .apply_session_team(
            session,
            SessionTeamApply {
                edit,
                review_digest: preview.review_digest.clone(),
            },
        )
        .await
        .unwrap_or_else(|error| panic!("{session}: {error}"));
    preview
}

/// The lead's change in `preview`: `(kind, history)`.
fn lead_change(preview: &SessionTeamPreview) -> (&str, &str) {
    let change = preview
        .changes
        .iter()
        .find(|change| change.slot_id == preview.edit.slots[0].slot_id)
        .unwrap();
    (change.kind.as_str(), change.history.as_str())
}

/// What Preview says is observed again, in order.
fn observed_again(preview: &SessionTeamPreview) -> Vec<(Option<&str>, &str)> {
    let mut observed: Vec<_> = preview
        .observed_again
        .iter()
        .map(|SessionTeamObservedAgain { helper, reason, .. }| (helper.as_deref(), reason.as_str()))
        .collect();
    observed.sort_unstable();
    observed
}

/// Each profile of `session`'s Team 1.4.0 recorded on Ollama 0.20.6 at the
/// recorded endpoint: connected with that endpoint, as the person's
/// configuration names it after an upgrade in place, each refuses with the
/// 0.20.6 message. (The test runs no Ollama at that endpoint, so the message
/// is usually the one for an Ollama that does not answer; an Ollama that
/// answers there is named in it.)
async fn refused_on_0_20_6(daemon: &AxocoatlDaemon, session: &str, expected: usize) {
    let profiles = daemon
        .connect_team_profiles(session, Some(RECORDED_OLLAMA))
        .await;
    assert_eq!(profiles.len(), expected, "{session}: {profiles:?}");
    for profile in &profiles {
        assert_eq!(
            (profile.server_version.as_str(), profile.base_url.as_str()),
            ("0.20.6", RECORDED_OLLAMA),
            "{profile:?}"
        );
        let refusal = profile.connected.as_ref().unwrap_err();
        assert!(
            refusal.contains("0.20.6") && refusal.contains("review Team & budget again"),
            "{profile:?}"
        );
    }
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

    // Every Session 1.4.0 made is there, with its History, and works in the
    // Workspace folder it recorded, which is there.
    let repo = std::path::PathBuf::from(std::env::var_os(REPO).unwrap());
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
        let session = daemon.get_session(session).await.unwrap();
        assert_eq!(session.working_dir, repo);
        daemon
            .require_session_folder(&session)
            .await
            .unwrap_or_else(|error| panic!("{error}"));
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

    // The run's Team was reviewed on Ollama 0.20.6: its writer refuses with
    // the 0.20.6 message.
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
    refused_on_0_20_6(&daemon, RUN_SESSION, 1).await;
    // Its budget expired, as one approved a day before an upgrade has: the
    // Team exactly as it is cannot be applied again, and the refusal says why.
    assert!(view.slots[0].expires_at_ms.unwrap() <= now_ms());
    let refusal = daemon
        .preview_session_team(RUN_SESSION, as_it_is(&view, "as-it-is"))
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(
        refusal.contains(&format!(
            "{}'s budget expiry has passed; choose a time in the future",
            view.slots[0].name
        )),
        "{refusal}"
    );
    // Applied again as Edit drafts it, its writer is observed again on
    // 0.40.6, for the reason Preview gives, and starts a new conversation.
    let before = loads(&server, "writer-model:latest").await;
    let preview = apply_again(&daemon, RUN_SESSION, "reapply-after-upgrade").await;
    assert_eq!(lead_change(&preview), ("replaced", "new conversation"));
    assert_eq!(observed_again(&preview), [(None, ON_0_20_6)]);
    assert!(
        loads(&server, "writer-model:latest").await > before,
        "the writer is observed again"
    );
    let profiles = daemon.connect_team_profiles(RUN_SESSION, None).await;
    assert_eq!(profiles.len(), 1);
    assert_eq!(
        profiles[0].server_version,
        axocoatl_llm_ollama::AUDITED_OLLAMA_VERSION
    );
    assert!(profiles[0].connected.is_ok(), "{profiles:?}");
    // Applied once more, the new profile is kept.
    let preview = apply_again(&daemon, RUN_SESSION, "reapply-again").await;
    assert_eq!(lead_change(&preview), ("retained", "preserved"));
    assert!(preview.observed_again.is_empty());

    // The Team whose lead delegates: Lead and the helpers 1.4.0 approved,
    // Reviewer and Scout, hold profiles recorded on Ollama 0.20.6, and each
    // refuses with the 0.20.6 message.
    let view = daemon.session_team(HELPERS_SESSION).await.unwrap();
    let delegation = view.slots[0].delegation.as_ref().expect("approved helpers");
    let mut approved: Vec<&str> = delegation
        .workers
        .iter()
        .map(|worker| worker.template_id.as_str())
        .collect();
    approved.sort_unstable();
    assert_eq!(approved, ["reviewer", "scout"]);
    refused_on_0_20_6(&daemon, HELPERS_SESSION, 3).await;
    // With the endpoint configured here, which is not the recorded one, each
    // is refused for that as well, and the refusal names both endpoints.
    let before = daemon.connect_team_profiles(HELPERS_SESSION, None).await;
    for profile in &before {
        // Lead's own profile, and its helpers'.
        assert_eq!(profile.slot_id, view.slots[0].slot_id, "{profile:?}");
        let refusal = profile.connected.as_ref().unwrap_err();
        assert!(
            refusal.contains(&format!(
                "providers.ollama.base_url names {}, but this Team was reviewed with the Ollama \
                 at {RECORDED_OLLAMA}; review Team & budget again",
                server.uri()
            )),
            "{refusal}"
        );
    }
    // Applied again as Edit drafts it, the lead and each helper are observed
    // again on 0.40.6; the 0.20.6 reason is the one Preview gives.
    let loaded = (
        loads(&server, "reviewer-model:latest").await,
        loads(&server, "helper-model:latest").await,
    );
    let preview = apply_again(&daemon, HELPERS_SESSION, "helpers-after-upgrade").await;
    assert_eq!(lead_change(&preview), ("replaced", "new conversation"));
    assert_eq!(
        observed_again(&preview),
        [
            (None, ON_0_20_6),
            (Some("reviewer"), ON_0_20_6),
            (Some("scout"), ON_0_20_6)
        ]
    );
    assert!(loads(&server, "reviewer-model:latest").await > loaded.0);
    assert!(loads(&server, "helper-model:latest").await > loaded.1);
    let observed = daemon.connect_team_profiles(HELPERS_SESSION, None).await;
    assert_eq!(observed.len(), 3);
    for profile in &observed {
        assert_eq!(
            profile.server_version,
            axocoatl_llm_ollama::AUDITED_OLLAMA_VERSION,
            "{profile:?}"
        );
        assert!(profile.connected.is_ok(), "{profile:?}");
        let old = before
            .iter()
            .find(|old| old.helper == profile.helper)
            .unwrap();
        assert_ne!(profile.definition, old.definition, "{profile:?}");
    }
    // Applied once more, they are kept as they are: nothing is loaded.
    let loaded = (
        loads(&server, "reviewer-model:latest").await,
        loads(&server, "helper-model:latest").await,
    );
    let preview = apply_again(&daemon, HELPERS_SESSION, "helpers-again").await;
    assert_eq!(lead_change(&preview), ("retained", "preserved"));
    assert!(preview.observed_again.is_empty());
    assert_eq!(
        (
            loads(&server, "reviewer-model:latest").await,
            loads(&server, "helper-model:latest").await,
        ),
        loaded
    );
    let kept = daemon.connect_team_profiles(HELPERS_SESSION, None).await;
    assert_eq!(kept.len(), 3);
    for profile in &kept {
        assert!(profile.connected.is_ok(), "{profile:?}");
        let observed = observed
            .iter()
            .find(|observed| observed.helper == profile.helper)
            .unwrap();
        assert_eq!(profile.definition, observed.definition, "{profile:?}");
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

/// The repository `make.sh` starts from, at `repo`.
fn make_repository(repo: &std::path::Path) {
    std::fs::create_dir(repo).unwrap();
    std::fs::write(
        repo.join("README.md"),
        "A tiny repository for the Axocoatl 1.4.0 data-root fixture.\n",
    )
    .unwrap();
    for args in [
        &["init", "--quiet", "--initial-branch=main"][..],
        &["add", "README.md"],
        &[
            "-c",
            "user.email=fixture@example.com",
            "-c",
            "user.name=Fixture",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "-m",
            "Add the README",
        ],
    ] {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

/// Point the copy's Workspace and Sessions at `repo` where they name the
/// folder the fixture was made in: the folder is there, as it is after an
/// upgrade in place. What the History recorded about past work (its runtime
/// root, the run's manifest) keeps the path it was done at.
fn relocate(data: &std::path::Path, repo: &std::path::Path) {
    let repo = repo.to_str().unwrap();
    for directory in ["workspaces", "sessions"] {
        for entry in std::fs::read_dir(data.join(directory)).unwrap() {
            let path = entry.unwrap().path();
            let text = std::fs::read_to_string(&path).unwrap();
            let recorded = format!("\"{RECORDED_REPO}\"");
            assert!(text.contains(&recorded), "{}", path.display());
            std::fs::write(&path, text.replace(&recorded, &format!("\"{repo}\""))).unwrap();
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
    let repo = root.join("repo");
    make_repository(&repo);
    relocate(&data, &repo);
    let text =
        succeeded(&run_child(TEST, CHILD, "1", &root, &data, &[(REPO, repo.as_os_str())]).await);
    // A copy of a 1.4.0 root (a restored backup, a migrated Mac) is taken
    // as one, not refused.
    assert!(
        text.contains("was copied or moved since its ownership was recorded"),
        "{text}"
    );
    // No Session is skipped for a missing Workspace folder.
    assert!(!text.contains("no longer exists"), "{text}");
}
