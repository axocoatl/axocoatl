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
use std::sync::atomic::{AtomicUsize, Ordering};

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

/// A fix loadout whose writer is `runtime`'s program, reviewed by a native
/// Ollama model for up to two rounds.
fn external_fix_loadout(runtime: AgentRuntime) -> String {
    external_loadout(runtime)
        .replace("id: external-", "id: external-fix-")
        .replace("kind: custom", "kind: fix")
        .replace(
            "  writer_model:",
            "  reviewer_model: { kind: model, required: true, description: the reviewer }\n  writer_model:",
        )
        .replace(
            "budgets:\n  agent: { activations: 1, invocations: 20,",
            "review:\n  model: { param: reviewer_model }\n  rounds: 2\n  tools: [read_file, list_dir, grep, glob]\n\
             budgets:\n  reviewer: { activations: 2, invocations: 20, tokens: 400000, cost_usd: 1 }\n\
             \x20 agent: { activations: 3, invocations: 40,",
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

// ------------------------------------------------------------------ Podman

/// What the program's tool call runs in the checkout: the change the run
/// checks for, then, for the test, every environment it can read (its own
/// processes', the program's among them), its home directory's listing, and
/// who it runs as.
const PROGRAM_COMMAND: &str = "printf 'fixed\\n' > fixed.txt && \
(for f in /proc/[0-9]*/environ; do tr '\\0' '\\n' < \"$f\"; echo; done) \
> /tmp/axocoatl-program-env.txt 2>/dev/null; ls -a \"$HOME\" > /tmp/axocoatl-program-home.txt 2>&1; \
(id -u; grep -E '^(NoNewPrivs|CapEff):' /proc/self/status) > /tmp/axocoatl-program-identity.txt 2>&1; \
cat fixed.txt";

/// One request the fake model API received.
#[derive(Debug, Clone)]
struct ApiRequest {
    method: String,
    /// Path and query, as sent.
    target: String,
    authorization: Vec<String>,
    body: serde_json::Value,
}

/// A local HTTPS model API for `api.anthropic.com` or `api.openai.com`
/// with its own authority. It plays the model for one tool round: a request
/// that offers tools and holds no tool result gets one shell call
/// ([`PROGRAM_COMMAND`]); a request with the call's result gets the final
/// text; any other request gets a short text.
struct FakeModelApi {
    port: u16,
    ca: crate::egress_broker::SessionCa,
    seen: Arc<StdMutex<Vec<ApiRequest>>>,
}

const CLAUDE_ANSWER: &str = "FIXED-BY-CLAUDE-CODE";
const CODEX_ANSWER: &str = "FIXED-BY-CODEX";

/// The program's final text: `answer`, and when the request carries review
/// findings, the writer's answer to them.
fn final_answer(answer: &str, body: &serde_json::Value) -> String {
    if body.to_string().contains("ADJUDICATIONS") {
        format!(
            "{answer}\n\nADJUDICATIONS\n```json\n[{{\"id\":\"F1\",\"decision\":\"accept\",\
             \"reason\":\"fixed.txt now ends with a line break\"}}]\n```"
        )
    } else {
        answer.to_string()
    }
}

fn sse(events: &[serde_json::Value]) -> String {
    events
        .iter()
        .map(|event| {
            format!(
                "event: {}\ndata: {event}\n\n",
                event["type"].as_str().unwrap()
            )
        })
        .collect()
}

/// The Messages API's answer to `body`: streamed when it asks for a stream.
fn anthropic_answer(body: &serde_json::Value) -> (String, &'static str) {
    let messages = body["messages"].as_array().cloned().unwrap_or_default();
    let has_result = messages.iter().any(|message| {
        message["content"]
            .as_array()
            .is_some_and(|blocks| blocks.iter().any(|block| block["type"] == "tool_result"))
    });
    let offers_bash = body["tools"]
        .as_array()
        .is_some_and(|tools| tools.iter().any(|tool| tool["name"] == "Bash"));
    let model = body["model"].as_str().unwrap_or("claude").to_string();
    let (block, stop) = if offers_bash && !has_result {
        (
            serde_json::json!({"type": "tool_use", "id": "toolu_axocoatl_1", "name": "Bash",
                "input": {"command": PROGRAM_COMMAND, "description": "Fix the file"}}),
            "tool_use",
        )
    } else if has_result {
        (
            serde_json::json!({"type": "text", "text": final_answer(CLAUDE_ANSWER, body)}),
            "end_turn",
        )
    } else {
        (
            serde_json::json!({"type": "text", "text": "ok"}),
            "end_turn",
        )
    };
    let usage = serde_json::json!({"input_tokens": 120, "output_tokens": 7,
        "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0});
    if body["stream"] != true {
        let message = serde_json::json!({"id": "msg_axocoatl", "type": "message",
            "role": "assistant", "model": model, "content": [block], "stop_reason": stop,
            "stop_sequence": null, "usage": usage});
        return (message.to_string(), "application/json");
    }
    let (start, delta) = if block["type"] == "tool_use" {
        (
            serde_json::json!({"type": "tool_use", "id": block["id"], "name": "Bash", "input": {}}),
            serde_json::json!({"type": "input_json_delta",
                "partial_json": block["input"].to_string()}),
        )
    } else {
        (
            serde_json::json!({"type": "text", "text": ""}),
            serde_json::json!({"type": "text_delta", "text": block["text"]}),
        )
    };
    let events = [
        serde_json::json!({"type": "message_start", "message": {"id": "msg_axocoatl",
            "type": "message", "role": "assistant", "model": model, "content": [],
            "stop_reason": null, "stop_sequence": null, "usage": usage}}),
        serde_json::json!({"type": "content_block_start", "index": 0, "content_block": start}),
        serde_json::json!({"type": "content_block_delta", "index": 0, "delta": delta}),
        serde_json::json!({"type": "content_block_stop", "index": 0}),
        serde_json::json!({"type": "message_delta", "delta": {"stop_reason": stop,
            "stop_sequence": null}, "usage": {"output_tokens": 7}}),
        serde_json::json!({"type": "message_stop"}),
    ];
    (sse(&events), "text/event-stream")
}

/// The shell tool Codex offers, and the arguments that run `command` in it.
fn codex_shell_call(body: &serde_json::Value, command: &str) -> Option<(String, String)> {
    let tools = body["tools"].as_array()?;
    let named = |name: &str| {
        tools
            .iter()
            .any(|tool| tool["name"] == name || tool["type"] == name)
    };
    if named("shell") {
        Some((
            "shell".into(),
            serde_json::json!({"command": ["bash", "-lc", command]}).to_string(),
        ))
    } else if named("shell_command") {
        Some((
            "shell_command".into(),
            serde_json::json!({"command": command}).to_string(),
        ))
    } else if named("exec_command") {
        Some((
            "exec_command".into(),
            serde_json::json!({"cmd": command}).to_string(),
        ))
    } else {
        None
    }
}

/// The Responses API's streamed answer to `body`.
fn openai_answer(body: &serde_json::Value) -> (String, &'static str) {
    let input = body["input"].as_array().cloned().unwrap_or_default();
    let has_result = input.iter().any(|item| {
        item["type"]
            .as_str()
            .is_some_and(|kind| kind.ends_with("call_output"))
    });
    let model = body["model"].as_str().unwrap_or("gpt").to_string();
    let call = (!has_result)
        .then(|| codex_shell_call(body, PROGRAM_COMMAND))
        .flatten();
    let item = match &call {
        Some((name, arguments)) => serde_json::json!({"type": "function_call", "id": "fc_axocoatl",
            "call_id": "call_axocoatl_1", "name": name, "arguments": arguments,
            "status": "completed"}),
        None => serde_json::json!({"type": "message", "id": "msg_axocoatl", "role": "assistant",
            "status": "completed", "content": [{"type": "output_text",
            "text": if has_result { final_answer(CODEX_ANSWER, body) } else { "ok".into() },
            "annotations": []}]}),
    };
    let response = |status: &str, output: serde_json::Value| {
        serde_json::json!({"id": "resp_axocoatl", "object": "response", "created_at": 1,
            "model": model, "status": status, "output": output,
            "usage": {"input_tokens": 300, "input_tokens_details": {"cached_tokens": 0},
                "output_tokens": 9, "output_tokens_details": {"reasoning_tokens": 0},
                "total_tokens": 309}})
    };
    let events = [
        serde_json::json!({"type": "response.created", "sequence_number": 0,
            "response": response("in_progress", serde_json::json!([]))}),
        serde_json::json!({"type": "response.output_item.added", "sequence_number": 1,
            "output_index": 0, "item": item}),
        serde_json::json!({"type": "response.output_item.done", "sequence_number": 2,
            "output_index": 0, "item": item}),
        serde_json::json!({"type": "response.completed", "sequence_number": 3,
            "response": response("completed", serde_json::json!([item]))}),
    ];
    (sse(&events), "text/event-stream")
}

impl FakeModelApi {
    async fn start(host: &str) -> Self {
        use http_body_util::{BodyExt, Full};
        use hyper::body::Incoming;
        let ca = crate::egress_broker::SessionCa::new("external-turn-model-api").unwrap();
        let (cert, key) = ca.leaf(host).unwrap();
        let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .unwrap();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let recorded = seen.clone();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                let recorded = recorded.clone();
                tokio::spawn(async move {
                    let Ok(tls) = acceptor.accept(tcp).await else {
                        return;
                    };
                    let service =
                        hyper::service::service_fn(move |request: hyper::Request<Incoming>| {
                            let recorded = recorded.clone();
                            async move {
                                let authorization = request
                                    .headers()
                                    .get_all(hyper::header::AUTHORIZATION)
                                    .iter()
                                    .map(|value| String::from_utf8_lossy(value.as_bytes()).into())
                                    .collect();
                                let method = request.method().to_string();
                                let target = request
                                    .uri()
                                    .path_and_query()
                                    .map(|target| target.as_str().to_string())
                                    .unwrap_or_default();
                                let path = request.uri().path().to_string();
                                let bytes = request
                                    .into_body()
                                    .collect()
                                    .await
                                    .map(|body| body.to_bytes())
                                    .unwrap_or_default();
                                let body: serde_json::Value =
                                    serde_json::from_slice(&bytes).unwrap_or_default();
                                let answer = match (method.as_str(), path.as_str()) {
                                    ("POST", "/v1/messages") => Some(anthropic_answer(&body)),
                                    ("POST", "/v1/responses") => Some(openai_answer(&body)),
                                    _ => None,
                                };
                                recorded.lock().unwrap().push(ApiRequest {
                                    method,
                                    target,
                                    authorization,
                                    body,
                                });
                                let (text, kind) = answer.unwrap_or_else(|| {
                                    ("{\"error\":\"not found\"}".into(), "application/json")
                                });
                                let mut response =
                                    hyper::Response::new(Full::new(bytes::Bytes::from(text)));
                                response.headers_mut().insert(
                                    hyper::header::CONTENT_TYPE,
                                    hyper::header::HeaderValue::from_static(kind),
                                );
                                if kind == "application/json" && path != "/v1/messages" {
                                    *response.status_mut() = hyper::StatusCode::NOT_FOUND;
                                }
                                Ok::<_, std::convert::Infallible>(response)
                            }
                        });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(hyper_util::rt::TokioIo::new(tls), service)
                        .await;
                });
            }
        });
        Self { port, ca, seen }
    }

    fn seen(&self) -> Vec<ApiRequest> {
        self.seen.lock().unwrap().clone()
    }

    /// Trust only this API's authority.
    fn verifier(&self) -> Arc<dyn rustls::client::danger::ServerCertVerifier> {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(self.ca.der().clone()).unwrap();
        rustls::client::WebPkiServerVerifier::builder_with_provider(
            Arc::new(roots),
            crate::egress_broker::crypto_provider(),
        )
        .build()
        .unwrap()
    }
}

/// How each sent turn's Send ended, once it has.
type SendResults = Arc<StdMutex<HashMap<String, Option<Result<(), String>>>>>;

/// The run driver's view of the daemon, as the server's `DaemonRunHost`
/// gives it, for a run driven in this process.
struct TestRunHost {
    daemon: Arc<AxocoatlDaemon>,
    checks: StdMutex<Vec<crate::loadout::host::CheckLabel>>,
    sends: SendResults,
    observed: StdMutex<Vec<axocoatl_session::run_outcome::TurnObservation>>,
}

#[async_trait::async_trait]
impl crate::loadout::RunHost for TestRunHost {
    async fn apply_team(
        &self,
        session_id: &str,
        edit: crate::SessionTeamEdit,
    ) -> Result<(), crate::loadout::RunError> {
        *self.checks.lock().unwrap() = crate::loadout::host::CheckLabel::of_edit(&edit);
        Ok(self.daemon.apply_loadout_team(session_id, edit).await?)
    }

    async fn send_turn(
        &self,
        session_id: &str,
        request: &str,
    ) -> Result<String, crate::loadout::RunError> {
        let turn_id = format!("turn-{}", uuid::Uuid::new_v4());
        self.sends.lock().unwrap().insert(turn_id.clone(), None);
        let (daemon, sends) = (self.daemon.clone(), self.sends.clone());
        let (session, turn, input) = (session_id.to_string(), turn_id.clone(), request.to_string());
        tokio::spawn(async move {
            let (sink, receiver) =
                tokio::sync::mpsc::unbounded_channel::<axocoatl_actor::AgentStreamChunk>();
            drop(receiver);
            let result = daemon
                .execute_session_turn_streaming(
                    &session,
                    &turn,
                    Some(turn.clone()),
                    None,
                    &input,
                    Vec::new(),
                    Vec::new(),
                    None,
                    None,
                    sink,
                )
                .await;
            sends.lock().unwrap().insert(
                turn,
                Some(result.map(|_| ()).map_err(|error| error.to_string())),
            );
        });
        Ok(turn_id)
    }

    async fn wait_turn(
        &self,
        session_id: &str,
        turn_id: &str,
        deadline: std::time::Instant,
    ) -> Result<axocoatl_session::run_outcome::TurnObservation, crate::loadout::RunError> {
        use axocoatl_session::run_outcome::TurnState;
        loop {
            let labels = self.checks.lock().unwrap().clone();
            let observed = self
                .daemon
                .loadout_turn_observation(session_id, turn_id, &labels, None)
                .await?;
            let sent = self.sends.lock().unwrap().get(turn_id).cloned().flatten();
            match (&observed, &sent) {
                (Some(observation), _) if observation.state != TurnState::Running => {
                    self.observed.lock().unwrap().push(observation.clone());
                    return Ok(observation.clone());
                }
                (None, Some(Err(error))) => {
                    return Err(crate::loadout::RunError::Infrastructure(format!(
                        "the turn could not start: {error}"
                    )))
                }
                _ => {}
            }
            if std::time::Instant::now() >= deadline {
                return observed.ok_or(crate::loadout::RunError::Deadline);
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    async fn stop_turn(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> Result<(), crate::loadout::RunError> {
        self.daemon
            .stop_session_turn(session_id, turn_id)
            .await
            .map(|_| ())
            .map_err(Into::into)
    }

    async fn run_repro(
        &self,
        _session_id: &str,
        _request: &crate::loadout::host::ReproRequest,
    ) -> Result<axocoatl_session::run_outcome::ReproRun, crate::loadout::RunError> {
        Err(crate::loadout::RunError::NotImplemented(
            "this run has no reproduction",
        ))
    }

    async fn read_sandbox_file(
        &self,
        session_id: &str,
        path: &str,
        max_bytes: usize,
    ) -> Result<Option<Vec<u8>>, crate::loadout::RunError> {
        Ok(self
            .daemon
            .loadout_read_sandbox_file(session_id, path, max_bytes)
            .await?)
    }

    async fn record(
        &self,
        run_id: &str,
        event: axocoatl_session::run_record::RunEvent,
    ) -> Result<(), crate::loadout::RunError> {
        Ok(self
            .daemon
            .record_loadout_run_event(run_id, &event)
            .map(|_| ())?)
    }

    async fn recorded_events(
        &self,
        run_id: &str,
    ) -> Result<Vec<axocoatl_session::run_record::RunEvent>, crate::loadout::RunError> {
        Ok(self.daemon.loadout_run_recorded_events(run_id)?)
    }

    async fn network_summary(
        &self,
        session_id: &str,
    ) -> Result<axocoatl_session::run_outcome::NetworkSummary, crate::loadout::RunError> {
        Ok(self.daemon.loadout_network_summary(session_id).await)
    }

    async fn finish(
        &self,
        run_id: &str,
        outcome: &axocoatl_session::run_outcome::RunOutcome,
    ) -> Result<(), crate::loadout::RunError> {
        Ok(self.daemon.finish_loadout_run(run_id, outcome)?)
    }
}

/// Removes the containers, volumes and networks of the Sessions whose ids
/// it holds, whatever an assertion does.
struct PodmanCleanup(StdMutex<Vec<String>>);

impl Drop for PodmanCleanup {
    fn drop(&mut self) {
        for id in self.0.lock().unwrap().iter() {
            let names: Vec<String> = ["axo-ses-", "axo-egr-", "axo-brw-", "axo-pvw-", "axo-svc-"]
                .iter()
                .map(|prefix| format!("{prefix}{id}"))
                .collect();
            let _ = std::process::Command::new("podman")
                .args(["rm", "-f", "--ignore"])
                .args(&names)
                .output();
            for kind in ["volume", "network"] {
                if let Ok(listed) = std::process::Command::new("podman")
                    .args([kind, "ls", "--format", "{{.Name}}"])
                    .output()
                {
                    for name in String::from_utf8_lossy(&listed.stdout)
                        .lines()
                        .filter(|name| name.contains(id.as_str()))
                    {
                        let _ = std::process::Command::new("podman")
                            .args([kind, "rm", "-f", name])
                            .output();
                    }
                }
            }
        }
    }
}

fn git(repo: &std::path::Path, args: &[&str]) {
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

/// A whole loadout run whose writer is `runtime`'s pinned program from its
/// recipe image, on a daemon with real Podman: admission (the recipe
/// image, the secret store's credential, the hardened egress Session), the
/// run driver's Apply, Send and wait, the required check and the Outcome.
/// The program talks to a fake model API through the Session's route, at
/// its real host and port: only the route broker's resolver, address
/// classes, upstream port and upstream trust are the test's. A `reviewed`
/// run is a fix loadout whose native reviewer (a fake Ollama model) asks for
/// changes once, so the writer runs its program a second time, answers the
/// finding and is approved.
async fn pinned_run_child_body(runtime: AgentRuntime, reviewed: bool) {
    use axocoatl_session::network_record::{Decision, NetworkEvent};
    use axocoatl_session::run_outcome::{RunVerdict, TurnState};
    let (recipe, host, credential, placeholder, answer) = match runtime {
        AgentRuntime::ClaudeCode => (
            "claude-code",
            "api.anthropic.com",
            "claude-code-oauth",
            "CLAUDE_CODE_OAUTH_TOKEN",
            CLAUDE_ANSWER,
        ),
        AgentRuntime::Codex => (
            "codex",
            "api.openai.com",
            "codex-openai",
            "OPENAI_API_KEY",
            CODEX_ANSWER,
        ),
        AgentRuntime::Native => unreachable!("an external runtime"),
    };
    let image = axocoatl_isolation::recipes::image_name(&[recipe.to_string()]).unwrap();
    let inspected = std::process::Command::new("podman")
        .args(["image", "inspect", "--format", "{{.Id}}", &image])
        .output()
        .unwrap();
    assert!(
        inspected.status.success(),
        "build {image} first: axocoatl recipe build {recipe} ({})",
        String::from_utf8_lossy(&inspected.stderr)
    );
    let image_id = String::from_utf8_lossy(&inspected.stdout)
        .trim()
        .to_string();

    // The daemon's configuration and the user loadout next to it.
    let (reviewer, reviews) = reviewer_model_server().await;
    let home = tempfile::tempdir().unwrap();
    let config_file = home.path().join("config.yaml");
    let yaml = format!(
        "agents: []\nproviders:\n  ollama:\n    base_url: {}\nsandbox:\n  backend: podman\n  \
         network: bridge\nconsolidation:\n  enabled: false\n",
        reviewer.uri()
    );
    std::fs::write(&config_file, &yaml).unwrap();
    std::fs::create_dir(home.path().join("loadouts")).unwrap();
    let (loadout_id, loadout_text) = if reviewed {
        (
            format!("external-fix-{recipe}"),
            external_fix_loadout(runtime),
        )
    } else {
        (format!("external-{recipe}"), external_loadout(runtime))
    };
    std::fs::write(
        home.path().join(format!("loadouts/{loadout_id}.yaml")),
        loadout_text,
    )
    .unwrap();
    let config = axocoatl_config::parse_config(&yaml, &config_file).unwrap();
    let daemon = Arc::new(AxocoatlDaemon::bootstrap_headless(config).await.unwrap());
    daemon.set_config_path(&config_file);

    // The model API, reached at its real host name and port through the
    // route: the broker resolves the host to the fake and trusts its
    // authority. Everything else is production's.
    let api = FakeModelApi::start(host).await;
    daemon
        .egress_points
        .use_test_upstreams(super::session_network_policy::TestUpstreams {
            resolver: crate::session_egress::tests::FakeResolver::with(&[(host, &["127.0.0.1"])]),
            classify: crate::session_egress::route_tests::loopback_is_public,
            upstream: Arc::new(
                crate::egress_broker::UpstreamConnector::with_verifier(
                    api.verifier(),
                    Arc::new(|_| false),
                )
                .on_port(api.port),
            ),
        });
    // `axocoatl recipe build` and `axocoatl secret set`, as they record.
    crate::external_agent::recipe_images::record_image(
        &daemon.data_root,
        crate::external_agent::recipe_images::record_for(&[recipe.to_string()], &image_id, 1)
            .unwrap(),
    )
    .unwrap();
    let secret = format!(
        "sk-axocoatl-test-{recipe}-{}",
        uuid::Uuid::new_v4().simple()
    );
    crate::secret_store::set_secret(daemon.data_root.path(), credential, secret.as_bytes())
        .unwrap();

    // The repository, outside the data root.
    let outside = tempfile::Builder::new()
        .prefix("axocoatl-external-turn-")
        .tempdir_in(std::env::temp_dir())
        .unwrap();
    let repo = outside.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "test@example.invalid"]);
    git(&repo, &["config", "user.name", "Test"]);
    std::fs::write(repo.join("README.md"), "external turn\n").unwrap();
    git(&repo, &["add", "README.md"]);
    git(&repo, &["commit", "-q", "-m", "initial"]);

    let cleanup = PodmanCleanup(StdMutex::new(Vec::new()));
    let mut params = ParamValues::new();
    params.insert("writer_model".into(), writer_model(runtime).into());
    if reviewed {
        params.insert("reviewer_model".into(), format!("ollama:{REVIEW_MODEL}"));
    }
    let request = crate::loadout::api::RunRequest {
        loadout: loadout_id.clone(),
        task: "Create fixed.txt in the repository.".into(),
        repo: repo.display().to_string(),
        params,
        keep: Default::default(),
        check_command: None,
        setup_command: None,
        request_id: loadout_id.clone(),
    };
    let result = async {
        let (accepted, context) = daemon.admit_loadout_run(request).await?;
        cleanup.0.lock().unwrap().push(accepted.session_id.clone());
        let host_view = TestRunHost {
            daemon: daemon.clone(),
            checks: StdMutex::new(Vec::new()),
            sends: Arc::new(StdMutex::new(HashMap::new())),
            observed: StdMutex::new(Vec::new()),
        };
        let outcome = crate::loadout::driver::run_to_outcome(&host_view, &context)
            .await
            .map_err(|error| DaemonError::Session(error.to_string()))?;
        let observed = host_view.observed.lock().unwrap().clone();
        Ok::<_, DaemonError>((accepted, outcome, observed))
    }
    .await;
    let (accepted, outcome, observed) = match result {
        Ok(done) => done,
        Err(error) => {
            let _ = daemon.shutdown_session_runtimes_checked().await;
            panic!("{recipe}: {error}");
        }
    };
    let session_id = accepted.session_id.clone();
    // The run's JUnit, as `GET /api/runs/{id}/junit` serves it.
    let junit = daemon.loadout_run_junit(&accepted.run_id).await;
    let seen = api.seen();
    let sandbox = daemon
        .session_sandboxes
        .lock()
        .await
        .get(&session_id)
        .cloned()
        .expect("the run's Session container is still Ready");
    let read = |path: &'static str| {
        let sandbox = sandbox.clone();
        async move {
            sandbox
                .exec(&["cat", path], Duration::from_secs(30))
                .await
                .map(|result| result.stdout)
                .unwrap_or_default()
        }
    };
    let program_env = read("/tmp/axocoatl-program-env.txt").await;
    let program_home = read("/tmp/axocoatl-program-home.txt").await;
    let program_identity = read("/tmp/axocoatl-program-identity.txt").await;
    let found = sandbox
        .exec(
            &[
                "sh",
                "-c",
                "grep -rIl -F -e \"$1\" /etc /tmp /home \"$2\" 2>/dev/null; true",
                "sh",
                &secret,
                &sandbox.root().display().to_string(),
            ],
            Duration::from_secs(60),
        )
        .await
        .map(|result| result.stdout)
        .unwrap_or_else(|error| error.to_string());
    let inspected: Vec<String> = [
        format!("axo-ses-{session_id}"),
        format!("axo-egr-{session_id}"),
    ]
    .iter()
    .map(|container| {
        let output = std::process::Command::new("podman")
            .args(["inspect", container])
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).into_owned()
    })
    .collect();
    let mut events = Vec::new();
    let mut after = None;
    loop {
        let page = daemon
            .session_network_records
            .read_after(&session_id, after, 500)
            .await
            .unwrap();
        if page.events.is_empty() {
            break;
        }
        after = page.events.last().map(|line| line.seq);
        events.extend(page.events.into_iter().map(|line| line.event));
    }
    // The program's parsed output in the Session record: its work log as
    // the writer activation's reasoning stream.
    let history = daemon
        .versioned_session_history_snapshot(&session_id)
        .await
        .unwrap();
    let work_log: String = observed
        .first()
        .and_then(|turn| history.get(&turn.turn_id))
        .map(|entry| match entry {
            SessionHistoryEntry::ExecutionV2(turn) => turn
                .activations
                .iter()
                .flat_map(|activation| activation.stream.iter())
                .filter_map(|item| match &item.content.payload {
                    axocoatl_session::execution_content::ActivationStreamPayload::ReasoningSummary { delta } => {
                        Some(delta.as_str())
                    }
                    _ => None,
                })
                .collect(),
            _ => String::new(),
        })
        .unwrap_or_default();
    let shutdown = daemon.shutdown_session_runtimes_checked().await;
    let context = format!(
        "{recipe}: {outcome:#?}\n{observed:#?}\nAPI: {:#?}\nrecord: {events:#?}",
        seen.iter()
            .map(|request| format!(
                "{} {} {:?} tools={}",
                request.method,
                request.target,
                request.authorization,
                request.body["tools"]
                    .as_array()
                    .map(|tools| tools
                        .iter()
                        .map(|tool| tool["name"]
                            .as_str()
                            .or(tool["type"].as_str())
                            .unwrap_or("?")
                            .to_string())
                        .collect::<Vec<_>>()
                        .join(","))
                    .unwrap_or_default()
            ))
            .collect::<Vec<_>>()
    );
    shutdown.unwrap();

    // The run passed: its turn completed, the writer's answer is the
    // program's final text, and the required check found the change.
    assert_eq!(outcome.verdict, RunVerdict::Pass, "{context}");
    assert_eq!(observed.len(), 1, "{context}");
    let turn = &observed[0];
    assert_eq!(turn.state, TurnState::Completed, "{context}");
    let writer = turn
        .nodes
        .iter()
        .find(|node| node.kind != "reviewer")
        .expect("the writer's node");
    let latest = writer
        .latest()
        .and_then(|generation| generation.answer.as_deref())
        .unwrap_or_default();
    assert!(latest.starts_with(answer), "{context}");
    // The writer is named as the loadout names it: the model API's
    // provider, the program's model and the runtime.
    assert_eq!(
        writer.model,
        crate::loadout::team_plan::model_identity(
            &axocoatl_config::loadout::ModelSpec::parse(writer_model(runtime)).unwrap(),
            runtime
        ),
        "{context}"
    );
    assert_eq!(writer.model.runtime, recipe, "{context}");
    // Claude Code reports what its run cost. Codex reports tokens but no
    // cost: the run's cost is what its calls reserved, which the Outcome and
    // the JUnit file say, never a price.
    let junit = junit.unwrap_or_else(|error| panic!("{error}\n{context}"));
    assert!(outcome.usage.complete, "{context}");
    match runtime {
        AgentRuntime::Codex => {
            assert!(!outcome.usage.cost_known, "{context}");
            assert!(outcome.usage.cost_microunits > 0, "{context}");
            assert!(
                junit.contains("output tokens, cost unknown (reserved up to $"),
                "{junit}"
            );
        }
        _ => {
            assert!(outcome.usage.cost_known, "{context}");
            assert!(!junit.contains("cost unknown"), "{junit}");
        }
    }
    assert_eq!(outcome.checks.len(), 1, "{context}");
    let activations = if reviewed { 2 } else { 1 };
    assert_eq!(writer.generations.len(), activations, "{context}");
    if reviewed {
        // The reviewer asked for changes once; the writer's second program
        // run answered the finding and the reviewer approved.
        let review = outcome.review.as_ref().expect("the run's review");
        assert!(review.passed, "{context}");
        assert_eq!(review.rounds.len(), 2, "{context}");
        assert_eq!(reviews.load(Ordering::SeqCst), 2, "{context}");
        assert_eq!(outcome.adjudications.len(), 1, "{context}");
        assert_eq!(
            outcome.adjudications[0].decision,
            axocoatl_session::run_outcome::AdjudicationDecision::Accept,
            "{context}"
        );
        assert!(latest.contains("ADJUDICATIONS"), "{context}");
    } else {
        assert!(outcome.review.is_none(), "{context}");
        assert_eq!(reviews.load(Ordering::SeqCst), 0, "{context}");
    }
    // The program's own edit landed in the Workspace.
    assert_eq!(
        std::fs::read_to_string(repo.join("fixed.txt")).unwrap(),
        "fixed\n",
        "{context}"
    );
    // The fake API got the program's requests with the stored secret, from
    // the route, and nothing else was asked of it.
    let calls: Vec<&ApiRequest> = seen
        .iter()
        .filter(|request| request.method == "POST")
        .collect();
    assert_eq!(calls.len(), 2 * activations, "{context}");
    assert_eq!(calls.len(), seen.len(), "{context}");
    let path = match runtime {
        AgentRuntime::ClaudeCode => "/v1/messages",
        _ => "/v1/responses",
    };
    for call in &calls {
        assert!(
            call.target == path || call.target.starts_with(&format!("{path}?")),
            "{context}"
        );
    }
    for call in &calls {
        assert_eq!(
            call.authorization,
            [format!("Bearer {secret}")],
            "{context}"
        );
    }
    // The program saw only the placeholder; the secret is nowhere in the
    // container or its configuration.
    assert!(
        program_env.contains(&format!("{placeholder}=axocoatl-route:{host}")),
        "{context}\n{program_env}"
    );
    assert!(
        program_env.contains("SSL_CERT_FILE=") && program_env.contains("HTTPS_PROXY="),
        "{program_env}"
    );
    assert!(
        program_env.contains("NODE_EXTRA_CA_CERTS=/etc/axocoatl/ca/session-ca.pem"),
        "{program_env}"
    );
    assert!(program_env.contains("HOME=/home/axocoatl"), "{program_env}");
    // Its tool ran as the non-root writer user, without capabilities and
    // under no-new-privileges (the supervisor's `--harden`).
    let uid = program_identity.lines().next().unwrap_or_default();
    assert!(!uid.is_empty() && uid != "0", "{program_identity}");
    assert!(
        program_identity.contains("NoNewPrivs:\t1")
            && program_identity.contains("CapEff:\t0000000000000000"),
        "{program_identity}"
    );
    assert!(!program_env.contains(&secret), "{context}");
    assert_eq!(found.trim(), "", "{context}");
    for inspect in &inspected {
        assert!(
            !inspect.is_empty() && !inspect.contains(&secret),
            "{context}"
        );
    }
    let refused: Vec<String> = events
        .iter()
        .filter_map(|event| match event {
            NetworkEvent::Open {
                host,
                port,
                decision: Decision::Deny,
                reason,
                ..
            } => Some(format!("{host}:{port} {}", reason.as_deref().unwrap_or(""))),
            NetworkEvent::Request {
                host,
                method,
                path,
                decision: Decision::Deny,
                reason,
                ..
            } => Some(format!(
                "{method} {host}{path} {}",
                reason.as_deref().unwrap_or("")
            )),
            _ => None,
        })
        .collect();
    eprintln!(
        "{recipe}: model API requests {:?}; refused {refused:?}; usage {:?}; the program's \
         home directory:\n{program_home}",
        calls.iter().map(|call| &call.target).collect::<Vec<_>>(),
        outcome.usage,
    );
    // Every model call is in the Session's network record, allowed by the
    // route with the credential's name, never its value.
    for event in &events {
        assert!(
            !serde_json::to_string(event).unwrap().contains(&secret),
            "{context}"
        );
    }
    let recorded = events
        .iter()
        .filter(|event| {
            matches!(event, NetworkEvent::Request { host: requested, method, decision: Decision::Allow, credential: Some(name), .. }
                if requested == host && method == "POST" && name == credential)
        })
        .count();
    assert_eq!(recorded, calls.len(), "{context}");
    assert!(
        outcome.network.route_requests >= calls.len() as u64,
        "{context}"
    );
    // Nothing reached another host. Claude Code 2.1.292 also asks the route
    // for its account's policy limits and remote settings; the route refuses
    // them, records the refusal and the program goes on.
    let allowed_refusals: &[&str] = match runtime {
        AgentRuntime::ClaudeCode => &[
            "GET api.anthropic.com/api/claude_code/policy_limits",
            "GET api.anthropic.com/api/claude_code/settings",
        ],
        _ => &[],
    };
    for refusal in &refused {
        assert!(
            allowed_refusals
                .iter()
                .any(|known| refusal.starts_with(&format!("{known} "))),
            "{recipe}: {refusal}\n{context}"
        );
    }
    // The work log names the program, its exit and its model requests, and
    // previews its tool call and the usage it reported.
    let (program, tool, usage) = match runtime {
        AgentRuntime::ClaudeCode => (
            "@anthropic-ai/claude-code 2.1.292",
            "[tool call] Bash: ",
            "[usage] 240 input and 14 output tokens, $0.000310",
        ),
        _ => (
            "@openai/codex 0.160.1",
            "[tool call] command: ",
            "[usage] 600 input and 18 output tokens, as the program reported",
        ),
    };
    for wanted in [
        format!(
            "[external agent] {program}, model {}: exit 0, 2 model request(s)",
            program_model(runtime),
        ),
        tool.to_string(),
        usage.to_string(),
    ] {
        assert!(work_log.contains(&wanted), "{wanted}\n{work_log}");
    }
    drop(cleanup);
}

/// The reviewer's model.
const REVIEW_MODEL: &str = "review-model:latest";

/// An audited local Ollama with [`REVIEW_MODEL`], whose chat answers ask
/// for one change and then approve; the counter counts its chats.
async fn reviewer_model_server() -> (wiremock::MockServer, Arc<AtomicUsize>) {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    let digest = "a80c4f17acd55265feec403c7aef86be0c25983ab279d83f3bcd3abbcb5b8b72";
    let model = serde_json::json!({"name": REVIEW_MODEL, "model": REVIEW_MODEL,
        "digest": digest, "details": {"format": "gguf"}, "context_length": 32768});
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
        ("GET", "/api/tags", serde_json::json!({"models": [model]})),
        ("GET", "/api/ps", serde_json::json!({"models": [model]})),
    ] {
        Mock::given(method(verb))
            .and(path(route))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
    }
    Mock::given(method("POST"))
        .and(path("/api/generate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "model": REVIEW_MODEL, "created_at": "2026-10-07T00:00:00Z",
            "response": "", "done": true, "done_reason": "load"
        })))
        .mount(&server)
        .await;
    let reviews = Arc::new(AtomicUsize::new(0));
    let counted = reviews.clone();
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(move |_: &wiremock::Request| {
            let content = if counted.fetch_add(1, Ordering::SeqCst) == 0 {
                "VERDICT: CHANGES\nF1: fixed.txt: write the file once more and say so."
            } else {
                "VERDICT: APPROVE"
            };
            let reply = serde_json::json!({"model": REVIEW_MODEL,
                "message": {"role": "assistant", "content": content},
                "done": true, "done_reason": "stop", "prompt_eval_count": 50, "eval_count": 10});
            ResponseTemplate::new(200).set_body_raw(format!("{reply}\n"), "application/x-ndjson")
        })
        .mount(&server)
        .await;
    (server, reviews)
}

