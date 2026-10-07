//! Loadout runs on a live daemon: the per-Session sandbox binding, Team
//! Apply with inline loadout definitions and the same-model warning, and
//! admission refusals. Each body runs in a child process with its own data
//! root (bootstrap reads the process environment) and, unless it says
//! otherwise, a fake Podman.
use super::*;
use axocoatl_config::loadout::{builtin_loadouts, ParamValues};
use axocoatl_session::execution_namespace::ExecutionComponent;
use axocoatl_session::execution_ownership::DataRootFormatOwnership;
use axocoatl_session::session_team::SessionTeamStore;
use std::os::unix::fs::PermissionsExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CHILD: &str = "AXOCOATL_LOADOUT_TEST_CHILD";
const MODEL: &str = "loadout-model:latest";
const OTHER: &str = "other-model:latest";

/// The audited local Ollama server the native provider admits.
async fn model_server() -> MockServer {
    let server = MockServer::start().await;
    let digest = "a80c4f17acd55265feec403c7aef86be0c25983ab279d83f3bcd3abbcb5b8b72";
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
        (
            "GET",
            "/api/tags",
            serde_json::json!({"models": [
                {"name": MODEL, "model": MODEL, "digest": digest},
                {"name": OTHER, "model": OTHER, "digest": digest}]}),
        ),
        (
            "GET",
            "/api/ps",
            serde_json::json!({"models": [
                {"name": MODEL, "model": MODEL, "digest": digest,
                    "details": {"format": "gguf"}, "context_length": 32768},
                {"name": OTHER, "model": OTHER, "digest": digest,
                    "details": {"format": "gguf"}, "context_length": 32768}]}),
        ),
    ] {
        Mock::given(method(verb))
            .and(path(route))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
    }
    // A load acknowledges the model it was asked for.
    Mock::given(method("POST"))
        .and(path("/api/generate"))
        .respond_with(|request: &wiremock::Request| {
            let body: serde_json::Value = request.body_json().unwrap_or_default();
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "model": body["model"], "created_at": "2026-10-06T00:00:00Z",
                "response": "", "done": true, "done_reason": "load"
            }))
        })
        .mount(&server)
        .await;
    server
}

fn config(base_url: &str, backend: &str) -> AxocoatlConfig {
    let e2b = if backend == "e2b" {
        "\n  e2b:\n    api_key: test-only-key"
    } else {
        ""
    };
    let yaml = format!(
        r#"
agents:
  - id: conversation
    name: Conversation
    provider: ollama
    model: {MODEL}
    tools: [read_file]
providers:
  ollama:
    base_url: {base_url}
sandbox:
  backend: {backend}
  network: bridge{e2b}
consolidation:
  enabled: false
"#
    );
    axocoatl_config::parse_config(&yaml, std::path::Path::new("loadout.yaml")).unwrap()
}

