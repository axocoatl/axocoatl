//! External writers (Claude Code, Codex) through the daemon's own turn path:
//! a loadout Session's Send, `prepare_native_turn`, Begin and the turn's
//! driver, as `axocoatl run` reaches them. Owner: workstream `agents`.
//!
//! Each body runs in a child process with its own data root (bootstrap reads
//! the process environment). The first test uses a fake Podman and a fake
//! Ready runtime, so it runs everywhere; the Podman tests are ignored by
//! default and run the pinned programs from their recipe images:
//!
//! ```text
//! CONTAINER_CONNECTION=axocoatl-ci-pr74 cargo test -p axocoatl-daemon --lib \
//!   external_agent_host::turn_tests -- --ignored
//! ```
use super::*;
use crate::bootstrap::native_send::NativeSessionSend;
use crate::bootstrap::native_turn::NativeFirstTurnStart;
use axocoatl_config::loadout::{
    parse_loadout, resolve_loadout, AgentRuntime, LoadoutSource, ParamValues,
};
use axocoatl_session::execution_content::ContentResolution;
use axocoatl_session::execution_ownership::DataRootFormatOwnership;
use axocoatl_session::run_outcome::LoadoutRef;
use axocoatl_session::run_record::SessionLoadoutBinding;
use axocoatl_session::session_history::SessionHistoryEntry;
use std::os::unix::fs::PermissionsExt;

const CHILD: &str = "AXOCOATL_EXTERNAL_TURN_TEST_CHILD";
const MODULE: &str = "bootstrap::external_agent_host::turn_tests";

/// A loadout whose only Agent is `runtime`'s writer, with one required
/// check and no review.
fn external_loadout(runtime: AgentRuntime) -> String {
    let (name, recipe) = match runtime {
        AgentRuntime::ClaudeCode => ("claude-code", "claude-code"),
        AgentRuntime::Codex => ("codex", "codex"),
        AgentRuntime::Native => unreachable!("an external runtime"),
    };
    format!(
        r#"
schema: axocoatl.loadout/1
id: external-{name}
version: 1
name: External {name}
kind: custom
params:
  writer_model: {{ kind: model, required: true, description: the program's model }}
agents:
  - id: writer
    role: writer
    runtime: {name}
    model: {{ param: writer_model }}
checks:
  - {{ name: fixed, run: {{ argv: [sh, -c, "test -f fixed.txt"] }}, timeout: 2m }}
budgets:
  agent: {{ activations: 1, invocations: 20, tokens: 400000, cost_usd: 1 }}
  wall_clock: 10m
prompt: "{{task}}"
environment: {{ recipes: [{recipe}] }}
"#
    )
}

/// `provider:model` for `runtime`'s writer.
fn writer_model(runtime: AgentRuntime) -> &'static str {
    match runtime {
        AgentRuntime::ClaudeCode => "anthropic:claude-haiku-4-5",
        AgentRuntime::Codex => "openai:gpt-5.5",
        AgentRuntime::Native => unreachable!("an external runtime"),
    }
}

/// The program's model name in [`writer_model`].
fn program_model(runtime: AgentRuntime) -> &'static str {
    writer_model(runtime).split_once(':').unwrap().1
}

/// Run `name` (a test of this module) in a child process with its own data
/// root; `podman` is a directory holding a fake `podman` to put first on
/// `PATH`, or `None` for the real one.
async fn run_child(name: &str, podman: Option<&std::path::Path>, ignored: bool) {
    let root = tempfile::tempdir().unwrap();
    let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", &format!("{MODULE}::{name}"), "--nocapture"])
        .env(CHILD, "1")
        .env("AXOCOATL_DATA_DIR", root.path().join("data"))
        .env("AXOCOATL_SOCKET_PATH", root.path().join("ipc/daemon.sock"))
        .current_dir(root.path())
        .kill_on_drop(true);
    if ignored {
        command.arg("--ignored");
    }
    if let Some(bin) = podman {
        command.env("PATH", format!("{}:/usr/bin:/bin", bin.display()));
    }
    let result = tokio::time::timeout(Duration::from_secs(900), command.output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        result.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

/// A directory with a fake rootless `podman` that answers what bootstrap
/// and shutdown ask and refuses everything else.
fn fake_podman(root: &std::path::Path) -> std::path::PathBuf {
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
  'rm '*|'volume rm '*|'network rm '*) ;;
  *) printf 'unexpected Podman command: %s\n' "$*" >&2; exit 1 ;;
esac
"#,
    )
    .unwrap();
    std::fs::set_permissions(&podman, std::fs::Permissions::from_mode(0o700)).unwrap();
    bin
}

