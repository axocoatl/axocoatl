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
    crate::ollama_contract_test_support::mount_contract_probes(&server).await;
    let digest = "a80c4f17acd55265feec403c7aef86be0c25983ab279d83f3bcd3abbcb5b8b72";
    for (verb, route, body) in [
        (
            "GET",
            "/api/version",
            serde_json::json!({"version": axocoatl_llm_ollama::AUDITED_OLLAMA_VERSION}),
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
                    "details": {"format": "gguf"}, "context_length": 32768, "runner": "llamacpp"},
                {"name": OTHER, "model": OTHER, "digest": digest,
                    "details": {"format": "gguf"}, "context_length": 32768, "runner": "llamacpp"}]}),
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
            credentials: Default::default(),
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
            ("reviewer_model".to_string(), format!("ollama:{OTHER}")),
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

/// The configuration file of the admission test, with `browser.allow`, and
/// native OpenRouter configured (a test key no request ever sends: the
/// tests read OpenRouter's catalog from a mock).
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
  openrouter:
    api_key: test-only-key
  openrouter_billing: credits
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
    // A loadout whose writer's tokens budget is valid on its own but cannot
    // hold one call of the model's 32768-token context plus its 8192-token
    // output bound.
    std::fs::create_dir(dir.path().join(USER_LOADOUT_DIR)).unwrap();
    std::fs::write(
        dir.path().join(USER_LOADOUT_DIR).join("tight.yaml"),
        NATIVE_RUN
            .replace("id: native-run", "id: tight")
            .replace("tokens: 100000", "tokens: 40000"),
    )
    .unwrap();
    // The same budget shape for a native OpenRouter writer, and a Codex
    // writer whose budget holds less than one call of its pinned model.
    std::fs::write(
        dir.path()
            .join(USER_LOADOUT_DIR)
            .join("tight-openrouter.yaml"),
        NATIVE_RUN
            .replace("id: native-run", "id: tight-openrouter")
            .replace("tokens: 100000", "tokens: 20000"),
    )
    .unwrap();
    std::fs::write(
        dir.path().join(USER_LOADOUT_DIR).join("tight-codex.yaml"),
        NATIVE_RUN
            .replace("id: native-run", "id: tight-codex")
            .replace("    tools: [read_file]\n", "    runtime: codex\n")
            .replace("tokens: 100000", "tokens: 300000"),
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
        Err(DaemonError::WorkspaceBusy(message)) => message,
        Err(other) => panic!("expected a busy Workspace, got {other}"),
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

    // A tokens budget below one call of the model's observed context is a
    // usage error (exit 3) naming the budget and the minimum, before the
    // held Workspace is even asked for.
    let message = usage(
        admit(RunRequest {
            loadout: "tight".into(),
            task: "Read README.md.".into(),
            repo: repo.path().display().to_string(),
            params: [("writer_model".to_string(), format!("ollama:{MODEL}"))]
                .into_iter()
                .collect(),
            keep: Default::default(),
            check_command: None,
            setup_command: None,
            request_id: "tight".into(),
        })
        .await,
    );
    assert_eq!(
        message,
        format!(
            "loadout field budgets.agent.tokens: 40000 tokens is less than one model call of \
             Agent writer (ollama:{MODEL}) needs: the model's 32768-token context plus 8192 \
             output tokens (agents.writer.max_output_tokens), at least 40960 tokens; raise it \
             to at least 40960 or lower agents.writer.max_output_tokens"
        )
    );

    // A native OpenRouter writer's floor comes from OpenRouter's public
    // catalog, read once: the smallest call Team & budget would check, its
    // prompt allowance, output bound and reasoning allowance at the model's
    // default effort, never the 131072-token window.
    let catalog = MockServer::start().await;
    Mock::given(method("GET"))
        .and(wiremock::matchers::path("/models"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"data": [
                {"id": "openai/gpt-oss-120b", "context_length": 131072,
                 "reasoning": {"mandatory": false, "default_enabled": true,
                     "default_effort": "medium", "supported_efforts": ["high", "medium", "low"]}}
            ]})),
        )
        .expect(1)
        .mount(&catalog)
        .await;
    *daemon.loadout_runs.openrouter_catalog.lock().unwrap() = Some(catalog.uri());
    let run = |loadout: &str, model: &str, id: &str| RunRequest {
        loadout: loadout.into(),
        task: "Read README.md.".into(),
        repo: repo.path().display().to_string(),
        params: [("writer_model".to_string(), model.to_string())]
            .into_iter()
            .collect(),
        keep: Default::default(),
        check_command: None,
        setup_command: None,
        request_id: id.into(),
    };
    let message = usage(
        admit(run(
            "tight-openrouter",
            "openrouter:openai/gpt-oss-120b",
            "tight-openrouter",
        ))
        .await,
    );
    assert_eq!(
        message,
        "loadout field budgets.agent.tokens: 20000 tokens is less than one model call of Agent \
         writer (openrouter:openai/gpt-oss-120b) needs: the 4096-token prompt allowance a native \
         OpenRouter call reserves at least (each call reserves its own request, within the \
         model's 131072-token context in OpenRouter's catalog) plus 8192 output tokens \
         (agents.writer.max_output_tokens) and 8192 reasoning tokens (reasoning effort medium), \
         at least 20480 tokens; raise it to at least 20480 or lower \
         agents.writer.max_output_tokens or set agents.writer.reasoning_effort lower"
    );
    catalog.verify().await;

    // A Codex writer's floor is one call of its pinned model. (Its
    // credential, codex-openai, is stored as a test value: a missing one
    // would be reported too.)
    crate::secret_store::set_secret(daemon.data_root.path(), "codex-openai", b"test-api-key")
        .unwrap();
    let message = usage(admit(run("tight-codex", "openai:gpt-5.5", "tight-codex")).await);
    assert_eq!(
        message,
        "loadout field budgets.agent.tokens: 300000 tokens is less than one model call of Agent \
         writer (openai:gpt-5.5) needs: the 272000-token context Codex 0.160.1 keeps for gpt-5.5 \
         plus the model's 128000 output tokens, at least 400000 tokens; raise it to at least \
         400000"
    );
    // An external writer's model names the provider its program calls.
    let message = usage(
        admit(run(
            "tight-codex",
            "openrouter:openai/gpt-5.5",
            "codex-on-openrouter",
        ))
        .await,
    );
    assert_eq!(
        message,
        "loadout parameter writer_model: a codex writer calls openai's model API, so its model \
         is openai:<the program's model name>; openrouter:openai/gpt-5.5 names the provider \
         openrouter"
    );

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

