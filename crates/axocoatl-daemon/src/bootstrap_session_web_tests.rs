//! The daemon builds its web tools from configuration, refuses a team whose
//! Agent lists a web tool that is not configured, and tells the team view
//! which Agents list one.
use super::*;
use axocoatl_session::execution_ownership::DataRootFormatOwnership;
use axocoatl_session::turn_contract::AgentDefinitionId;
use std::os::unix::fs::PermissionsExt;

const CONFIG: &str = r#"
agents:
  - id: researcher
    name: Researcher
    provider: ollama
    model: fixture-model
    tools: [read_file, web_search, web_fetch]
  - id: searcher
    name: Searcher
    provider: ollama
    model: fixture-model
    tools: [read_file, web_search]
  - id: coder
    name: Coder
    provider: ollama
    model: fixture-model
    tools: [read_file]
providers:
  ollama:
    base_url: http://127.0.0.1:1
web_search:
  provider: searxng
consolidation:
  enabled: false
"#;

async fn child_body() {
    let config = axocoatl_config::parse_config(CONFIG, std::path::Path::new("web.yaml")).unwrap();
    let daemon = AxocoatlDaemon::bootstrap_headless(config.clone())
        .await
        .unwrap();
    // Startup looks for a SearXNG left by an earlier run, by this data root's
    // authority and the searxng role, in the background.
    let log = std::path::PathBuf::from(std::env::var_os("AXOCOATL_TEST_PODMAN_LOG").unwrap());
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let calls = std::fs::read_to_string(&log).unwrap_or_default();
        if calls.lines().any(|line| {
            line.starts_with("ps -a --filter label=io.axocoatl.runtime-authority=")
                && line.contains("--filter label=io.axocoatl.role=searxng")
        }) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no SearXNG orphan lookup: {calls}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // The managed SearXNG keeps its secret owner-only in the data root; no
    // container starts until a search.
    let secret = daemon.data_root.path().join("searxng/secret");
    let mode = std::fs::metadata(&secret).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    assert!(daemon.web_tools.managed_searxng().is_some());
    assert!(
        !daemon
            .web_tools
            .managed_searxng()
            .unwrap()
            .is_running()
            .await
    );

    // Team admission: web_fetch is listed but not configured.
    let agent = |id: &str| {
        config
            .agents
            .iter()
            .find(|agent| agent.id == id)
            .unwrap()
            .to_core()
    };
    let definition = AgentDefinitionId::new("team-definition-fixture").unwrap();
    let refused = daemon
        .host_tool_refusal(&agent("researcher"), &definition)
        .expect("web_fetch is not configured");
    assert_eq!(
        refused,
        "web_fetch is listed for Researcher but web_fetch is not configured; add a web_fetch: \
         block to the configuration"
    );
    assert_eq!(
        daemon.host_tool_refusal(&agent("searcher"), &definition),
        None
    );
    assert_eq!(daemon.host_tool_refusal(&agent("coder"), &definition), None);
    let names: Vec<&str> = daemon
        .host_invocation_tools()
        .iter()
        .map(|tool| tool.name())
        .collect();
    assert_eq!(names, ["web_search", "web_fetch"]);

    // The team view marks the templates and slots that list a web tool.
    let work = tempfile::tempdir().unwrap();
    let workspace = daemon
        .workspace_store
        .lock()
        .await
        .register(work.path(), Some("Web"))
        .unwrap();
    let DataRootFormatOwnership::Upgraded(ownership) = &daemon._data_dir_lease.ownership else {
        panic!("fresh data roots must use native ownership");
    };
    let (session, receipt) = daemon
        .session_store
        .lock()
        .await
        .create_native_with_environment(
            ownership,
            "Web",
            &workspace.id,
            &workspace.canonical_path,
            SessionMode::SingleAgent {
                agent_id: "searcher".into(),
            },
            vec![],
            vec![],
            None,
            None,
            false,
            true,
        )
        .unwrap();
    let _token = daemon
        .session_dispatch_lifecycles
        .retain_native_session(ownership.clone(), receipt)
        .unwrap();
    let view = daemon.session_team(&session.id).await.unwrap();
    assert_eq!(view.web_templates, ["researcher", "searcher"]);
    assert_eq!(view.web_slots, ["slot-searcher"]);
    let json = serde_json::to_value(&view).unwrap();
    assert_eq!(json["web_slots"], serde_json::json!(["slot-searcher"]));

    daemon.delete_session(&session.id).await.unwrap();
    // Shutdown runs no Podman command for a SearXNG that never started.
    daemon.shutdown().await.unwrap();
}

#[tokio::test]
async fn web_tools_come_from_configuration_and_gate_team_admission() {
    const CHILD: &str = "AXOCOATL_TEST_SESSION_WEB_CHILD";
    if std::env::var_os(CHILD).is_some() {
        child_body().await;
        return;
    }
    // Bootstrap owns process environment; isolate it from concurrent tests.
    let root = tempfile::tempdir().unwrap();
    let bin = root.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let log = root.path().join("podman.log");
    let podman = bin.join("podman");
    std::fs::write(
        &podman,
        format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
case "$*" in
  --version) printf 'podman version 5.0.0\n' ;;
  'machine list --format json') printf '[{{"Running":true}}]\n' ;;
  'info --format json') printf '{{}}\n' ;;
  'ps '*) ;;
  'rm '*|'volume rm '*|'network rm '*) ;;
  *) printf 'unexpected Podman command: %s\n' "$*" >&2; exit 1 ;;
esac
"#,
            log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&podman, std::fs::Permissions::from_mode(0o700)).unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(60),
        tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "bootstrap::session_web_tests::web_tools_come_from_configuration_and_gate_team_admission",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("AXOCOATL_TEST_PODMAN_LOG", &log)
            .env("AXOCOATL_DATA_DIR", root.path().join("data"))
            .env("AXOCOATL_SOCKET_PATH", root.path().join("ipc/daemon.sock"))
            .env("PATH", bin)
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
    // Nothing created a SearXNG: it starts only on a search.
    let calls = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        !calls
            .lines()
            .any(|line| line.starts_with("create ") || line.starts_with("container ")),
        "{calls}"
    );
}