/// Claude Code 2.1.292 from `localhost/axocoatl-recipe-claude-code`, as
/// `axocoatl run` runs it (see [`pinned_run_child_body`]).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Podman (CONTAINER_CONNECTION) and the claude-code recipe image"]
async fn actual_loadout_run_with_the_pinned_claude_code() {
    if std::env::var_os(CHILD).is_some() {
        pinned_run_child_body(AgentRuntime::ClaudeCode, false).await;
        return;
    }
    run_child("actual_loadout_run_with_the_pinned_claude_code", None, true).await;
}

/// Codex 0.160.1 from `localhost/axocoatl-recipe-codex`, as `axocoatl run`
/// runs it (see [`pinned_run_child_body`]).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Podman (CONTAINER_CONNECTION) and the codex recipe image"]
async fn actual_loadout_run_with_the_pinned_codex() {
    if std::env::var_os(CHILD).is_some() {
        pinned_run_child_body(AgentRuntime::Codex, false).await;
        return;
    }
    run_child("actual_loadout_run_with_the_pinned_codex", None, true).await;
}

/// A fix run with the pinned Claude Code as its writer and a native
/// reviewer that asks for changes once (see [`pinned_run_child_body`]).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Podman (CONTAINER_CONNECTION) and the claude-code recipe image"]
async fn actual_reviewed_loadout_run_with_the_pinned_claude_code() {
    if std::env::var_os(CHILD).is_some() {
        pinned_run_child_body(AgentRuntime::ClaudeCode, true).await;
        return;
    }
    run_child(
        "actual_reviewed_loadout_run_with_the_pinned_claude_code",
        None,
        true,
    )
    .await;
}

/// A fix run with the pinned Codex as its writer and a native reviewer
/// that asks for changes once (see [`pinned_run_child_body`]).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Podman (CONTAINER_CONNECTION) and the codex recipe image"]
async fn actual_reviewed_loadout_run_with_the_pinned_codex() {
    if std::env::var_os(CHILD).is_some() {
        pinned_run_child_body(AgentRuntime::Codex, true).await;
        return;
    }
    run_child(
        "actual_reviewed_loadout_run_with_the_pinned_codex",
        None,
        true,
    )
    .await;
}
