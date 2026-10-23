//! After a restart, a Session whose last turn had ended attaches that turn
//! again when its next request arrives, with a repository owner that holds
//! the Session's Workspace. When that request's turn then cannot start (here
//! the configured Ollama endpoint changed, as for any provider that refuses),
//! the Session is between turns all the same: its Workspace is free for a
//! run or another Session. It used to stay held until the Session closed or
//! the daemon restarted, and every `axocoatl run` on the repository failed as
//! busy (exit 7).
//!
//! Each daemon runs in a child process (bootstrap reads `AXOCOATL_DATA_DIR`
//! from the process environment) with a fake Podman, and the Session's Ready
//! runtime runs nothing, so no container starts.
use super::*;
use crate::child_daemon_test_support::{
    audited_ollama, log_warnings_to_stderr, run_child, succeeded, AdmissionOnlySandbox,
};
use crate::loadout::api::RunRequest;
use axocoatl_session::control_authority::GrantLimits;
use axocoatl_session::execution_ownership::DataRootFormatOwnership;
use axocoatl_session::session_history::{HistoryVisibility, SessionHistoryEntry};
use axocoatl_session::turn_contract::LogicalTurnState;
use std::os::unix::fs::PermissionsExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

const CHILD: &str = "AXOCOATL_RECOVERED_TURN_TEST_CHILD";
const TEST: &str = "bootstrap::recovered_turn_tests::after_a_restart_a_turn_that_cannot_start_leaves_the_workspace_free";
const MODEL: &str = "restart-model:latest";

/// The loadout of the run admitted on the repository after the refusal.
const LOADOUT: &str = r#"
schema: axocoatl.loadout/1
id: restart
version: 1
name: Restart
kind: custom
params:
  writer_model: { kind: model, required: true }
agents:
  - id: writer
    role: writer
    model: { param: writer_model }
    tools: [read_file]
budgets:
  agent: { activations: 2, invocations: 20, tokens: 100000, cost_usd: 1 }
  wall_clock: 10m
prompt: "{task}"
"#;

/// The Session's Ready runtime, as a started container makes it.
async fn ready(daemon: &AxocoatlDaemon, session: &Session) {
    daemon
        .session_store
        .lock()
        .await
        .set_environment(
            &session.id,
            SessionEnvironmentState::Ready,
            Some("recovered-turn-test".into()),
            Some(SessionRuntimeIdentity {
                backend: "podman".into(),
                id: session.id.clone(),
                remote_root: None,
                control_plane: None,
                data_plane_domain: None,
                authority_fingerprint: None,
                ownership_token: None,
                cleanup_confirmed: false,
            }),
            vec![],
            None,
        )
        .unwrap();
    daemon.session_sandboxes.lock().await.insert(
        session.id.clone(),
        Arc::new(AdmissionOnlySandbox(session.working_dir.clone())),
    );
}

/// A request to `session`, as the workbench and IPC send one.
async fn send(daemon: &AxocoatlDaemon, session: &str) -> Result<AgentRunOutcome, DaemonError> {
    let (sink, receiver) = tokio::sync::mpsc::unbounded_channel();
    drop(receiver);
    let turn = format!("turn-{}", uuid::Uuid::new_v4());
    daemon
        .execute_session_turn_streaming(
            session,
            &turn,
            Some(turn.clone()),
            None,
            "Say done.",
            Vec::new(),
            Vec::new(),
            None,
            None,
            sink,
        )
        .await
}