/// A native Session in `work`, bound to `binding` when one is given.
async fn native_session(
    daemon: &AxocoatlDaemon,
    work: &std::path::Path,
    binding: Option<SessionLoadoutBinding>,
) -> Session {
    let workspace = daemon
        .workspace_store
        .lock()
        .await
        .register(work, Some("Loadout"))
        .unwrap();
    let DataRootFormatOwnership::Upgraded(ownership) = &daemon._data_dir_lease.ownership else {
        panic!("fresh data roots use native ownership");
    };
    let mut sessions = daemon.session_store.lock().await;
    let (session, receipt) = sessions
        .create_native_with_environment(
            ownership,
            "Loadout",
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
    match binding {
        Some(binding) => sessions.bind_loadout(&session.id, binding).unwrap(),
        None => session,
    }
}

fn binding(network: &str) -> SessionLoadoutBinding {
    SessionLoadoutBinding {
        run_id: format!("run-{}", uuid::Uuid::new_v4()),
        loadout: LoadoutRef {
            id: "fix".into(),
            version: 1,
            kind: "fix".into(),
            digest: "0".repeat(64),
            builtin: true,
        },
        network: network.into(),
        workload: "hardened".into(),
    }
}

fn resolved_fix(writer: &str, reviewer: &str) -> axocoatl_config::loadout::ResolvedLoadout {
    let fix = builtin_loadouts()
        .into_iter()
        .map(Result::unwrap)
        .find(|loadout| loadout.file.id == "fix")
        .unwrap();
    let mut params = ParamValues::new();
    params.insert("writer_model".into(), writer.into());
    params.insert("reviewer_model".into(), reviewer.into());
    let mut resolved = resolve_loadout(&fix, &params, "fix it", "/repo").unwrap();
    crate::loadout::team_plan::fill_detected_checks(&mut resolved, Some("true"), None).unwrap();
    resolved
}

async fn team_child_body() {
    let server = model_server().await;
    let daemon = AxocoatlDaemon::bootstrap_headless(config(&server.uri(), "podman"))
        .await
        .unwrap();
    let work = tempfile::tempdir().unwrap();

    // An unbound Session keeps the global network and workload plan; a
    // bound one runs under its loadout's network, hardened, whatever the
    // global `sandbox.network` says.
    let plain = native_session(&daemon, work.path(), None).await;
    let (network, plan, _) = daemon.session_sandbox_policy(&plain).unwrap();
    assert_eq!(network, "bridge");
    assert_eq!(plan, axocoatl_config::workload::WorkloadPlan::Image);
    let work_bound = tempfile::tempdir().unwrap();
    let bound = native_session(&daemon, work_bound.path(), Some(binding("egress"))).await;
    let (network, plan, _) = daemon.session_sandbox_policy(&bound).unwrap();
    assert_eq!(network, "egress");
    assert_eq!(
        plan,
        axocoatl_config::workload::WorkloadPlan::Hardened { required: true }
    );
    // A binding is set once, and never with a weaker sandbox.
    assert!(daemon
        .session_store
        .lock()
        .await
        .bind_loadout(&bound.id, binding("egress"))
        .is_err());
    assert!(daemon
        .session_store
        .lock()
        .await
        .bind_loadout(&plain.id, binding("bridge"))
        .is_err());
    // The Session's JSON carries its binding.
    let wire = serde_json::to_value(daemon.get_session(&bound.id).await.unwrap()).unwrap();
    assert_eq!(wire["loadout"]["network"], "egress");
    assert!(serde_json::to_value(&plain)
        .unwrap()
        .get("loadout")
        .is_none());

    // A loadout's own Agents and reviewer apply through preview → apply,
    // defined inline: no configured template names them.
    let model = format!("ollama:{MODEL}");
    let resolved = resolved_fix(&model, &model);
    let slots = crate::loadout::team_plan::default_slots(&resolved).unwrap();
    let edit = crate::loadout::team_plan::team_edit(&resolved, &slots, true, 0).unwrap();
    daemon
        .apply_loadout_team(&bound.id, edit.clone())
        .await
        .unwrap();
    let team = daemon.session_team(&bound.id).await.unwrap();
    assert!(team.approved);
    assert_eq!(team.configuration_revision, 1);
    assert_eq!(team.slots.len(), 1);
    assert_eq!(team.slots[0].slot_id, "writer");
    assert_eq!(team.slots[0].model, MODEL);
    assert_eq!(team.required_checks, edit.required_checks);
    // The record bundle's team section says what was applied: the writer
    // starts from a fresh conversation, with the loadout's own definition
    // and tools. The team view keeps offering reset_history: false, the
    // choice for the person's next edit.
    assert!(!team.slots[0].reset_history);
    let record = daemon.session_team_record(&bound.id).await.unwrap();
    let slot = &record["slots"][0];
    assert_eq!(slot["slot_id"], "writer");
    assert_eq!(slot["reset_history"], true, "{record}");
    assert_eq!(
        slot["tools"],
        serde_json::json!([
            "read_file",
            "list_dir",
            "grep",
            "glob",
            "write_file",
            "edit_file",
            "bash"
        ]),
        "{record}"
    );
    assert!(
        slot["definition"]["source"]
            .as_str()
            .is_some_and(|source| source.contains("fix")),
        "{record}"
    );
    assert_eq!(slot["definition"]["tools"], slot["tools"]);
    assert_eq!(record["configuration_revision"], 1);
    let review = team.required_review.as_ref().unwrap();
    assert_eq!(review.template_id, "loadout-reviewer");
    assert!(review.inline.is_some());
    // The same model writes and reviews: the team view says so.
    assert_eq!(
        team.warnings
            .iter()
            .map(|warning| warning.code.as_str())
            .collect::<Vec<_>>(),
        ["same_model_reviewer"]
    );
    // The retained slot definition is the loadout's, tools included.
    let token = daemon
        .session_dispatch_lifecycles
        .session_team_token(&bound.id)
        .unwrap();
    let tools = daemon
        .session_dispatch_lifecycles
        .with_session_team_stores(&token, |canonical, content, _| {
            let store = SessionTeamStore::open_owned(
                canonical
                    .component_namespace(ExecutionComponent::SessionTeam)
                    .map_err(|error| DaemonError::Session(error.to_string()))?,
                canonical,
                content,
                None,
            )
            .map_err(|error| DaemonError::Session(error.to_string()))?;
            let current = store
                .current()
                .map_err(|error| DaemonError::Session(error.to_string()))?
                .unwrap();
            session_team::slot_tools_for_tests(&current.graph.slots[0], content)
        })
        .unwrap();
    assert_eq!(
        tools,
        [
            "read_file",
            "list_dir",
            "grep",
            "glob",
            "write_file",
            "edit_file",
            "bash"
        ]
    );
    // A different reviewer model leaves no warning.
    let other_model = format!("ollama:{OTHER}");
    let resolved = resolved_fix(&model, &other_model);
    let edit = crate::loadout::team_plan::team_edit(
        &resolved,
        &crate::loadout::team_plan::default_slots(&resolved).unwrap(),
        true,
        0,
    )
    .unwrap();
    daemon.apply_loadout_team(&bound.id, edit).await.unwrap();
    let team = daemon.session_team(&bound.id).await.unwrap();
    assert_eq!(team.configuration_revision, 2);
    assert!(team.warnings.is_empty(), "{:?}", team.warnings);

    // An external writer (Claude Code) applies inline too: its retained
    // definition names the runtime as its provider and the program's model,
    // as the autonomous writer with bash and no per-call limits.
    let external = axocoatl_config::loadout::parse_loadout(
        r#"
schema: axocoatl.loadout/1
id: cc-fix
version: 1
name: Claude Code fix
kind: custom
agents:
  - id: writer
    role: writer
    runtime: claude-code
    model: { provider: anthropic, model: claude-sonnet-5-5 }
    tools: [bash]
checks:
  - { name: tests, run: { argv: [sh, -c, "true"] }, timeout: 2m }
budgets:
  agent: { activations: 1, invocations: 20, tokens: 100000, cost_usd: 1 }
  wall_clock: 10m
prompt: "{task}"
environment: { recipes: [claude-code] }
"#,
        axocoatl_config::loadout::LoadoutSource::Builtin,
    )
    .unwrap();
    let resolved = resolve_loadout(&external, &ParamValues::new(), "fix it", "/repo").unwrap();
    let edit = crate::loadout::team_plan::team_edit(
        &resolved,
        &crate::loadout::team_plan::default_slots(&resolved).unwrap(),
        true,
        0,
    )
    .unwrap();
    daemon.apply_loadout_team(&bound.id, edit).await.unwrap();
    let team = daemon.session_team(&bound.id).await.unwrap();
    assert_eq!(team.configuration_revision, 3);
    assert_eq!(team.slots[0].provider, "claude-code");
    assert_eq!(team.slots[0].model, "claude-sonnet-5-5");
    let tools = daemon
        .session_dispatch_lifecycles
        .with_session_team_stores(&token, |canonical, content, _| {
            let store = SessionTeamStore::open_owned(
                canonical
                    .component_namespace(ExecutionComponent::SessionTeam)
                    .map_err(|error| DaemonError::Session(error.to_string()))?,
                canonical,
                content,
                None,
            )
            .map_err(|error| DaemonError::Session(error.to_string()))?;
            let current = store
                .current()
                .map_err(|error| DaemonError::Session(error.to_string()))?
                .unwrap();
            session_team::slot_tools_for_tests(&current.graph.slots[0], content)
        })
        .unwrap();
    assert_eq!(tools, ["bash"]);

    // Admission refuses a host that cannot run a loadout hardened: this fake
    // Podman runs as root.
    let fix_repo = tempfile::tempdir().unwrap();
    let request = RunRequest {
        loadout: "fix".into(),
        task: "fix it".into(),
        repo: fix_repo.path().display().to_string(),
        params: [
            ("writer_model".to_string(), model.clone()),
            ("reviewer_model".to_string(), other_model.clone()),
        ]
        .into_iter()
        .collect(),
        keep: Default::default(),
        check_command: Some("true".into()),
        setup_command: None,
        request_id: "rootful".into(),
    };
    let refused = daemon.admit_loadout_run(request.clone()).await.unwrap_err();
    assert!(
        matches!(&refused, DaemonError::Session(message) if message.contains("rootless")),
        "{refused}"
    );
    // Usage errors come first and are 422/404 at the API.
    let mut unknown = request.clone();
    unknown.loadout = "nope".into();
    unknown.request_id = "unknown".into();
    assert!(matches!(
        daemon.admit_loadout_run(unknown).await,
        Err(DaemonError::NotFound(_))
    ));
    let mut missing = request.clone();
    missing.params.clear();
    missing.request_id = "missing".into();
    assert!(matches!(
        daemon.admit_loadout_run(missing).await,
        Err(DaemonError::InvalidRequest(_))
    ));
    let mut bad_id = request;
    bad_id.request_id = "a b".into();
    assert!(matches!(
        daemon.admit_loadout_run(bad_id).await,
        Err(DaemonError::InvalidRequest(_))
    ));
    // A run left without an Outcome by a restart ends failed, never resumed.
    let store = daemon.loadout_runs.store().unwrap();
    let cut_off = format!("run-{}", uuid::Uuid::new_v4());
    store
        .create(&RunManifest {
            schema: RUN_MANIFEST_SCHEMA.into(),
            run_id: cut_off.clone(),
            session_id: bound.id.clone(),
            workspace_id: bound.workspace_id.clone(),
            loadout: binding("egress").loadout,
            loadout_text: resolved.loadout.text.clone(),
            params: resolved.params.clone(),
            task: "fix it".into(),
            repo: "/repo".into(),
            repo_head: None,
            dirty_paths: Vec::new(),
            started_at_ms: 1,
            options: serde_json::Value::Null,
        })
        .unwrap();
    daemon.recover_loadout_runs().await;
    let ended = daemon.loadout_run(&cut_off).await.unwrap();
    assert_eq!(ended.state, "failed");
    let outcome = ended.outcome.unwrap();
    assert_eq!(outcome.exit_code, exit_code::INFRASTRUCTURE);
    assert!(outcome.error.unwrap().contains("restarted"));
    let events = daemon.loadout_run_recorded_events(&cut_off).unwrap();
    assert!(matches!(events.last(), Some(RunEvent::Ended { .. })));
    daemon.shutdown().await.unwrap();
}

async fn e2b_child_body() {
    let server = model_server().await;
    let config = config(&server.uri(), "e2b");
    let daemon = AxocoatlDaemon::bootstrap_headless(config).await.unwrap();
    let repo = tempfile::tempdir().unwrap();
    let request = RunRequest {
        loadout: "fix".into(),
        task: "fix it".into(),
        repo: repo.path().display().to_string(),
        params: [
            ("writer_model".to_string(), format!("ollama:{MODEL}")),
            (
                "reviewer_model".to_string(),
                "ollama:other:latest".to_string(),
            ),
        ]
        .into_iter()
        .collect(),
        keep: Default::default(),
        check_command: Some("true".into()),
        setup_command: None,
        request_id: "e2b".into(),
    };
    let refused = daemon.admit_loadout_run(request).await.unwrap_err();
    assert!(
        matches!(&refused, DaemonError::Session(message) if message == LOADOUT_NEEDS_LOCAL_PODMAN),
        "{refused}"
    );
    // A bound Session whose environment the E2B backend would prepare is
    // refused rather than run without its isolation.
    let work = tempfile::tempdir().unwrap();
    let session = native_session(&daemon, work.path(), Some(binding("egress"))).await;
    let failure = match daemon.start_prepared_session_sandbox(&session).await {
        Ok(_) => panic!("an E2B sandbox must not start for a loadout Session"),
        Err(failure) => failure.error.to_string(),
    };
    assert!(failure.contains("local rootless Podman"), "{failure}");
    daemon.shutdown().await.unwrap();
}

/// The configuration file of the admission test, with `browser.allow`.
fn admission_yaml(base_url: &str, browser_allow: &str) -> String {
    format!(
        r#"
agents:
  - id: conversation
    name: Conversation
    provider: ollama
    model: {MODEL}
    tools: [read_file]
providers:
  ollama:
    base_url: {base_url}
sandbox:
  backend: podman
  network: bridge
browser:
  allow: [{browser_allow}]
  private_destinations: [192.168.1.0/24]
consolidation:
  enabled: false
"#
    )
}

async fn admission_child_body() {
    let server = model_server().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("axocoatl.yaml");
    std::fs::write(
        &path,
        admission_yaml(&server.uri(), "{cidr: 192.168.1.0/24, ports: [8766]}"),
    )
    .unwrap();
    let config = axocoatl_config::load_config(&path).await.unwrap();
    let daemon = AxocoatlDaemon::bootstrap_headless(config).await.unwrap();
    daemon.set_config_path(&path);
    let repo = tempfile::tempdir().unwrap();
    // A Session of the repository's Workspace holds it, as the Session of a
    // turn that needs attention does until someone continues, stops or
    // closes that turn.
    let holder = native_session(&daemon, repo.path(), None).await;
    let operation = daemon
        .attempt_operation_for_workspace(&holder.workspace_id)
        .await;
    let held = operation.clone().lock_owned().await;
    let request = |target: &str, id: &str| RunRequest {
        loadout: "qa".into(),
        task: "find the bugs".into(),
        repo: repo.path().display().to_string(),
        params: [
            ("explorer_model".to_string(), format!("ollama:{MODEL}")),
            ("target_url".to_string(), target.to_string()),
        ]
        .into_iter()
        .collect(),
        keep: Default::default(),
        check_command: None,
        setup_command: None,
        request_id: id.into(),
    };
    let admit = |request: RunRequest| {
        let daemon = &daemon;
        async move {
            tokio::time::timeout(Duration::from_secs(60), daemon.admit_loadout_run(request))
                .await
                .expect("admission never waits for a held Workspace")
        }
    };
    let conflict = |result: Result<(RunAccepted, RunContext), DaemonError>| match result {
        Err(DaemonError::SessionConflict(message)) => message,
        Err(other) => panic!("expected a conflict, got {other}"),
        Ok((accepted, _)) => panic!("admitted {}", accepted.run_id),
    };
    let usage = |result: Result<(RunAccepted, RunContext), DaemonError>| match result {
        Err(DaemonError::InvalidRequest(message)) => message,
        Err(other) => panic!("expected a usage error, got {other}"),
        Ok((accepted, _)) => panic!("admitted {}", accepted.run_id),
    };

    // A cidr entry of browser.allow admits a URL in its range: the run gets
    // past the qa checks to the Workspace, which another Session holds, and
    // is refused at once, naming that Session.
    let message = conflict(admit(request("http://192.168.1.5:8766", "cidr")).await);
    assert!(message.contains(&holder.id), "{message}");
    assert!(
        message.contains(&repo.path().canonicalize().unwrap().display().to_string()),
        "{message}"
    );
    // Another port of the range is not admitted.
    let message = usage(admit(request("http://192.168.1.5:8767", "cidr-port")).await);
    assert!(message.contains("browser.allow"), "{message}");

    // A host the lists do not name is a usage error, until `axocoatl network
    // reload` adds it: admission reads the lists in force, not the ones the
    // daemon started with.
    let message = usage(admit(request("http://shop.example.test:8080", "host-before")).await);
    assert!(message.contains("browser.allow"), "{message}");
    std::fs::write(
        &path,
        admission_yaml(
            &server.uri(),
            "{cidr: 192.168.1.0/24, ports: [8766]}, {host: shop.example.test, ports: [8080]}",
        ),
    )
    .unwrap();
    daemon.reload_network_policy().await.unwrap();
    let message = conflict(admit(request("http://shop.example.test:8080", "host-after")).await);
    assert!(message.contains(&holder.id), "{message}");

    // A reproductions directory that is a link is refused before anything
    // is created.
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), repo.path().join("axocoatl-qa")).unwrap();
    let message = usage(admit(request("http://192.168.1.5:8766", "linked")).await);
    assert!(message.contains("symbolic link"), "{message}");
    std::fs::remove_file(repo.path().join("axocoatl-qa")).unwrap();

    // Nothing was admitted: no run record exists.
    assert!(daemon.list_loadout_runs().await.unwrap().is_empty());
    drop(held);
    daemon.shutdown().await.unwrap();
}

