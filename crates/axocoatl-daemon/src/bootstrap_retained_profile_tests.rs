//! A Team's retained provider profile that can no longer serve a turn for a
//! reason that observing the model again resolves: `providers.ollama.base_url`
//! names another Ollama than the one the Team was reviewed with, the Ollama
//! model was pulled again (another digest), or the OpenRouter endpoint lists
//! other terms (a price). Each turn says which, and to review Team & budget
//! again. Applied again with nothing changed, the Team observes the model
//! again, Preview says why (the Agent starts a new conversation), and the
//! next turn runs; applied once more, the new profile is kept. These refusals
//! used to repeat after every unchanged Apply. (A profile recorded on Ollama
//! 0.20.6 is another such reason: `upgrade_from_1_4_tests`.)
//!
//! Each daemon runs in a child process (bootstrap reads `AXOCOATL_DATA_DIR`
//! from the process environment) with a fake Podman, and the Session's Ready
//! runtime runs nothing, so no container starts.
use super::native_session_test_support::{native_session_with_team, ready, send};
use super::session_team::{SessionTeamApply, SessionTeamEdit, SessionTeamPreview};
use super::*;
use crate::child_daemon_test_support::{
    answer_chats, audited_ollama, log_warnings_to_stderr, run_child, serve_audited_ollama,
    succeeded,
};
use axocoatl_session::session_history::{HistoryVisibility, SessionHistoryEntry};
use axocoatl_session::turn_contract::LogicalTurnState;
use std::os::unix::fs::PermissionsExt;
use wiremock::MockServer;

const CHILD: &str = "AXOCOATL_RETAINED_PROFILE_TEST_CHILD";
const TEST: &str = "bootstrap::retained_profile_tests::a_changed_endpoint_or_a_model_pulled_again_is_observed_again_on_apply";
const OPENROUTER_TEST: &str = "bootstrap::retained_profile_tests::an_openrouter_endpoint_whose_terms_changed_is_observed_again_on_apply";
const MODEL: &str = "retained-model:latest";
const OPENROUTER_MODEL: &str = "fixture/coder";

/// How many times the model was loaded (each observation loads it once).
async fn loads(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.url.path() == "/api/generate")
        .count()
}