/// The built-in loadouts `fix-claude-code` and `fix-codex` through
/// admission, with a fake rootless Podman and test secrets in the child's
/// own data root: the writer's runtime, default model and recipe image, the
/// credential its route sends (fix-codex's `auto` with each combination of
/// stored secrets, and `--param credential=`), the routes, the team, the
/// record of the choice (manifest, note event, Outcome notes), the
/// same-model warning, and a user file that declares a built-in id.
async fn external_fix_admission_child_body() {
    use crate::loadout::{credentials, egress, team_plan};
    let server = model_server().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("axocoatl.yaml");
    std::fs::write(
        &path,
        admission_yaml(&server.uri(), "{host: localhost, ports: [3000]}"),
    )
    .unwrap();
    std::fs::create_dir(dir.path().join(USER_LOADOUT_DIR)).unwrap();
    let config = axocoatl_config::load_config(&path).await.unwrap();
    let daemon = AxocoatlDaemon::bootstrap_headless(config).await.unwrap();
    daemon.set_config_path(&path);
    let data = daemon.data_root.path().to_path_buf();
    let base = daemon.network_policy.current();

    // Listed and shown like the other built-ins, with their defaults.
    let listed = daemon.list_loadouts().await.unwrap();
    for id in ["fix", "fix-claude-code", "fix-codex", "qa", "audit"] {
        let row = listed.iter().find(|row| row.id == id).unwrap();
        assert!(row.builtin && row.error.is_none(), "{row:?}");
        assert!(row.warnings.is_empty(), "{row:?}");
    }
    let codex_row = listed.iter().find(|row| row.id == "fix-codex").unwrap();
    let param = |name: &str| {
        codex_row
            .params
            .iter()
            .find(|param| param.name == name)
            .unwrap()
            .clone()
    };
    assert_eq!(
        param("writer_model").default.as_deref(),
        Some("openai:gpt-6.1-sol")
    );
    assert!(!param("writer_model").required);
    assert!(param("reviewer_model").required);
    assert_eq!(param("credential").default.as_deref(), Some("auto"));
    let view = daemon.loadout_view("fix-codex").await.unwrap();
    let writer = &view.graph.nodes[0].detail;
    assert_eq!(
        writer[..3],
        [
            "param writer_model (default openai:gpt-6.1-sol)".to_string(),
            "runtime codex".into(),
            "credential param credential (default auto)".into()
        ]
    );
    let view = daemon.loadout_view("fix-claude-code").await.unwrap();
    assert_eq!(
        view.graph.nodes[0].detail[..2],
        [
            "param writer_model (default anthropic:sonnet)".to_string(),
            "runtime claude-code".into()
        ]
    );

    let repo = tempfile::tempdir().unwrap();
    let holder = native_session(&daemon, repo.path(), None).await;
    let operation = daemon
        .attempt_operation_for_workspace(&holder.workspace_id)
        .await;
    let held = operation.clone().lock_owned().await;
    let request = |loadout: &str, params: &[(&str, &str)], id: &str| RunRequest {
        loadout: loadout.into(),
        task: "Fix the failing test.".into(),
        repo: repo.path().display().to_string(),
        params: std::iter::once(("reviewer_model".to_string(), format!("ollama:{MODEL}")))
            .chain(
                params
                    .iter()
                    .map(|(name, value)| (name.to_string(), value.to_string())),
            )
            .collect(),
        keep: Default::default(),
        check_command: Some("true".into()),
        setup_command: None,
        request_id: id.into(),
    };
    let admit = |request: RunRequest| {
        let daemon = &daemon;
        async move {
            tokio::time::timeout(Duration::from_secs(60), daemon.admit_loadout_run(request))
                .await
                .expect("admission never waits")
        }
    };
    let usage = |result: Result<(RunAccepted, RunContext), DaemonError>| match result {
        Err(DaemonError::InvalidRequest(message)) => message,
        Err(other) => panic!("expected a usage error, got {other}"),
        Ok((accepted, _)) => panic!("admitted {}", accepted.run_id),
    };
    let busy = |result: Result<(RunAccepted, RunContext), DaemonError>| match result {
        Err(DaemonError::WorkspaceBusy(message)) => message,
        Err(other) => panic!("expected a busy Workspace, got {other}"),
        Ok((accepted, _)) => panic!("admitted {}", accepted.run_id),
    };
    // The steps admission takes for a built-in, by themselves.
    let resolved = |id: &str, params: &[(&str, &str)]| {
        let loadout = builtin_loadouts()
            .into_iter()
            .map(Result::unwrap)
            .find(|loadout| loadout.file.id == id)
            .unwrap();
        let request = request(id, params, "steps");
        let mut resolved =
            resolve_loadout(&loadout, &request.params, &request.task, "/repo").unwrap();
        team_plan::fill_detected_checks(&mut resolved, Some("true"), None).unwrap();
        let chosen = credentials::choose(&mut resolved, |name| {
            egress::credential_available(&base, &data, name)
        })
        .unwrap();
        (resolved, chosen)
    };
    let hosts = |resolved: &axocoatl_config::loadout::ResolvedLoadout| -> Vec<String> {
        egress::loadout_overlay(&base, resolved, &data)
            .unwrap()
            .routes
            .into_iter()
            .map(|route| route.host)
            .collect()
    };

    // fix-codex with nothing stored: auto finds no credential and says how
    // to connect, and the codex image is not built either: both at once,
    // before anything is created.
    let message = usage(admit(request("fix-codex", &[], "codex-none")).await);
    assert_eq!(
        message,
        "2 problems stop this run, and nothing was started:\n  1. Agent writer (codex) has \
         credential auto, and neither codex-openai nor codex-chatgpt is stored: run `axocoatl \
         connect codex` to sign in with your ChatGPT plan (stored as codex-chatgpt), or \
         `axocoatl connect codex --api-key` to also store the OpenAI API key OpenAI issues \
         with that sign-in (as codex-openai), then run again\n  2. recipe images: no image is \
         built from the recipes codex; run `axocoatl recipe build codex`"
    );
    // A credential that is none of Codex's is a parameter error, reported
    // with the image that is not built.
    let message = usage(admit(request("fix-codex", &[("credential", "codex")], "bogus")).await);
    assert!(
        message.contains(
            "\n  1. loadout parameter credential: a codex writer's route sends codex-openai or \
             codex-chatgpt, or auto"
        ) && message.contains("\n  2. recipe images: no image is built"),
        "{message}"
    );
    // Named but not stored: the route's own refusal, with how to store it.
    let message = usage(
        admit(request(
            "fix-codex",
            &[("credential", "codex-openai")],
            "named-none",
        ))
        .await,
    );
    assert!(
        message.contains("credential \"codex-openai\"")
            && message.contains("axocoatl connect codex --api-key"),
        "{message}"
    );

    // Only the ChatGPT sign-in is stored (a test value, never a real one):
    // auto takes it. Without the codex recipe image the run is refused,
    // naming the build.
    crate::secret_store::set_secret(&data, "codex-chatgpt", b"test-sign-in").unwrap();
    match admit(request("fix-codex", &[], "codex-no-image")).await {
        Err(DaemonError::Session(message)) => assert!(
            message.contains("run `axocoatl recipe build codex`"),
            "{message}"
        ),
        Err(other) => panic!("expected the missing image, got {other}"),
        Ok((accepted, _)) => panic!("admitted {}", accepted.run_id),
    }
    for recipe in ["codex", "claude-code"] {
        crate::external_agent::recipe_images::record_image(
            &daemon.data_root,
            crate::external_agent::recipe_images::record_for(
                &[recipe.to_string()],
                &format!("{:0>64}", if recipe == "codex" { "c0de" } else { "c1a0de" }),
                1,
            )
            .unwrap(),
        )
        .unwrap();
    }
    // Everything resolves: the run reaches the Workspace, which another
    // Session holds.
    let message = busy(admit(request("fix-codex", &[], "codex-chatgpt-auto")).await);
    assert!(message.contains(&holder.id), "{message}");
    let (codex, chosen) = resolved("fix-codex", &[]);
    assert_eq!(chosen["writer"], "codex-chatgpt");
    assert_eq!(codex.agent_credentials["writer"], "codex-chatgpt");
    assert_eq!(hosts(&codex), ["chatgpt.com"]);
    assert_eq!(
        codex.notes,
        [
            "Agent writer (codex) sends the stored credential codex-chatgpt (a ChatGPT sign-in, \
          sent to chatgpt.com); credential auto chose it, the first of codex-openai and \
          codex-chatgpt that is stored."
        ]
    );
    assert_eq!(
        codex.agent_models["writer"],
        axocoatl_config::loadout::ModelSpec::parse("openai:gpt-6.1-sol").unwrap()
    );
    // The writer's slot runs Codex on the default model with the chosen
    // credential; the checks and the three-round review are fix's.
    let edit =
        team_plan::team_edit(&codex, &team_plan::default_slots(&codex).unwrap(), true, 0).unwrap();
    assert_eq!(edit.slots.len(), 1);
    assert_eq!(
        (
            edit.slots[0].provider.as_str(),
            edit.slots[0].model.as_str()
        ),
        ("openai", "gpt-6.1-sol")
    );
    let definition = edit.slots[0].definition.as_ref().unwrap();
    assert_eq!(definition.runtime, AgentRuntime::Codex);
    assert_eq!(definition.credential.as_deref(), Some("codex-chatgpt"));
    assert_eq!(
        edit.required_checks,
        [vec!["sh".to_string(), "-c".into(), "true".into()]]
    );
    let review = edit.required_review.as_ref().unwrap();
    assert_eq!(review.max_rounds, 3);
    // Both stored: auto takes the API key; --param chooses either.
    crate::secret_store::set_secret(&data, "codex-openai", b"test-api-key").unwrap();
    let (codex, chosen) = resolved("fix-codex", &[]);
    assert_eq!(chosen["writer"], "codex-openai");
    assert_eq!(hosts(&codex), ["api.openai.com"]);
    let (codex, chosen) = resolved("fix-codex", &[("credential", "codex-chatgpt")]);
    assert_eq!(chosen["writer"], "codex-chatgpt");
    assert_eq!(hosts(&codex), ["chatgpt.com"]);
    assert_eq!(
        codex.notes,
        [
            "Agent writer (codex) sends the stored credential codex-chatgpt (a ChatGPT sign-in, \
          sent to chatgpt.com)."
        ]
    );
    // Only the API key: auto takes it.
    crate::secret_store::remove_secret(&data, "codex-chatgpt").unwrap();
    let (codex, chosen) = resolved("fix-codex", &[]);
    assert_eq!(chosen["writer"], "codex-openai");
    assert_eq!(hosts(&codex), ["api.openai.com"]);
    let message = busy(admit(request("fix-codex", &[], "codex-openai-auto")).await);
    assert!(message.contains(&holder.id), "{message}");

    // fix-claude-code: Claude Code's token must be stored.
    let message = usage(admit(request("fix-claude-code", &[], "claude-none")).await);
    assert!(
        message.contains("credential \"claude-code-oauth\"")
            && message.contains("axocoatl connect claude-code"),
        "{message}"
    );
    crate::secret_store::set_secret(&data, "claude-code-oauth", b"test-oauth-token").unwrap();
    let message = busy(admit(request("fix-claude-code", &[], "claude-stored")).await);
    assert!(message.contains(&holder.id), "{message}");
    let (claude, chosen) = resolved("fix-claude-code", &[]);
    assert_eq!(chosen["writer"], "claude-code-oauth");
    assert_eq!(hosts(&claude), ["api.anthropic.com"]);
    assert_eq!(
        claude.agent_models["writer"],
        axocoatl_config::loadout::ModelSpec::parse("anthropic:sonnet").unwrap()
    );
    // The writer's tokens hold one call of the default model (sonnet runs
    // claude-sonnet-5-5, whose 1M-token window one call can fill), and of
    // any other pinned model.
    for model in [
        "anthropic:sonnet",
        "anthropic:opus",
        "anthropic:claude-fable-5-1",
        "anthropic:haiku",
    ] {
        let (claude, _) = resolved("fix-claude-code", &[("writer_model", model)]);
        team_plan::refuse_budgets_below_one_call(&claude, &Default::default()).unwrap();
    }
    for model in ["openai:gpt-6.1-sol", "openai:gpt-6-astra", "openai:gpt-5.5"] {
        let (codex, _) = resolved("fix-codex", &[("writer_model", model)]);
        team_plan::refuse_budgets_below_one_call(&codex, &Default::default()).unwrap();
    }

    // Without the held Workspace the run is admitted: its environment
    // cannot start under the fake Podman, so it ends at once (exit 5), with
    // its manifest, its note event and its Outcome recording the credential
    // that was chosen.
    drop(held);
    let (accepted, _) = admit(request("fix-codex", &[], "codex-admitted"))
        .await
        .unwrap();
    let store = daemon.loadout_runs.store().unwrap();
    let manifest = store.manifest(&accepted.run_id).unwrap();
    assert_eq!(manifest.loadout.id, "fix-codex");
    assert!(manifest.loadout.builtin);
    assert_eq!(manifest.params["credential"], "auto");
    assert_eq!(manifest.params["writer_model"], "openai:gpt-6.1-sol");
    assert_eq!(
        manifest.credentials,
        [("writer".to_string(), "codex-openai".to_string())]
            .into_iter()
            .collect()
    );
    let note = "Agent writer (codex) sends the stored credential codex-openai (an OpenAI API \
                key, sent to api.openai.com); credential auto chose it, the first of \
                codex-openai and codex-chatgpt that is stored.";
    let events = daemon
        .loadout_run_events(&accepted.run_id, None, 100)
        .await
        .unwrap();
    assert!(
        events.events.iter().any(|(_, event)| matches!(event,
            RunEvent::Phase { phase, detail, .. } if phase == "note" && detail == note)),
        "{events:?}"
    );
    let status = daemon.loadout_run(&accepted.run_id).await.unwrap();
    let outcome = status.outcome.expect("the run ended at admission");
    assert_eq!(outcome.exit_code, exit_code::INFRASTRUCTURE, "{outcome:?}");
    assert_eq!(outcome.notes, [note], "{outcome:?}");
    let junit = daemon.loadout_run_junit(&accepted.run_id).await.unwrap();
    assert!(
        junit.contains(&format!("note: {}", note.replace('\'', "&apos;"))),
        "{junit}"
    );

    // A reviewer on the writer's model, through OpenRouter, is warned
    // about at admission (Claude Code's sonnet is claude-sonnet-5-5).
    let catalog = MockServer::start().await;
    Mock::given(method("GET"))
        .and(wiremock::matchers::path("/models"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"data": [
                {"id": "anthropic/claude-sonnet-5.5", "context_length": 1000000}
            ]})),
        )
        .mount(&catalog)
        .await;
    *daemon.loadout_runs.openrouter_catalog.lock().unwrap() = Some(catalog.uri());
    let mut same = request("fix-claude-code", &[], "claude-same-model");
    same.params.insert(
        "reviewer_model".into(),
        "openrouter:anthropic/claude-sonnet-5.5".into(),
    );
    let (accepted, _) = admit(same).await.unwrap();
    assert!(
        accepted
            .warnings
            .iter()
            .any(|warning| warning.code == "same_model_reviewer"),
        "{:?}",
        accepted.warnings
    );

    // A user file that declares a built-in id (the docs once showed one
    // named fix-claude-code) is listed with why it is not used, the
    // built-in carries a warning, the built-in is still shown, and a run of
    // that id is refused: neither one is chosen silently.
    let mine = dir.path().join(USER_LOADOUT_DIR).join("claude.yaml");
    std::fs::write(
        &mine,
        builtin_loadouts()
            .into_iter()
            .map(Result::unwrap)
            .find(|loadout| loadout.file.id == "fix-claude-code")
            .unwrap()
            .text
            .replace("rounds: 3", "rounds: 2"),
    )
    .unwrap();
    let listed = daemon.list_loadouts().await.unwrap();
    let user_row = listed.iter().find(|row| !row.builtin).unwrap();
    // The file as the daemon names it (its configuration directory, as it
    // resolved it).
    let shown = user_row.path.clone().unwrap();
    assert!(shown.ends_with("/loadouts/claude.yaml"), "{shown}");
    let builtin_row = listed
        .iter()
        .find(|row| row.id == "fix-claude-code" && row.builtin)
        .unwrap();
    assert_eq!(builtin_row.warnings.len(), 1, "{builtin_row:?}");
    assert_eq!(builtin_row.warnings[0].code, "user_loadout_uses_builtin_id");
    assert_eq!(
        builtin_row.warnings[0].message,
        format!(
            "your loadout file {shown} also declares the id fix-claude-code, which is now \
             built in: a run of fix-claude-code is refused until you give that file another id \
             (such as my-fix-claude-code) or remove it"
        )
    );
    assert_eq!(
        user_row.error.as_deref(),
        Some(
            "\"fix-claude-code\" is now the id of a built-in loadout, so this file is not \
             used: to keep running it, give it another id (such as my-fix-claude-code) and run \
             that id; to use the built-in, remove this file"
        )
    );
    let view = daemon.loadout_view("fix-claude-code").await.unwrap();
    assert!(view.summary.builtin);
    assert_eq!(view.summary.warnings, builtin_row.warnings);
    let message = usage(admit(request("fix-claude-code", &[], "claude-clash")).await);
    assert_eq!(
        message,
        format!(
            "loadout fix-claude-code is now built into Axocoatl, and your loadout file {shown} \
             also declares the id fix-claude-code, so nothing was started: Axocoatl does not \
             choose between the two. To run your file, give it another id (such as \
             my-fix-claude-code) and run that id; to run the built-in, remove {shown}",
            shown = shown,
        )
    );
    // Renamed, it runs as itself and the built-in runs again.
    let renamed = std::fs::read_to_string(&mine)
        .unwrap()
        .replace("id: fix-claude-code", "id: my-fix-claude-code");
    std::fs::write(&mine, renamed).unwrap();
    let listed = daemon.list_loadouts().await.unwrap();
    assert!(
        listed
            .iter()
            .all(|row| row.error.is_none() && row.warnings.is_empty()),
        "{listed:?}"
    );
    assert!(listed
        .iter()
        .any(|row| row.id == "my-fix-claude-code" && !row.builtin));
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

