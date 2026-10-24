//! A request to a Session whose Team's budget expiry has passed is refused
//! before any model is asked, with the Agent's name and how to renew the
//! budget: Team & budget's Edit fills in a new expiry. Applied again the way
//! Edit drafts it (a new expiry, the suggested lifetime from now, nothing
//! else changed), the Agent keeps its conversation and the next request
//! runs.
//!
//! The daemon runs in a child process (bootstrap reads `AXOCOATL_DATA_DIR`
//! from the process environment) with a fake Podman, and the Session's Ready
//! runtime runs nothing, so no container starts.
use super::native_session_test_support::{native_session_with_team, ready, send};
use super::session_team::{SessionTeamApply, SessionTeamEdit};
use super::*;
use crate::child_daemon_test_support::{
    answer_chats, audited_ollama, log_warnings_to_stderr, run_child, succeeded,
};
use axocoatl_session::session_history::{HistoryVisibility, SessionHistoryEntry};
use axocoatl_session::turn_contract::LogicalTurnState;
use std::os::unix::fs::PermissionsExt;
use wiremock::MockServer;

const CHILD: &str = "AXOCOATL_EXPIRED_BUDGET_TEST_CHILD";
const TEST: &str =
    "bootstrap::expired_budget_tests::a_request_after_its_budget_expired_says_how_to_renew_it";
const MODEL: &str = "budget-model:latest";

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// How many requests reached the model.
async fn chats(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.url.path() == "/api/chat")
        .count()
}

/// Preview and Apply the Team of `session` as it is, except that every
/// Agent's budget expires at `expires_at_ms`, as `command`; the lead's change
/// in Preview, `(kind, history)`.
async fn apply_with_expiry(
    daemon: &AxocoatlDaemon,
    session: &str,
    command: &str,
    expires_at_ms: u64,
) -> (String, String) {
    let view = daemon.session_team(session).await.unwrap();
    let mut slots = view.slots.clone();
    for slot in &mut slots {
        slot.expires_at_ms = Some(expires_at_ms);
    }
    let edit = SessionTeamEdit {
        command_id: command.into(),
        expected_configuration_revision: view.configuration_revision,
        slots,
        dependencies: view.dependencies.clone(),
        layout: view.layout.clone(),
        required_checks: view.required_checks.clone(),
        required_review: view.required_review.clone(),
        check_options: vec![],
    };
    let preview = daemon
        .preview_session_team(session, edit.clone())
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    daemon
        .apply_session_team(
            session,
            SessionTeamApply {
                edit,
                review_digest: preview.review_digest.clone(),
            },
        )
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    let [change] = preview.changes.as_slice() else {
        panic!("one Agent");
    };
    (change.kind.clone(), change.history.clone())
}

/// The states of the Session's turns, in order.
async fn turns(daemon: &AxocoatlDaemon, session: &str) -> Vec<LogicalTurnState> {
    daemon
        .versioned_session_history_snapshot(session)
        .await
        .unwrap()
        .entries(HistoryVisibility::Visible)
        .iter()
        .map(|entry| match entry {
            SessionHistoryEntry::ExecutionV2(turn) => turn.state,
            other => panic!("a native turn: {other:?}"),
        })
        .collect()
}

async fn child_body() {
    log_warnings_to_stderr();
    let server = audited_ollama(&[MODEL]).await;
    answer_chats(&server, "Done.").await;
    let config = format!(
        r#"
agents:
  - id: lead
    name: "Lead"
    provider: ollama
    model: "{MODEL}"
    role: autonomous
    tools: []
    sampling:
      max_tokens: 1024
providers:
  ollama:
    base_url: {}
sandbox:
  backend: podman
  network: bridge
consolidation:
  enabled: false
"#,
        server.uri()
    );
    let config =
        axocoatl_config::parse_config(&config, std::path::Path::new("budget.yaml")).unwrap();
    let repo = std::path::PathBuf::from(std::env::var_os("AXOCOATL_EXPIRED_BUDGET_REPO").unwrap());
    let daemon = AxocoatlDaemon::bootstrap_headless(config).await.unwrap();
    let session = native_session_with_team(&daemon, &repo, "Budget", "first-team").await;
    ready(&daemon, &session).await;
    send(&daemon, &session.id)
        .await
        .unwrap_or_else(|error| panic!("the first turn: {error}"));

    // A budget that expires in a few seconds, as a person may choose.
    let expiry = now_ms() + 3_000;
    assert_eq!(
        apply_with_expiry(&daemon, &session.id, "short-budget", expiry).await,
        ("retained".into(), "preserved".into())
    );
    tokio::time::sleep(std::time::Duration::from_millis(
        expiry.saturating_sub(now_ms()) + 100,
    ))
    .await;
    // Once it has passed, a request is refused before the model is asked,
    // and the refusal says what to do.
    let asked = chats(&server).await;
    let refused = send(&daemon, &session.id)
        .await
        .expect_err("the budget expired")
        .to_string();
    assert!(
        refused.contains(
            "Lead's budget expired before this request could start; open Team & budget and \
             select Edit: it fills in a new expiry. Then Preview and Apply"
        ),
        "{refused}"
    );
    assert_eq!(chats(&server).await, asked, "the model was not asked");

    // Applied again as Edit drafts it, Lead keeps its conversation and the
    // next request runs.
    let view = daemon.session_team(&session.id).await.unwrap();
    assert!(view.slots[0].expires_at_ms.unwrap() <= now_ms());
    let lifetime = view.suggested_budget.as_ref().unwrap().expires_in_ms;
    assert_eq!(
        apply_with_expiry(&daemon, &session.id, "renewed", now_ms() + lifetime).await,
        ("retained".into(), "preserved".into())
    );
    send(&daemon, &session.id)
        .await
        .unwrap_or_else(|error| panic!("the turn after the renewal: {error}"));
    assert_eq!(
        turns(&daemon, &session.id).await,
        [LogicalTurnState::Completed, LogicalTurnState::Completed]
    );
    daemon.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_request_after_its_budget_expired_says_how_to_renew_it() {
    if std::env::var_os(CHILD).is_some() {
        child_body().await;
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    let data = root.join("data");
    std::fs::create_dir(&data).unwrap();
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).unwrap();
    let repo = root.join("repo");
    std::fs::create_dir(&repo).unwrap();
    std::fs::write(repo.join("README.md"), "# repo\n").unwrap();
    let envs = [("AXOCOATL_EXPIRED_BUDGET_REPO", repo.as_os_str())];
    succeeded(&run_child(TEST, CHILD, "1", &root, &data, &envs).await);
}