/// Preview and Apply the Team of `session` as it is, as `command`, and
/// return the Preview.
async fn apply_unchanged(
    daemon: &AxocoatlDaemon,
    session: &str,
    command: &str,
) -> SessionTeamPreview {
    let view = daemon.session_team(session).await.unwrap();
    let edit = SessionTeamEdit {
        command_id: command.into(),
        expected_configuration_revision: view.configuration_revision,
        slots: view.slots.clone(),
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
    preview
}

/// The lead's change in `preview`, `(kind, history)`, and the reasons
/// Preview gives for observing a model again.
fn reviewed(preview: &SessionTeamPreview) -> ((&str, &str), Vec<&str>) {
    let [change] = preview.changes.as_slice() else {
        panic!("one Agent");
    };
    assert!(preview
        .observed_again
        .iter()
        .all(|observed| observed.slot_id == change.slot_id && observed.helper.is_none()));
    (
        (change.kind.as_str(), change.history.as_str()),
        preview
            .observed_again
            .iter()
            .map(|observed| observed.reason.as_str())
            .collect(),
    )
}

/// Send a request to `session` and check that its turn completed.
async fn completes(daemon: &AxocoatlDaemon, session: &str) {
    send(daemon, session)
        .await
        .unwrap_or_else(|error| panic!("the turn: {error}"));
    let history = daemon
        .versioned_session_history_snapshot(session)
        .await
        .unwrap();
    let turns = history.entries(HistoryVisibility::Visible);
    let Some(SessionHistoryEntry::ExecutionV2(turn)) = turns.last() else {
        panic!("a native turn: {turns:?}");
    };
    assert_eq!(turn.state, LogicalTurnState::Completed);
}

async fn phase_body(phase: &str) {
    // The parent reads what the daemon logs.
    log_warnings_to_stderr();
    // Each phase starts its own Ollama, on another port: the restarted
    // daemon's configured endpoint is not the one the Team was reviewed with.
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
        axocoatl_config::parse_config(&config, std::path::Path::new("retained.yaml")).unwrap();
    let repo =
        std::path::PathBuf::from(std::env::var_os("AXOCOATL_RETAINED_PROFILE_REPO").unwrap());
    let ids = std::path::PathBuf::from(std::env::var_os("AXOCOATL_RETAINED_PROFILE_IDS").unwrap());
    let daemon = AxocoatlDaemon::bootstrap_headless(config).await.unwrap();
    if phase == "first" {
        // A workbench Session on Lead, its Team applied, and a turn.
        let session = native_session_with_team(&daemon, &repo, "Retained", "first-team").await;
        ready(&daemon, &session).await;
        completes(&daemon, &session.id).await;
        std::fs::write(&ids, format!("{}\n{}", session.id, server.uri())).unwrap();
        daemon.shutdown().await.unwrap();
        return;
    }
    let ids = std::fs::read_to_string(&ids).unwrap();
    let (session, first) = ids.split_once('\n').unwrap();
    let session = daemon.get_session(session).await.unwrap();
    ready(&daemon, &session).await;

    // Restarted with another `providers.ollama.base_url`: the turn says so.
    let endpoint = format!(
        "providers.ollama.base_url names {}, but this Team was reviewed with the Ollama at \
         {first}",
        server.uri()
    );
    let refused = send(&daemon, &session.id)
        .await
        .expect_err("the Team was reviewed with another Ollama")
        .to_string();
    assert!(
        refused.contains(&format!(
            "{endpoint}; review Team & budget again: applying the Team observes the model at \
             the endpoint configured now (or set providers.ollama.base_url back and restart \
             Axocoatl)"
        )),
        "{refused}"
    );
    // Applied again unchanged, the model is observed at the endpoint
    // configured now, and the next turn runs.
    let loaded = loads(&server).await;
    let preview = apply_unchanged(&daemon, &session.id, "endpoint-changed").await;
    assert_eq!(
        reviewed(&preview),
        (("replaced", "new conversation"), vec![endpoint.as_str()])
    );
    assert!(loads(&server).await > loaded, "observed again");
    completes(&daemon, &session.id).await;

    // The model was pulled again (another digest): the turn says so.
    server.reset().await;
    serve_audited_ollama(&server, &[MODEL], 1).await;
    answer_chats(&server, "Done.").await;
    let pulled = format!(
        "{MODEL} was pulled again since this Team was reviewed (digest 000000000000, now \
         010000000000)"
    );
    let refused = send(&daemon, &session.id)
        .await
        .expect_err("the model was pulled again")
        .to_string();
    assert!(
        refused.contains(&format!(
            "{pulled}; review Team & budget again: applying the Team observes the model again"
        )),
        "{refused}"
    );
    let loaded = loads(&server).await;
    let preview = apply_unchanged(&daemon, &session.id, "model-pulled-again").await;
    assert_eq!(
        reviewed(&preview),
        (("replaced", "new conversation"), vec![pulled.as_str()])
    );
    assert!(loads(&server).await > loaded, "observed again");
    completes(&daemon, &session.id).await;

    // Applied once more, the new profile is kept: nothing is loaded.
    let loaded = loads(&server).await;
    let preview = apply_unchanged(&daemon, &session.id, "kept").await;
    assert_eq!(reviewed(&preview), (("retained", "preserved"), vec![]));
    assert_eq!(loads(&server).await, loaded);
    completes(&daemon, &session.id).await;
    daemon.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_changed_endpoint_or_a_model_pulled_again_is_observed_again_on_apply() {
    if let Some(phase) = std::env::var_os(CHILD) {
        phase_body(phase.to_str().unwrap()).await;
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
    let ids = root.join("session");
    let envs = [
        ("AXOCOATL_RETAINED_PROFILE_REPO", repo.as_os_str()),
        ("AXOCOATL_RETAINED_PROFILE_IDS", ids.as_os_str()),
    ];
    succeeded(&run_child(TEST, CHILD, "first", &root, &data, &envs).await);
    succeeded(&run_child(TEST, CHILD, "restarted", &root, &data, &envs).await);
}

/// `server`'s OpenRouter: the catalog row of [`OPENROUTER_MODEL`], its one
/// endpoint at `prompt` dollars per input token, and inference that answers
/// every request "Done.".
async fn serve_openrouter(server: &MockServer, prompt: &str) {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"data": [{
                "id": OPENROUTER_MODEL,
                "architecture": {"input_modalities": ["text"], "output_modalities": ["text"]},
                "supported_parameters": ["max_tokens", "tools"]
            }]})),
        )
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/models/{OPENROUTER_MODEL}/endpoints")))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"data": {
                "id": OPENROUTER_MODEL,
                "endpoints": [{
                    "model_id": OPENROUTER_MODEL, "provider_name": "FiniteProvider",
                    "tag": "finite/exact", "context_length": 32768, "max_completion_tokens": 4096,
                    "supported_parameters": ["max_tokens", "tools"],
                    "pricing": {"prompt": prompt, "completion": "0.000002"},
                    "status": 0, "supports_implicit_caching": false
                }]
            }})),
        )
        .mount(server)
        .await;
    let delta = serde_json::json!({"id": "gen-fixture", "model": OPENROUTER_MODEL,
        "provider": "FiniteProvider", "choices": [{"index": 0,
        "delta": {"content": "Done."}, "finish_reason": "stop"}]});
    let usage = serde_json::json!({"id": "gen-fixture", "model": OPENROUTER_MODEL,
        "provider": "FiniteProvider", "choices": [], "usage": {"prompt_tokens": 40,
        "completion_tokens": 2, "total_tokens": 42, "cost": 0.000044, "is_byok": false}});
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            format!("data: {delta}\n\ndata: {usage}\n\ndata: [DONE]\n\n"),
            "text/event-stream",
        ))
        .mount(server)
        .await;
}