/// `git` in `dir` with no configuration of the person's.
fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Setup")
        .env("GIT_AUTHOR_EMAIL", "setup@example.invalid")
        .env("GIT_COMMITTER_NAME", "Setup")
        .env("GIT_COMMITTER_EMAIL", "setup@example.invalid")
        .output()
        .unwrap();
    assert!(output.status.success(), "{args:?}: {output:?}");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// A run of `session` in `repo`, admitted as admission writes it.
fn receipt_manifest(session: &Session, repo: &std::path::Path, head: &str) -> RunManifest {
    let loadout = parse_loadout(NATIVE_RUN, LoadoutSource::Builtin).unwrap();
    RunManifest {
        schema: RUN_MANIFEST_SCHEMA.into(),
        run_id: format!("run-{}", uuid::Uuid::new_v4()),
        session_id: session.id.clone(),
        workspace_id: session.workspace_id.clone(),
        loadout: LoadoutRef {
            id: loadout.file.id.clone(),
            version: loadout.file.version,
            kind: loadout.file.kind.to_string(),
            digest: loadout.digest.clone(),
            builtin: false,
        },
        loadout_text: NATIVE_RUN.into(),
        params: [("writer_model".to_string(), format!("ollama:{MODEL}"))]
            .into_iter()
            .collect(),
        credentials: Default::default(),
        task: "Read README.md and say what it is.".into(),
        repo: repo.display().to_string(),
        repo_head: Some(head.to_string()),
        dirty_paths: Vec::new(),
        started_at_ms: now_ms(),
        options: serde_json::json!({}),
    }
}

