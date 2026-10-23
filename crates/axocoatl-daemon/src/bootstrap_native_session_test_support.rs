//! A native workbench Session for daemon tests that run turns without a
//! container: made on the configured `lead`, its Team applied with the
//! suggested budget, and a Ready runtime that admits work and runs nothing
//! ([`AdmissionOnlySandbox`]). Turns are sent as the workbench and IPC send
//! them.
use super::*;
use crate::child_daemon_test_support::AdmissionOnlySandbox;
use axocoatl_session::control_authority::GrantLimits;
use axocoatl_session::execution_ownership::DataRootFormatOwnership;

/// A native Session on `lead` in the Workspace at `repo`, with its Team
/// applied (the suggested limits at no cost, expiring in the suggested
/// lifetime) as `command`.
pub(super) async fn native_session_with_team(
    daemon: &AxocoatlDaemon,
    repo: &std::path::Path,
    name: &str,
    command: &str,
) -> Session {
    let workspace = daemon
        .workspace_store
        .lock()
        .await
        .register(repo, Some(name))
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
            name,
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
        command_id: command.into(),
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
    session
}

/// The Session's Ready runtime, as a started container makes it, which
/// admits work and runs nothing.
pub(super) async fn ready(daemon: &AxocoatlDaemon, session: &Session) {
    daemon
        .session_store
        .lock()
        .await
        .set_environment(
            &session.id,
            SessionEnvironmentState::Ready,
            Some("native-session-test".into()),
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
pub(super) async fn send(
    daemon: &AxocoatlDaemon,
    session: &str,
) -> Result<AgentRunOutcome, DaemonError> {
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