/// How many inference requests `server` received.
async fn inferences(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.url.path() == "/chat/completions")
        .count()
}

async fn openrouter_body() {
    log_warnings_to_stderr();
    // A stand-in for OpenRouter, where the daemon observes its endpoints.
    let server = MockServer::start().await;
    serve_openrouter(&server, "0.000001").await;
    crate::session_dispatch::use_openrouter_stand_in(server.uri());
    let config = format!(
        r#"
agents:
  - id: lead
    name: "Lead"
    provider: openrouter
    model: "{OPENROUTER_MODEL}"
    role: autonomous
    tools: []
    sampling:
      max_tokens: 1024
providers:
  openrouter_billing: credits
  openrouter:
    api_key: fixture-openrouter-key
sandbox:
  backend: podman
  network: bridge
consolidation:
  enabled: false
"#
    );
    let config =
        axocoatl_config::parse_config(&config, std::path::Path::new("openrouter.yaml")).unwrap();
    let repo =
        std::path::PathBuf::from(std::env::var_os("AXOCOATL_RETAINED_PROFILE_REPO").unwrap());
    let daemon = AxocoatlDaemon::bootstrap_headless(config).await.unwrap();
    let session = native_session_with_team(&daemon, &repo, "OpenRouter", "first-team").await;
    ready(&daemon, &session).await;
    completes(&daemon, &session.id).await;

    // OpenRouter's endpoint now lists another input price: the turn says
    // so, and nothing is sent to the model.
    server.reset().await;
    serve_openrouter(&server, "0.000002").await;
    let changed = format!(
        "OpenRouter lists other terms for {OPENROUTER_MODEL}'s endpoint finite/exact than when \
         this Team was reviewed (input price $1/M, now $2/M)"
    );
    let refused = send(&daemon, &session.id)
        .await
        .expect_err("the endpoint's terms changed")
        .to_string();
    assert!(
        refused.contains(&format!(
            "{changed}; review Team & budget again: applying the Team observes \
             {OPENROUTER_MODEL}'s endpoints again"
        )),
        "{refused}"
    );
    assert_eq!(
        inferences(&server).await,
        0,
        "nothing was sent to the model"
    );
    // Applied again unchanged, the endpoints are observed again, Preview
    // says why, and the next turn runs at the price listed now.
    let preview = apply_unchanged(&daemon, &session.id, "endpoint-terms-changed").await;
    assert_eq!(
        reviewed(&preview),
        (("replaced", "new conversation"), vec![changed.as_str()])
    );
    completes(&daemon, &session.id).await;
    assert_eq!(inferences(&server).await, 1);
    // Applied once more, the new profile is kept.
    let preview = apply_unchanged(&daemon, &session.id, "kept").await;
    assert_eq!(reviewed(&preview), (("retained", "preserved"), vec![]));
    completes(&daemon, &session.id).await;
    daemon.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_openrouter_endpoint_whose_terms_changed_is_observed_again_on_apply() {
    if std::env::var_os(CHILD).is_some() {
        openrouter_body().await;
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
    let envs = [("AXOCOATL_RETAINED_PROFILE_REPO", repo.as_os_str())];
    succeeded(&run_child(OPENROUTER_TEST, CHILD, "openrouter", &root, &data, &envs).await);
}