/// Run `name` in a child process with its own data root and a fake Podman
/// that reports `rootless`.
async fn run_child(name: &str, rootless: bool) {
    let root = tempfile::tempdir().unwrap();
    let bin = root.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let podman = bin.join("podman");
    std::fs::write(
        &podman,
        format!(
            r#"#!/bin/sh
case "$*" in
  --version) printf 'podman version 5.0.0\n' ;;
  'machine list --format json') printf '[{{"Running":true}}]\n' ;;
  'info --format json') printf '{{}}\n' ;;
  'info --format {{{{.Host.Security.Rootless}}}}') printf '{rootless}\n' ;;
  'ps '*) ;;
  'rm '*|'volume rm '*|'network rm '*) ;;
  *) printf 'unexpected Podman command: %s\n' "$*" >&2; exit 1 ;;
esac
"#
        ),
    )
    .unwrap();
    std::fs::set_permissions(&podman, std::fs::Permissions::from_mode(0o700)).unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(120),
        tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env(CHILD, "1")
            .env("AXOCOATL_DATA_DIR", root.path().join("data"))
            .env("AXOCOATL_SOCKET_PATH", root.path().join("ipc/daemon.sock"))
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .current_dir(root.path())
            .kill_on_drop(true)
            .output(),
    )
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