async fn receipts_child_body() {
    use axocoatl_session::receipt::{check_signature, open_receipt, PublicKey, SignatureCheck};
    let server = model_server().await;
    let daemon = AxocoatlDaemon::bootstrap_headless(config(&server.uri(), "podman"))
        .await
        .unwrap();
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().canonicalize().unwrap();
    std::fs::write(repo.join("README.md"), "# Receipts\n").unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["add", "README.md"]);
    git(&repo, &["commit", "-q", "-m", "initial"]);
    // Credentials in the remote's URL never reach a receipt.
    git(
        &repo,
        &[
            "remote",
            "add",
            "origin",
            "https://person:not-a-real-token@github.com/acme/widgets.git",
        ],
    );
    let head = git(&repo, &["rev-parse", "HEAD"]);
    let session = native_session(&daemon, &repo, Some(binding("egress"))).await;
    let store = daemon.loadout_runs.store().unwrap();

    // A finished run, whatever its verdict, is signed before its Outcome
    // is written; the receipt is served only once the run has finished.
    let manifest = receipt_manifest(&session, &repo, &head);
    let run_id = manifest.run_id.clone();
    store.create(&manifest).unwrap();
    let outcome = ended_outcome(&manifest, "the environment failed".into(), Vec::new());
    daemon.seal_loadout_run(&run_id, &outcome).await.unwrap();
    assert!(matches!(
        daemon.loadout_run_receipt(&run_id),
        Err(DaemonError::NotFound(message)) if message.contains("has not finished")
    ));
    daemon.finish_loadout_run(&run_id, &outcome).unwrap();
    let bytes = daemon.loadout_run_receipt(&run_id).unwrap();
    let key = daemon.receipt_public_key().await.unwrap();
    assert_eq!(key.algorithm, "ed25519");
    assert!(key
        .line
        .starts_with(&format!("ed25519 {} axocoatl@", key.public_key)));
    let public = PublicKey::from_base64(&key.public_key).unwrap();
    assert_eq!(public.key_id(), key.key_id);
    let opened = open_receipt(&bytes).unwrap();
    assert_eq!(
        check_signature(&opened, &[public]),
        SignatureCheck::Trusted {
            key_id: key.key_id.clone()
        }
    );
    let predicate = &opened.statement.predicate;
    assert_eq!(predicate.run.run_id, run_id);
    assert_eq!(predicate.verdict, RunVerdict::Error);
    assert_eq!(predicate.exit_code, exit_code::INFRASTRUCTURE);
    assert_eq!(predicate.loadout.id, "native-run");
    assert_eq!(predicate.agents.len(), 1);
    assert_eq!(predicate.agents[0].provider, "ollama");
    assert_eq!(predicate.agents[0].model, MODEL);
    assert_eq!(predicate.agents[0].runtime, "native");
    // No call of the model ran, so no Ollama served it.
    assert_eq!(predicate.agents[0].program_version, None);
    assert_eq!(
        predicate.repository.base_commit.as_deref(),
        Some(head.as_str())
    );
    assert_eq!(
        predicate.repository.origin_url.as_deref(),
        Some("https://github.com/acme/widgets.git")
    );
    assert!(!String::from_utf8_lossy(&opened.payload).contains("not-a-real-token"));
    assert_eq!(predicate.sandbox.network, "egress");
    assert_eq!(predicate.sandbox.backend, "podman");
    assert_eq!(
        predicate.run.task_sha256,
        axocoatl_session::receipt::sha256_hex(manifest.task.as_bytes())
    );
    // No turn ran: nothing is attributed, and the list says where it was read.
    assert!(opened.statement.subject.is_empty());
    assert_eq!(predicate.files.source, "workspace_at_finish");
    assert!(predicate.files.complete);
    let status = daemon.loadout_run(&run_id).await.unwrap();
    let view = status.receipt.unwrap();
    assert_eq!(view.key_id, key.key_id);
    assert_eq!(view.statement_sha256, opened.payload_digest());
    // The bundle's last section is the receipt, as written.
    let mut bundle = Vec::new();
    daemon
        .write_record_bundle(&run_id, &mut bundle)
        .await
        .unwrap();
    let summary = axocoatl_session::record_bundle::verify_bundle(bundle.as_slice()).unwrap();
    assert_eq!(
        summary.header.schema,
        axocoatl_session::record_bundle::RECORD_BUNDLE_SCHEMA
    );
    assert_eq!(
        summary.receipt,
        Some(serde_json::from_slice::<serde_json::Value>(&bytes).unwrap())
    );
    // The same key signs the next run; the receipt is written once.
    assert!(store.write_receipt(&run_id, b"{}\n").is_err());

    // A run whose model Ollama served: the Outcome records the version
    // and the receipt names it in the Agent's program version, a field the
    // 1.4.0 verifier already reads.
    let served = receipt_manifest(&session, &repo, &head);
    store.create(&served).unwrap();
    let mut outcome = ended_outcome(&served, "the environment failed".into(), Vec::new());
    outcome.ollama_versions = [
        (
            MODEL.to_string(),
            vec![axocoatl_llm_ollama::AUDITED_OLLAMA_VERSION.to_string()],
        ),
        (
            OTHER.to_string(),
            vec![
                axocoatl_llm_ollama::AUDITED_OLLAMA_VERSION.to_string(),
                "0.40.7".to_string(),
            ],
        ),
    ]
    .into_iter()
    .collect();
    outcome.review = Some(axocoatl_session::run_outcome::ReviewOutcome {
        reviewer: axocoatl_session::run_outcome::ModelIdentity {
            provider: "ollama".into(),
            model: OTHER.into(),
            runtime: "native".into(),
        },
        max_rounds: 3,
        rounds: Vec::new(),
        passed: false,
        state: "not_run".into(),
        reason: "the environment failed".into(),
    });
    daemon
        .seal_loadout_run(&served.run_id, &outcome)
        .await
        .unwrap();
    daemon.finish_loadout_run(&served.run_id, &outcome).unwrap();
    let opened = open_receipt(&daemon.loadout_run_receipt(&served.run_id).unwrap()).unwrap();
    assert_eq!(
        opened.statement.predicate.agents[0].program_version,
        Some(format!(
            "ollama {}",
            axocoatl_llm_ollama::AUDITED_OLLAMA_VERSION
        ))
    );
    assert!(axocoatl_session::receipt::describe(&opened.statement)
        .iter()
        .any(|line| line.contains(&format!(
            "native ollama {}, ollama:{MODEL}",
            axocoatl_llm_ollama::AUDITED_OLLAMA_VERSION
        ))));
    // The reviewer's Ollama, upgraded during the run, names both versions.
    assert_eq!(
        opened
            .statement
            .predicate
            .review
            .as_ref()
            .and_then(|review| review.program_version.as_deref()),
        Some(
            format!(
                "ollama {}, 0.40.7",
                axocoatl_llm_ollama::AUDITED_OLLAMA_VERSION
            )
            .as_str()
        )
    );
    let recorded: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            daemon
                .data_root
                .path()
                .join(axocoatl_session::run_record::RUN_RECORD_DIR)
                .join(&served.run_id)
                .join("outcome.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        recorded["ollama_versions"][MODEL],
        serde_json::json!([axocoatl_llm_ollama::AUDITED_OLLAMA_VERSION])
    );

    // A run the daemon restarted during: a receipt its interrupted finish
    // left is replaced by the receipt of the Outcome the restart writes.
    let restarted = receipt_manifest(&session, &repo, &head);
    store.create(&restarted).unwrap();
    store
        .append(
            &restarted.run_id,
            &RunEvent::TurnStarted {
                at_ms: now_ms(),
                turn_id: "turn-gone".into(),
                purpose: "run".into(),
            },
        )
        .unwrap();
    store
        .write_receipt(&restarted.run_id, b"{\"stale\":true}\n")
        .unwrap();
    daemon.recover_loadout_runs().await;
    let bytes = daemon.loadout_run_receipt(&restarted.run_id).unwrap();
    let opened = open_receipt(&bytes).unwrap();
    assert!(matches!(
        check_signature(&opened, &[public]),
        SignatureCheck::Trusted { .. }
    ));
    let files = &opened.statement.predicate.files;
    assert!(!files.complete);
    assert_eq!(
        files.unattributed,
        vec!["turn turn-gone is not in the Session's history".to_string()]
    );

    // A key that cannot be read: the run keeps its verdict and says why it
    // has no receipt; the API says so too.
    crate::secret_store::set_secret(
        daemon.data_root.path(),
        crate::receipt_key::RECEIPT_KEY_SECRET,
        b"not-a-key",
    )
    .unwrap();
    let broken = receipt_manifest(&session, &repo, &head);
    store.create(&broken).unwrap();
    let mut outcome = ended_outcome(&broken, "the environment failed".into(), Vec::new());
    let warning = daemon
        .seal_or_warn(&broken.run_id, &outcome)
        .await
        .expect("a warning");
    assert_eq!(warning.code, "receipt_unavailable");
    assert!(
        !warning.message.contains("not-a-key"),
        "{}",
        warning.message
    );
    outcome.warnings.push(warning);
    daemon.finish_loadout_run(&broken.run_id, &outcome).unwrap();
    assert_eq!(outcome.exit_code, exit_code::INFRASTRUCTURE);
    match daemon.loadout_run_receipt(&broken.run_id) {
        Err(DaemonError::NotFound(message)) => {
            assert!(message.contains("has no receipt"), "{message}");
            assert!(message.contains("--rotate"), "{message}");
        }
        other => panic!("{other:?}"),
    }
    assert!(daemon
        .loadout_run(&broken.run_id)
        .await
        .unwrap()
        .receipt
        .is_none());
    daemon.shutdown().await.unwrap();
}

