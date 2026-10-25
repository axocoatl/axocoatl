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
use super::native_session_test_support::{native_session_with_team, ready, send};
use super::*;
use crate::child_daemon_test_support::{
    answer_chats, audited_ollama, log_warnings_to_stderr, run_child, succeeded,
};
use crate::loadout::api::RunRequest;
use axocoatl_session::session_history::{HistoryVisibility, SessionHistoryEntry};
use axocoatl_session::turn_contract::LogicalTurnState;
use std::os::unix::fs::PermissionsExt;

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

async fn phase_body(phase: &str) {
    // The parent reads what the daemon logs.
    log_warnings_to_stderr();
    // Each phase starts its own Ollama, on another port: the restarted
    // daemon's configured endpoint is not the one the Team was reviewed on.
    let server = audited_ollama(&[MODEL]).await;
    answer_chats(&server, "Done.").await;
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
        let session =
            native_session_with_team(&daemon, &repo, "Recovered turn", "recovered-turn-team").await;
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
                .contains("providers.ollama.base_url names"),
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