#[tokio::test]
async fn loadout_sessions_bind_their_sandbox_apply_inline_teams_and_refuse_rootful_podman() {
    if std::env::var_os(CHILD).is_some() {
        team_child_body().await;
        return;
    }
    run_child(
        "bootstrap::loadout_runs::daemon_tests::loadout_sessions_bind_their_sandbox_apply_inline_teams_and_refuse_rootful_podman",
        false,
    )
    .await;
}

/// qa admission against the live lists (a cidr entry, a reloaded host) and
/// a Workspace another Session holds: refused at once with that Session
/// named, never waiting.
#[tokio::test]
async fn qa_admission_reads_the_live_lists_and_never_waits_for_a_held_workspace() {
    if std::env::var_os(CHILD).is_some() {
        admission_child_body().await;
        return;
    }
    run_child(
        "bootstrap::loadout_runs::daemon_tests::qa_admission_reads_the_live_lists_and_never_waits_for_a_held_workspace",
        true,
    )
    .await;
}

#[tokio::test]
async fn loadout_runs_refuse_the_e2b_backend() {
    if std::env::var_os(CHILD).is_some() {
        e2b_child_body().await;
        return;
    }
    run_child(
        "bootstrap::loadout_runs::daemon_tests::loadout_runs_refuse_the_e2b_backend",
        true,
    )
    .await;
}