/// A native Session in `work` bound to a loadout run under egress, as
/// `create_loadout_session` makes it.
async fn bound_session(daemon: &AxocoatlDaemon, work: &std::path::Path) -> Session {
    let workspace = daemon
        .workspace_store
        .lock()
        .await
        .register(work, Some("External turn"))
        .unwrap();
    let DataRootFormatOwnership::Upgraded(ownership) = &daemon._data_dir_lease.ownership else {
        panic!("fresh data roots use native ownership");
    };
    let mut sessions = daemon.session_store.lock().await;
    let (session, receipt) = sessions
        .create_native_with_environment(
            ownership,
            "External turn",
            &workspace.id,
            &workspace.canonical_path,
            SessionMode::Custom { agents: vec![] },
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
    sessions
        .bind_loadout(
            &session.id,
            SessionLoadoutBinding {
                run_id: format!("run-{}", uuid::Uuid::new_v4()),
                loadout: LoadoutRef {
                    id: "external".into(),
                    version: 1,
                    kind: "custom".into(),
                    digest: "0".repeat(64),
                    builtin: false,
                },
                network: "egress".into(),
                workload: "hardened".into(),
            },
        )
        .unwrap()
}

/// A Ready runtime that admits ownership and Begin and has no egress
/// decision point, so an external program never starts in it.
struct AdmissionOnlySandbox(std::path::PathBuf);

#[async_trait::async_trait]
impl Sandbox for AdmissionOnlySandbox {
    fn root(&self) -> &std::path::Path {
        &self.0
    }
    fn execution_identity(&self) -> Option<&str> {
        Some("external-turn-admission")
    }
    async fn exec(
        &self,
        _argv: &[&str],
        _timeout: Duration,
    ) -> Result<ExecResult, axocoatl_isolation::IsolationError> {
        Err(axocoatl_isolation::IsolationError::Io(
            std::io::Error::other("the admission test runtime runs no command"),
        ))
    }
    async fn exec_stdin(
        &self,
        _argv: &[&str],
        _stdin: &str,
        _timeout: Duration,
    ) -> Result<ExecResult, axocoatl_isolation::IsolationError> {
        Err(axocoatl_isolation::IsolationError::Io(
            std::io::Error::other("the admission test runtime runs no command"),
        ))
    }
    fn spawn_background(&self, _command: &str) -> String {
        unreachable!("the admission test runtime runs no background task")
    }
    fn spawn_pty(
        &self,
        _command: &str,
        _rows: u16,
        _cols: u16,
    ) -> Result<Arc<axocoatl_isolation::pty::PtyTerminal>, String> {
        Err("the admission test runtime has no terminal".into())
    }
    fn get_terminal(&self, _id: &str) -> Option<Arc<axocoatl_isolation::pty::PtyTerminal>> {
        None
    }
    fn kill_terminal(&self, _id: &str) -> bool {
        false
    }
    fn list_terminals(&self) -> Vec<(String, String, bool)> {
        Vec::new()
    }
    fn list_tasks(&self) -> Vec<axocoatl_isolation::session_sandbox::BgTask> {
        Vec::new()
    }
    fn with_root(&self, root: &std::path::Path) -> Arc<dyn Sandbox> {
        Arc::new(Self(root.to_path_buf()))
    }
    async fn stop(&self) {}
}

async fn admission_child_body() {
    let mut config = axocoatl_config::AxocoatlConfig::default();
    config.agents.clear();
    config.consolidation.enabled = false;
    let daemon = AxocoatlDaemon::bootstrap_headless(config).await.unwrap();
    // The Workspaces outlive the daemon's shutdown, which reads them.
    let mut workspaces = Vec::new();
    for runtime in [AgentRuntime::ClaudeCode, AgentRuntime::Codex] {
        let work = tempfile::tempdir().unwrap();
        let session = bound_session(&daemon, work.path()).await;
        workspaces.push(work);
        // The loadout's own team, as the run applies it.
        let loadout = parse_loadout(&external_loadout(runtime), LoadoutSource::Builtin).unwrap();
        let mut params = ParamValues::new();
        params.insert("writer_model".into(), writer_model(runtime).into());
        let resolved = resolve_loadout(&loadout, &params, "Fix it", "/repo").unwrap();
        let edit = crate::loadout::team_plan::team_edit(
            &resolved,
            &crate::loadout::team_plan::default_slots(&resolved).unwrap(),
            true,
            0,
        )
        .unwrap();
        daemon.apply_loadout_team(&session.id, edit).await.unwrap();
        let team = daemon.session_team(&session.id).await.unwrap();
        assert_eq!(
            team.slots[0].provider,
            crate::external_agent::runtime_provider(runtime).unwrap()
        );
        assert_eq!(team.slots[0].model, program_model(runtime));
        // A Ready runtime whose ownership admission passes.
        daemon
            .session_store
            .lock()
            .await
            .set_environment(
                &session.id,
                SessionEnvironmentState::Ready,
                Some("external-turn-test".into()),
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
        // The Send the run makes, through the daemon's own preparation and
        // Begin: before the fix, admission refused every external writer
        // ("native factory requires an exact configured bounded Ollama or
        // OpenRouter actor").
        let send = NativeSessionSend {
            session_id: session.id.clone(),
            turn_id: format!("turn-{}", uuid::Uuid::new_v4()),
            idempotency_key: None,
            display_input: None,
            input: "Fix it".into(),
            reference_ids: Vec::new(),
            context_references: Vec::new(),
            model_override: None,
            target_agent: None,
        };
        let request = daemon.prepare_native_send_request(&send).await.unwrap();
        let start = match daemon.prepare_native_turn(request).await {
            Ok(start) => start,
            Err(error) => panic!("{runtime:?}: the turn did not start: {error}"),
        };
        let NativeFirstTurnStart::Prepared(prepared) = start else {
            panic!("{runtime:?}: a new turn is prepared, not reattached");
        };
        let controller = prepared.controller();
        // The driver runs the writer's activation. Its resources come from
        // the external factory (no native provider observation exists for
        // it), and its one model call is the program run, which this
        // runtime refuses: it has no egress decision point.
        let _ = tokio::time::timeout(Duration::from_secs(60), prepared.run())
            .await
            .expect("the turn ends");
        let history = controller.history_snapshot().unwrap();
        let Some(SessionHistoryEntry::ExecutionV2(turn)) = history.get(&send.turn_id) else {
            panic!("{runtime:?}: the admitted turn is in the Session history");
        };
        assert_eq!(turn.activations.len(), 1, "{runtime:?}: {turn:#?}");
        let writer = &turn.activations[0];
        assert!(
            matches!(&writer.definition_name, ContentResolution::Available { .. }),
            "{runtime:?}: {:?}",
            writer.definition_name
        );
        let recorded = serde_json::to_string(writer).unwrap();
        assert!(
            !recorded.contains("native factory"),
            "{runtime:?}: {recorded}"
        );
        assert!(
            recorded.contains("an external agent runs only in a Session under network: egress"),
            "{runtime:?}: the writer's model call reached the external program port: {recorded}"
        );
        daemon.session_sandboxes.lock().await.remove(&session.id);
    }
    daemon.shutdown().await.unwrap();
}

/// A claude-code and a codex writer each start a turn through the real
/// Send path: `prepare_native_send_request`, `prepare_native_turn` (whose
/// admission refused every external writer), Begin and the turn's driver,
/// which resolves the writer's activation through the external factory and
/// runs its model call as the program run.
#[tokio::test]
async fn external_writers_start_turns_through_the_native_turn_path() {
    if std::env::var_os(CHILD).is_some() {
        admission_child_body().await;
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let bin = fake_podman(root.path());
    run_child(
        "external_writers_start_turns_through_the_native_turn_path",
        Some(&bin),
        false,
    )
    .await;
}