async fn phase_body(phase: &str) {
    // The parent reads what the daemon logs.
    log_warnings_to_stderr();
    // Each phase starts its own Ollama, on another port: the restarted
    // daemon's configured endpoint is not the one the Team was reviewed on.
    let server = audited_ollama(&[MODEL]).await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(|request: &wiremock::Request| {
            let body: serde_json::Value = request.body_json().unwrap_or_default();
            let record = serde_json::json!({"model": body["model"],
                "created_at": "2026-10-23T00:00:00Z",
                "message": {"role": "assistant", "content": "Done."}, "done": true,
                "done_reason": "stop", "prompt_eval_count": 20, "eval_count": 2});
            ResponseTemplate::new(200).set_body_raw(format!("{record}\n"), "application/x-ndjson")
        })
        .mount(&server)
        .await;
    let config_dir = tempfile::tempdir().unwrap();
    let config_path = config_dir.path().join("axocoatl.yaml");
    std::fs::write(
        &config_path,
        format!(
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
        ),
    )
    .unwrap();
    std::fs::create_dir(config_dir.path().join("loadouts")).unwrap();
    std::fs::write(config_dir.path().join("loadouts/restart.yaml"), LOADOUT).unwrap();
    let config = axocoatl_config::load_config(&config_path).await.unwrap();
    let repo = std::path::PathBuf::from(std::env::var_os("AXOCOATL_RECOVERED_TURN_REPO").unwrap());
    let ids = std::path::PathBuf::from(std::env::var_os("AXOCOATL_RECOVERED_TURN_IDS").unwrap());
    let daemon = AxocoatlDaemon::bootstrap_headless(config).await.unwrap();
    daemon.set_config_path(&config_path);
    if phase == "first" {
        // A workbench Session on Lead, its Team applied, and one turn that
        // completes: the Session is between turns when the daemon stops.
        let workspace = daemon
            .workspace_store
            .lock()
            .await
            .register(&repo, Some("Recovered turn"))
            .unwrap();
        let DataRootFormatOwnership::Upgraded(ownership) = &daemon._data_dir_lease.ownership else {
            panic!("fresh data roots use native ownership");
        };
        let (session, receipt) = daemon
            .session_store
            .lock()
            .await
            .create_native_with_environment(
                ownership,
                "Recovered turn",
                &workspace.id,
                &workspace.canonical_path,
                SessionMode::SingleAgent {
                    agent_id: "lead".into(),
                },
                vec![],
                vec![],
                None,
                None,
                false,
                true,
            )
            .unwrap();
        daemon
            .session_dispatch_lifecycles
            .retain_native_session(ownership.clone(), receipt)
            .unwrap();
        let view = daemon.session_team(&session.id).await.unwrap();
        let budget = view.suggested_budget.clone().unwrap();
        let expires_at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            + budget.expires_in_ms;
        let mut slots = view.slots.clone();
        for slot in &mut slots {
            slot.limits = Some(GrantLimits {
                cost_microunits: 0,
                ..budget.agent.clone()
            });
            slot.expires_at_ms = Some(expires_at_ms);
        }
        let edit = session_team::SessionTeamEdit {
            command_id: "recovered-turn-team".into(),
            expected_configuration_revision: view.configuration_revision,
            slots,
            dependencies: view.dependencies.clone(),
            layout: view.layout.clone(),
            required_checks: vec![],
            required_review: None,
            check_options: vec![],
        };
        let preview = daemon
            .preview_session_team(&session.id, edit.clone())
            .await
            .unwrap_or_else(|error| panic!("{error}"));
        daemon
            .apply_session_team(
                &session.id,
                session_team::SessionTeamApply {
                    edit,
                    review_digest: preview.review_digest,
                },
            )
            .await
            .unwrap_or_else(|error| panic!("{error}"));
        ready(&daemon, &session).await;
        send(&daemon, &session.id)
            .await
            .unwrap_or_else(|error| panic!("the first turn: {error}"));
        let history = daemon
            .versioned_session_history_snapshot(&session.id)
            .await
            .unwrap();
        let turns = history.entries(HistoryVisibility::Visible);
        let [SessionHistoryEntry::ExecutionV2(turn)] = turns.as_slice() else {
            panic!("one native turn: {turns:?}");
        };
        assert_eq!(turn.state, LogicalTurnState::Completed);
        std::fs::write(&ids, &session.id).unwrap();
    } else {
        // Restarted: the Session's next request attaches its ended turn and
        // then cannot start its own.
        let session_id = std::fs::read_to_string(&ids).unwrap();
        let session = daemon.get_session(&session_id).await.unwrap();
        ready(&daemon, &session).await;
        let refused = send(&daemon, &session_id)
            .await
            .expect_err("the turn cannot start on another Ollama endpoint");
        assert!(
            refused
                .to_string()
                .contains("configured Ollama endpoint differs from the admitted native profile"),
            "{refused}"
        );
        // The Workspace is free: a run on the repository is admitted.
        match daemon
            .admit_loadout_run(RunRequest {
                loadout: "restart".into(),
                task: "Say done.".into(),
                repo: repo.display().to_string(),
                params: [("writer_model".to_string(), format!("ollama:{MODEL}"))]
                    .into_iter()
                    .collect(),
                keep: Default::default(),
                check_command: None,
                setup_command: None,
                environment: Default::default(),
                request_id: "run-after-refusal".into(),
            })
            .await
        {
            Ok((accepted, _)) => assert_eq!(accepted.workspace_id, session.workspace_id),
            Err(error) => panic!("the run was refused: {error}"),
        }
    }
    daemon.shutdown().await.unwrap();
}

#[tokio::test]
async fn after_a_restart_a_turn_that_cannot_start_leaves_the_workspace_free() {
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
    let ids = root.join("session-id");
    let envs = [
        ("AXOCOATL_RECOVERED_TURN_REPO", repo.as_os_str()),
        ("AXOCOATL_RECOVERED_TURN_IDS", ids.as_os_str()),
    ];
    succeeded(&run_child(TEST, CHILD, "first", &root, &data, &envs).await);
    succeeded(&run_child(TEST, CHILD, "restarted", &root, &data, &envs).await);
}