/// Every finished run is signed with the data root's key before its Outcome
/// is written, served once it has finished, carried as the record bundle's
/// last section, re-signed when a restart ends it, and a signing failure
/// leaves the verdict alone with a warning.
#[tokio::test]
async fn every_finished_run_is_signed_served_and_bundled() {
    if std::env::var_os(CHILD).is_some() {
        receipts_child_body().await;
        return;
    }
    run_child(
        "bootstrap::loadout_runs::daemon_tests::every_finished_run_is_signed_served_and_bundled",
        true,
    )
    .await;
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

/// fix-claude-code and fix-codex through admission: writer runtime, model
/// default, recipe image, routes and the credential (auto with each
/// combination of stored test secrets), how the choice is recorded, the
/// same-model warning, and a user file declaring a built-in id.
#[tokio::test]
async fn the_external_fix_builtins_resolve_their_writer_recipe_routes_and_credential() {
    if std::env::var_os(CHILD).is_some() {
        external_fix_admission_child_body().await;
        return;
    }
    run_child(
        "bootstrap::loadout_runs::daemon_tests::the_external_fix_builtins_resolve_their_writer_recipe_routes_and_credential",
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
                            .filter(|name| name.ends_with(id))
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
        // Its read-only Agents run as the helper user, which can enter and
        // list this Workspace (0755), so a run's admission lets them start.
        assert_eq!(
            sandbox
                .helper_workspace_access()
                .await
                .map_err(|error| DaemonError::Session(error.to_string()))?,
            Some(axocoatl_isolation::HelperWorkspaceAccess::Readable)
        );
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
        // Without workload users every command runs as the image's user.
        assert_eq!(
            plain_sandbox
                .helper_workspace_access()
                .await
                .map_err(|error| DaemonError::Session(error.to_string()))?,
            None
        );
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

/// A custom loadout with one writer that may only read files and no checks:
/// the writer's turn needs attention when its provider fails.
const NATIVE_RUN: &str = r#"
schema: axocoatl.loadout/1
id: native-run
version: 1
name: Native run
kind: custom
params:
  writer_model: { kind: model, required: true }
agents:
  - id: writer
    role: writer
    model: { param: writer_model }
    tools: [read_file]
budgets:
  agent: { activations: 2, invocations: 40, tokens: 100000, cost_usd: 1 }
  wall_clock: 5m
prompt: "{task}"
"#;

/// The task of the run the test stops while its provider call is running.
const SLOW_TASK: &str = "Wait for the person to stop this run.";
/// What the writer of the stopped run has written when it is stopped: part
/// of an answer, as the fix re-smoke's writer had.
const STREAMED_TEXT: &str = "I can see the issue now. Let me also look at the tests to better";

/// A model server in front of `upstream` (the wiremock model server): a
/// `/api/chat` call for [`SLOW_TASK`] streams one line of the writer's
/// answer ([`STREAMED_TEXT`]) and then stalls without ending the stream, as
/// a model does while it works; every other request goes to `upstream` as
/// it came. Returns its base URL and how many such lines it has sent.
async fn streaming_front(upstream: String) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
    use http_body_util::{combinators::BoxBody, BodyExt, Full, StreamBody};
    use hyper::body::{Bytes, Frame, Incoming};
    type Body = BoxBody<Bytes, std::convert::Infallible>;

    async fn respond(
        request: hyper::Request<Incoming>,
        client: reqwest::Client,
        upstream: String,
        streamed: Arc<std::sync::atomic::AtomicUsize>,
    ) -> hyper::Response<Body> {
        let method = request.method().as_str().to_owned();
        let target = request
            .uri()
            .path_and_query()
            .map(|path| path.as_str().to_owned())
            .unwrap_or_else(|| "/".into());
        let content_type = request.headers().get("content-type").cloned();
        let body = request
            .into_body()
            .collect()
            .await
            .map(|collected| collected.to_bytes())
            .unwrap_or_default();
        if target.starts_with("/api/chat") && String::from_utf8_lossy(&body).contains(SLOW_TASK) {
            let model = serde_json::from_slice::<serde_json::Value>(&body)
                .map(|body| body["model"].clone())
                .unwrap_or_default();
            let line = serde_json::json!({
                "model": model, "created_at": "2026-10-07T00:00:00Z",
                "message": {"role": "assistant", "content": STREAMED_TEXT},
                "done": false
            });
            let (sender, receiver) =
                tokio::sync::mpsc::channel::<Result<Frame<Bytes>, std::convert::Infallible>>(1);
            tokio::spawn(async move {
                if sender
                    .send(Ok(Frame::data(Bytes::from(format!("{line}\n")))))
                    .await
                    .is_ok()
                {
                    streamed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
                // Never end the stream: wait until the client goes away.
                sender.closed().await;
            });
            return hyper::Response::builder()
                .header("content-type", "application/x-ndjson")
                .body(
                    StreamBody::new(tokio_stream::wrappers::ReceiverStream::new(receiver)).boxed(),
                )
                .unwrap();
        }
        let mut forwarded = client.request(
            reqwest::Method::from_bytes(method.as_bytes()).unwrap(),
            format!("{upstream}{target}"),
        );
        if let Some(content_type) = content_type {
            forwarded = forwarded.header("content-type", content_type.as_bytes());
        }
        let (status, content_type, bytes) = match forwarded.body(body.to_vec()).send().await {
            Ok(response) => (
                response.status().as_u16(),
                response
                    .headers()
                    .get("content-type")
                    .map(|value| value.as_bytes().to_vec()),
                response.bytes().await.unwrap_or_default(),
            ),
            Err(error) => (502, None, Bytes::from(error.to_string())),
        };
        let mut response = hyper::Response::builder().status(status);
        if let Some(content_type) = content_type {
            response = response.header("content-type", content_type);
        }
        response.body(Full::new(bytes).boxed()).unwrap()
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let client = reqwest::Client::new();
    let streamed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = streamed.clone();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let (client, upstream, counter) = (client.clone(), upstream.clone(), counter.clone());
            tokio::spawn(async move {
                let service = hyper::service::service_fn(move |request| {
                    let (client, upstream, counter) =
                        (client.clone(), upstream.clone(), counter.clone());
                    async move {
                        Ok::<_, std::convert::Infallible>(
                            respond(request, client, upstream, counter).await,
                        )
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    (format!("http://{address}"), streamed)
}

/// Native Ollama's `/api/chat` as a model whose tool call Ollama cannot
/// parse answers it: the first call asks for read_file (120 input and 7
/// output tokens), every later call ends its stream with Ollama's error, as
/// the qa smoke run's explorer did.
struct ToolCallThenParseError {
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

impl wiremock::Respond for ToolCallThenParseError {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let body: serde_json::Value = request.body_json().unwrap_or_default();
        let reply = if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            serde_json::json!({
                "model": body["model"], "created_at": "2026-10-07T00:00:00Z",
                "message": {"role": "assistant", "content": "", "tool_calls": [{
                    "id": "call_1",
                    "function": {"index": 0, "name": "read_file", "arguments": {"path": "README.md"}}
                }]},
                "done": true, "done_reason": "stop", "prompt_eval_count": 120, "eval_count": 7
            })
        } else {
            serde_json::json!({"error": "error parsing tool call: raw='{\"command\":\"x'"})
        };
        ResponseTemplate::new(200).set_body_raw(format!("{reply}\n"), "application/x-ndjson")
    }
}

/// What the task that sent each turn returned, by turn id (`None` while it
/// runs).
type SendResults = Arc<std::sync::Mutex<HashMap<String, Option<Result<(), String>>>>>;

/// The run driver's host over this daemon, as the server's `DaemonRunHost`
/// is: each turn is sent as `/ws` sends it and observed through the
/// control-plane projection.
struct LiveHost {
    daemon: Arc<AxocoatlDaemon>,
    labels: std::sync::Mutex<Vec<CheckLabel>>,
    sends: SendResults,
}

#[async_trait::async_trait]
impl crate::loadout::RunHost for LiveHost {
    async fn apply_team(
        &self,
        session_id: &str,
        edit: crate::SessionTeamEdit,
    ) -> Result<(), RunError> {
        *self.labels.lock().unwrap() = CheckLabel::of_edit(&edit);
        Ok(self.daemon.apply_loadout_team(session_id, edit).await?)
    }

    async fn send_turn(&self, session_id: &str, request: &str) -> Result<String, RunError> {
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
    ) -> Result<TurnObservation, RunError> {
        loop {
            let labels = self.labels.lock().unwrap().clone();
            let observed = self
                .daemon
                .loadout_turn_observation(session_id, turn_id, &labels, None)
                .await?;
            let sent = self.sends.lock().unwrap().get(turn_id).cloned().flatten();
            match (&observed, &sent) {
                (Some(observation), _) if observation.state != TurnState::Running => {
                    return Ok(observation.clone())
                }
                (None, Some(Err(error))) => {
                    return Err(RunError::Infrastructure(format!(
                        "the turn could not start: {error}"
                    )))
                }
                _ => {}
            }
            if std::time::Instant::now() >= deadline {
                return observed
                    .ok_or_else(|| RunError::Infrastructure("the turn has no projection".into()));
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    async fn stop_turn(&self, session_id: &str, turn_id: &str) -> Result<(), RunError> {
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
    ) -> Result<axocoatl_session::run_outcome::ReproRun, RunError> {
        Err(RunError::NotImplemented("this loadout reproduces nothing"))
    }

    async fn read_sandbox_file(
        &self,
        session_id: &str,
        path: &str,
        max_bytes: usize,
    ) -> Result<Option<Vec<u8>>, RunError> {
        Ok(self
            .daemon
            .loadout_read_sandbox_file(session_id, path, max_bytes)
            .await?)
    }

    async fn record(&self, run_id: &str, event: RunEvent) -> Result<(), RunError> {
        self.daemon
            .record_loadout_run_event(run_id, &event)
            .map(|_| ())
            .map_err(Into::into)
    }

    async fn recorded_events(&self, run_id: &str) -> Result<Vec<RunEvent>, RunError> {
        Ok(self.daemon.loadout_run_recorded_events(run_id)?)
    }

    async fn network_summary(&self, session_id: &str) -> Result<NetworkSummary, RunError> {
        Ok(self.daemon.loadout_network_summary(session_id).await)
    }

    async fn ollama_versions(
        &self,
        session_id: &str,
    ) -> std::collections::BTreeMap<String, Vec<String>> {
        self.daemon.loadout_ollama_versions(session_id)
    }

    async fn stop_requested(&self, run_id: &str) -> bool {
        self.daemon.loadout_run_stop_requested(run_id)
    }

    async fn seal(&self, run_id: &str, outcome: &RunOutcome) -> Result<(), String> {
        self.daemon.seal_loadout_run(run_id, outcome).await
    }

    async fn finish(&self, run_id: &str, outcome: &RunOutcome) -> Result<(), RunError> {
        Ok(self.daemon.finish_loadout_run(run_id, outcome)?)
    }
}

/// A whole loadout run on real Podman whose writer's provider fails after
/// one successful call, so its turn needs attention:
/// - the run counts the tokens of the call that succeeded (the projection
///   attaches usage only to accepted answers);
/// - once the run has its Outcome, its Session no longer holds the
///   Workspace, so the next run on the repository is not refused or kept
///   waiting;
/// - the record bundle's history is the Session's versioned export, and its
///   team section says the writer started from a fresh conversation with the
///   loadout's tools.
///
/// ```text
/// CONTAINER_CONNECTION=axocoatl-ci-pr74 cargo test -p axocoatl-daemon --lib \
///   a_needs_attention_run_on_podman -- --ignored
/// ```
#[tokio::test]
#[ignore = "requires rootless Podman, docker.io/library/alpine:3.20 and the egress sidecar image"]
async fn a_needs_attention_run_on_podman_releases_its_workspace_and_records_what_ran() {
    if std::env::var_os(CHILD).is_none() {
        let root = tempfile::tempdir().unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(900),
            tokio::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "bootstrap::loadout_runs::daemon_tests::a_needs_attention_run_on_podman_releases_its_workspace_and_records_what_ran",
                    "--nocapture",
                    "--ignored",
                ])
                .env(CHILD, "1")
                .env("AXOCOATL_DATA_DIR", root.path().join("data"))
                .env("AXOCOATL_SOCKET_PATH", "ipc/daemon.sock")
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
    let chat_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ToolCallThenParseError {
            calls: chat_calls.clone(),
        })
        .mount(&server)
        .await;
    // The stopped run's writer streams part of an answer before it stalls,
    // so its stop goes through the path a real model's does.
    let (models, streamed) = streaming_front(server.uri()).await;
    let config_dir = tempfile::tempdir().unwrap();
    let config_path = config_dir.path().join("axocoatl.yaml");
    std::fs::write(
        &config_path,
        format!(
            "agents: []\nproviders:\n  ollama:\n    base_url: {models}\nsandbox:\n  backend: podman\n  network: bridge\nconsolidation:\n  enabled: false\n"
        ),
    )
    .unwrap();
    std::fs::create_dir(config_dir.path().join(USER_LOADOUT_DIR)).unwrap();
    std::fs::write(
        config_dir
            .path()
            .join(USER_LOADOUT_DIR)
            .join("native-run.yaml"),
        NATIVE_RUN,
    )
    .unwrap();
    let config = axocoatl_config::load_config(&config_path).await.unwrap();
    let daemon = Arc::new(AxocoatlDaemon::bootstrap_headless(config).await.unwrap());
    daemon.set_config_path(&config_path);
    // The repository is outside the data root, which the container must not
    // reach.
    let outside = tempfile::Builder::new()
        .prefix("axocoatl-loadout-run-")
        .tempdir_in(std::env::temp_dir())
        .unwrap();
    let repo = outside.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("README.md"), "# Native run\n").unwrap();

    let (accepted, context) = daemon
        .admit_loadout_run(RunRequest {
            loadout: "native-run".into(),
            task: "Read README.md and say what it is.".into(),
            repo: repo.display().to_string(),
            params: [("writer_model".to_string(), format!("ollama:{MODEL}"))]
                .into_iter()
                .collect(),
            keep: Default::default(),
            check_command: None,
            setup_command: None,
            request_id: "podman-run".into(),
        })
        .await
        .unwrap();
    // Whatever an assertion does, the containers, volumes and networks of
    // every Session the test's runs created are removed.
    struct Cleanup(std::sync::Mutex<Vec<String>>);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            for id in self
                .0
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .iter()
            {
                remove_session_runtime(id);
            }
        }
    }
    fn remove_session_runtime(id: &str) {
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
                    .filter(|name| name.ends_with(id))
                {
                    let _ = std::process::Command::new("podman")
                        .args([kind, "rm", "-f", name])
                        .output();
                }
            }
        }
    }
    let cleanup = Cleanup(std::sync::Mutex::new(vec![accepted.session_id.clone()]));
    let result = async {
        let ready = daemon.loadout_run(&accepted.run_id).await?;
        assert!(ready.outcome.is_none(), "the environment failed: {ready:?}");
        // The qa driver creates a missing reproductions directory on the
        // host as SecureDir creates directories, owner-only; the Session's
        // hardened writer still writes into it with write_file's own
        // command, because the Workspace's owner is the writer in the
        // container.
        let repo_root = repo.canonicalize().unwrap();
        assert!(
            crate::loadout::qa::repro_dir_ready(&repo_root, "axocoatl-qa", true)
                .await
                .map_err(|error| DaemonError::Session(error.to_string()))?
        );
        let sandbox = daemon
            .session_sandboxes
            .lock()
            .await
            .get(&accepted.session_id)
            .cloned()
            .unwrap();
        let written = sandbox
            .exec_stdin(
                &["sh", "-c", "cat > \"$1\"", "sh", "axocoatl-qa/b1.spec.ts"],
                "import { test } from '@playwright/test';\n",
                Duration::from_secs(60),
            )
            .await
            .map_err(|error| DaemonError::Session(error.to_string()))?;
        assert_eq!(written.exit_code, 0, "{written:?}");
        assert_eq!(
            std::fs::read_to_string(repo_root.join("axocoatl-qa/b1.spec.ts")).unwrap(),
            "import { test } from '@playwright/test';\n"
        );
        let id = sandbox
            .exec(&["id", "-u"], Duration::from_secs(30))
            .await
            .map_err(|error| DaemonError::Session(error.to_string()))?;
        assert_ne!(id.stdout.trim(), "0", "the writer is not root: {id:?}");
        let host = LiveHost {
            daemon: daemon.clone(),
            labels: std::sync::Mutex::new(Vec::new()),
            sends: Arc::new(std::sync::Mutex::new(HashMap::new())),
        };
        let outcome = crate::loadout::driver::run_to_outcome(&host, &context)
            .await
            .map_err(|error| DaemonError::Session(error.to_string()))?;
        assert!(chat_calls.load(std::sync::atomic::Ordering::SeqCst) >= 2);
        assert_eq!(
            outcome.exit_code,
            exit_code::NEEDS_ATTENTION,
            "{:?} {:?} {:?}",
            outcome.error,
            outcome.attention,
            outcome.not_covered
        );
        assert_eq!(outcome.turns.len(), 1);
        assert_eq!(outcome.turns[0].state, TurnState::NeedsAttention);
        let writer = outcome
            .not_covered
            .iter()
            .find(|entry| entry.area == "writer")
            .unwrap_or_else(|| panic!("{:?}", outcome.not_covered));
        assert!(
            writer.detail.contains("error parsing tool call"),
            "{writer:?}"
        );
        // The call that succeeded before the provider failed is counted.
        assert!(
            outcome.usage.input_tokens >= 120 && outcome.usage.output_tokens >= 7,
            "{:?}",
            outcome.usage
        );

        // The run has its Outcome: the Workspace is free for the next run,
        // because the run closed its turn.
        let operation = daemon
            .attempt_operation_for_workspace(&accepted.workspace_id)
            .await;
        assert!(
            operation.try_lock().is_ok(),
            "the run's Session still holds the Workspace"
        );
        let events = daemon.loadout_run_recorded_events(&accepted.run_id)?;
        assert!(events.iter().any(
            |event| matches!(event, RunEvent::Phase { phase, .. } if phase == "closing_turn")
        ));

        // The record bundle carries the versioned history and the team as
        // applied.
        let mut bundle = Vec::new();
        daemon
            .write_record_bundle(&accepted.run_id, &mut bundle)
            .await?;
        let sections: Vec<serde_json::Value> = String::from_utf8(bundle)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let section = |name: &str| {
            sections
                .iter()
                .find(|line| line["section"] == name)
                .map(|line| line["data"].clone())
                .unwrap_or_else(|| panic!("no {name} section"))
        };
        let history = section("history");
        let entries = history
            .as_array()
            .unwrap_or_else(|| panic!("history is not the Session export: {history}"));
        let turn_id = &outcome.turns[0].turn_id;
        assert!(
            entries
                .iter()
                .any(|entry| entry.to_string().contains(turn_id.as_str())),
            "{history}"
        );
        let team = section("team");
        let slot = &team["slots"][0];
        assert_eq!(slot["slot_id"], "writer", "{team}");
        assert_eq!(slot["reset_history"], true, "{team}");
        assert_eq!(slot["tools"], serde_json::json!(["read_file"]), "{team}");
        // The run that needs attention is signed too: its receipt verifies
        // with the data root's key and is the bundle's last section.
        let receipt = daemon.loadout_run_receipt(&accepted.run_id)?;
        let key = daemon.receipt_public_key().await?;
        let opened = axocoatl_session::receipt::open_receipt(&receipt).unwrap();
        assert!(matches!(
            axocoatl_session::receipt::check_signature(
                &opened,
                &[axocoatl_session::receipt::PublicKey::from_base64(&key.public_key).unwrap()]
            ),
            axocoatl_session::receipt::SignatureCheck::Trusted { .. }
        ));
        assert_eq!(
            opened.statement.predicate.verdict,
            axocoatl_session::run_outcome::RunVerdict::NeedsAttention
        );
        assert_eq!(
            sections[sections.len() - 2]["section"],
            "receipt",
            "the receipt is the last section"
        );

        // A run a person stops while its writer's provider call is running,
        // after the writer streamed part of its answer, ends interrupted;
        // the writer is not covered because of the stop, with a reason,
        // never "other" with an empty one or with the half-written answer;
        // and the Workspace is free again.
        let stopped_repo = outside.path().join("stopped");
        std::fs::create_dir_all(&stopped_repo).unwrap();
        let (stopped, stopped_context) = daemon
            .admit_loadout_run(RunRequest {
                loadout: "native-run".into(),
                task: SLOW_TASK.into(),
                repo: stopped_repo.display().to_string(),
                params: [("writer_model".to_string(), format!("ollama:{MODEL}"))]
                    .into_iter()
                    .collect(),
                keep: Default::default(),
                check_command: None,
                setup_command: None,
                request_id: "podman-stop".into(),
            })
            .await?;
        cleanup.0.lock().unwrap().push(stopped.session_id.clone());
        let driver = {
            let host = LiveHost {
                daemon: daemon.clone(),
                labels: std::sync::Mutex::new(Vec::new()),
                sends: Arc::new(std::sync::Mutex::new(HashMap::new())),
            };
            tokio::spawn(async move {
                crate::loadout::driver::run_to_outcome(&host, &stopped_context).await
            })
        };
        // Stop once the writer's partial answer is in its turn's record.
        let started = tokio::time::Instant::now();
        loop {
            let turn_id = daemon
                .loadout_run_recorded_events(&stopped.run_id)?
                .into_iter()
                .find_map(|event| match event {
                    RunEvent::TurnStarted { turn_id, .. } => Some(turn_id),
                    _ => None,
                });
            let recorded = match &turn_id {
                Some(turn_id) => daemon
                    .session_turn_control_plane(&stopped.session_id, turn_id)
                    .await?
                    .is_some_and(|view| {
                        view.nodes
                            .iter()
                            .flat_map(|node| &node.activations)
                            .any(|activation| {
                                activation.evidence.iter().any(|evidence| {
                                    evidence.kind == "stream_text"
                                        && matches!(
                                            &evidence.summary,
                                            crate::session_control_plane::EvidenceValue::Available {
                                                value
                                            } if value.contains(STREAMED_TEXT)
                                        )
                                })
                            })
                    }),
                None => false,
            };
            if recorded {
                break;
            }
            assert!(
                started.elapsed() < Duration::from_secs(120),
                "the stopped run's writer never recorded its partial answer ({} lines \
                 streamed, turn {turn_id:?})",
                streamed.load(std::sync::atomic::Ordering::SeqCst)
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        daemon.request_loadout_run_stop(&stopped.run_id).await?;
        let outcome = tokio::time::timeout(Duration::from_secs(180), driver)
            .await
            .expect("the stopped run ends")
            .unwrap()
            .map_err(|error| DaemonError::Session(error.to_string()))?;
        assert_eq!(outcome.exit_code, exit_code::INTERRUPTED, "{outcome:?}");
        for entry in &outcome.not_covered {
            assert!(
                !entry.detail.trim().is_empty()
                    && entry.detail != "other: "
                    && !entry.detail.contains(STREAMED_TEXT)
                    && entry.class != axocoatl_session::run_outcome::FailureClass::Other,
                "{entry:?}"
            );
        }
        let writer = outcome
            .not_covered
            .iter()
            .find(|entry| entry.area == "writer")
            .unwrap_or_else(|| panic!("{:?}", outcome.not_covered));
        assert_eq!(writer.reason(), "stopped: writer ended without a result");
        let operation = daemon
            .attempt_operation_for_workspace(&stopped.workspace_id)
            .await;
        assert!(
            operation.try_lock().is_ok(),
            "the stopped run's Session still holds the Workspace"
        );
        Ok::<(), DaemonError>(())
    }
    .await;
    let shutdown = daemon.shutdown_session_runtimes_checked().await;
    drop(cleanup);
    result.unwrap();
    shutdown.unwrap();
}