/// The Session of a loadout runs its container under egress with the
/// hardened workload although the daemon's `sandbox.network` is `bridge`;
/// a Session without a loadout keeps `bridge`. Real Podman:
///
/// ```text
/// CONTAINER_CONNECTION=axocoatl-ci-pr74 cargo test -p axocoatl-daemon --lib \
///   loadout_session_container_runs_egress_and_hardened -- --ignored
/// ```
#[tokio::test]
#[ignore = "requires rootless Podman and the egress sidecar image"]
async fn loadout_session_container_runs_egress_and_hardened() {
    if std::env::var_os(CHILD).is_none() {
        let root = tempfile::tempdir().unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(600),
            tokio::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "bootstrap::loadout_runs::daemon_tests::loadout_session_container_runs_egress_and_hardened",
                    "--nocapture",
                    "--ignored",
                ])
                .env(CHILD, "1")
                .env("AXOCOATL_DATA_DIR", root.path().join("data"))
                .env("AXOCOATL_SOCKET_PATH", root.path().join("ipc/daemon.sock"))
                .current_dir(root.path())
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            result.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        return;
    }
    let server = model_server().await;
    let mut config = config(&server.uri(), "podman");
    config.sandbox.network = "bridge".into();
    let daemon = AxocoatlDaemon::bootstrap_headless(config).await.unwrap();
    // Workspaces outside the data root, which the container must not reach.
    let outside = tempfile::Builder::new()
        .prefix("axocoatl-loadout-podman-")
        .tempdir_in(std::env::temp_dir())
        .unwrap();
    let bound_dir = outside.path().join("bound");
    let plain_dir = outside.path().join("plain");
    std::fs::create_dir_all(&bound_dir).unwrap();
    std::fs::create_dir_all(&plain_dir).unwrap();
    let mut run_binding = binding("egress");
    run_binding.run_id = format!("run-{}", uuid::Uuid::new_v4());
    let bound = native_session(&daemon, &bound_dir, Some(run_binding)).await;
    daemon
        .egress_points
        .set_loadout_overlay(&bound.id, Default::default());
    let plain = native_session(&daemon, &plain_dir, None).await;
    // Whatever an assertion does, the containers, volumes and networks of
    // both Sessions are removed.
    struct Cleanup(Vec<String>);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            for id in &self.0 {
                let names: Vec<String> =
                    ["axo-ses-", "axo-egr-", "axo-brw-", "axo-pvw-", "axo-svc-"]
                        .iter()
                        .map(|prefix| format!("{prefix}{id}"))
                        .collect();
                let _ = std::process::Command::new("podman")
                    .args(["rm", "-f", "--ignore"])
                    .args(&names)
                    .output();
                for kind in ["volume", "network"] {
                    let listed = std::process::Command::new("podman")
                        .args([kind, "ls", "--format", "{{.Name}}"])
                        .output();
                    if let Ok(listed) = listed {
                        for name in String::from_utf8_lossy(&listed.stdout)
                            .lines()
                            .filter(|name| name.ends_with(id.as_str()))
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
    let _cleanup = Cleanup(vec![bound.id.clone(), plain.id.clone()]);
    let inspect = |name: String, format: &'static str| async move {
        let output = tokio::process::Command::new("podman")
            .args(["inspect", "--format", format, &name])
            .output()
            .await
            .unwrap();
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };
    let result = async {
        let ready = daemon.prepare_session_environment(&bound).await?;
        assert_eq!(
            ready.environment.state,
            SessionEnvironmentState::Ready,
            "{:?}",
            ready.environment.error
        );
        let sandbox = daemon
            .session_sandboxes
            .lock()
            .await
            .get(&bound.id)
            .cloned()
            .unwrap();
        // Commands run as the non-root writer user, under the egress sidecar.
        let id = sandbox
            .exec(&["id", "-u"], Duration::from_secs(30))
            .await
            .map_err(|error| DaemonError::Session(error.to_string()))?;
        assert_ne!(id.stdout.trim(), "0", "{id:?}");
        assert!(sandbox.egress_status().is_some(), "an egress sidecar runs");
        // Its container reaches nothing but the sidecar: no bridge network.
        let mode = inspect(
            format!("axo-ses-{}", bound.id),
            "{{.HostConfig.NetworkMode}}",
        )
        .await;
        assert!(!mode.is_empty() && mode != "bridge", "{mode}");
        let user = inspect(format!("axo-ses-{}", bound.id), "{{.Config.User}}").await;
        let sidecar = inspect(format!("axo-egr-{}", bound.id), "{{.State.Running}}").await;
        assert_eq!(sidecar, "true", "the Session's egress sidecar runs");
        tracing::info!(%mode, %user, "loadout Session container");
        let plain_ready = daemon.prepare_session_environment(&plain).await?;
        assert_eq!(
            plain_ready.environment.state,
            SessionEnvironmentState::Ready,
            "{:?}",
            plain_ready.environment.error
        );
        let plain_sandbox = daemon
            .session_sandboxes
            .lock()
            .await
            .get(&plain.id)
            .cloned()
            .unwrap();
        assert!(plain_sandbox.egress_status().is_none());
        let plain_mode = inspect(
            format!("axo-ses-{}", plain.id),
            "{{.HostConfig.NetworkMode}}",
        )
        .await;
        assert_eq!(plain_mode, "bridge", "an unbound Session keeps bridge");
        let plain_id = plain_sandbox
            .exec(&["id", "-u"], Duration::from_secs(30))
            .await
            .map_err(|error| DaemonError::Session(error.to_string()))?;
        assert!(plain_id.exit_code == 0, "{plain_id:?}");
        Ok::<(), DaemonError>(())
    }
    .await;
    let cleanup = daemon.shutdown_session_runtimes_checked().await;
    result.unwrap();
    cleanup.unwrap();
}
