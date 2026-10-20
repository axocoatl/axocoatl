//! Team and budget for a new user: the team view suggests limits that work
//! (the built-in fix loadout's), and Preview refuses limits that cannot pay
//! for a turn with the numbers, for a changed slot and for one kept as it
//! was. The Ollama server is a fixture that reports a 32,768-token context.
use super::session_team::{SessionTeamApply, SessionTeamEdit, SessionTeamView};
use super::*;
use axocoatl_session::control_authority::GrantLimits;
use axocoatl_session::execution_ownership::DataRootFormatOwnership;
use std::os::unix::fs::PermissionsExt;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

const DIGEST: &str = "a80c4f17acd55265feec403c7aef86be0c25983ab279d83f3bcd3abbcb5b8b72";

async fn ollama() -> MockServer {
    let server = MockServer::start().await;
    crate::ollama_contract_test_support::mount_contract_probes(&server).await;
    for (verb, endpoint, value) in [
        (
            "GET",
            "/api/version",
            serde_json::json!({"version": axocoatl_llm_ollama::AUDITED_OLLAMA_VERSION}),
        ),
        (
            "GET",
            "/api/status",
            serde_json::json!({"cloud":{"disabled":true}}),
        ),
        (
            "POST",
            "/api/show",
            serde_json::json!({"details":{"format":"gguf"},"capabilities":["completion","tools"]}),
        ),
        (
            "GET",
            "/api/tags",
            serde_json::json!({"models":[{"name":"budget-model:latest","model":"budget-model:latest","digest":DIGEST}]}),
        ),
        (
            "GET",
            "/api/ps",
            serde_json::json!({"models":[{"name":"budget-model:latest","model":"budget-model:latest","digest":DIGEST,"details":{"format":"gguf"},"context_length":32768,"runner":"llamacpp"}]}),
        ),
        (
            "POST",
            "/api/generate",
            serde_json::json!({"model":"budget-model:latest","created_at":"2026-09-15T00:00:00Z","response":"","done":true,"done_reason":"load"}),
        ),
    ] {
        Mock::given(method(verb))
            .and(path(endpoint))
            .respond_with(ResponseTemplate::new(200).set_body_json(value))
            .mount(&server)
            .await;
    }
    server
}

fn edit_with_tokens(
    view: &SessionTeamView,
    command: &str,
    tokens: u64,
    reset_history: bool,
) -> SessionTeamEdit {
    let mut slots = view.slots.clone();
    let budget = view.suggested_budget.clone().unwrap();
    // The app fixes the suggested expiry when it fills a slot in.
    let filled_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    for slot in &mut slots {
        let mut limits = slot.limits.clone().unwrap_or(budget.agent.clone());
        limits.tokens = tokens;
        limits.cost_microunits = 0;
        slot.limits = Some(limits);
        slot.expires_at_ms = Some(
            slot.expires_at_ms
                .unwrap_or(filled_at_ms + budget.expires_in_ms),
        );
        slot.reset_history = reset_history;
    }
    SessionTeamEdit {
        command_id: command.into(),
        expected_configuration_revision: view.configuration_revision,
        slots,
        dependencies: view.dependencies.clone(),
        layout: view.layout.clone(),
        required_checks: vec![],
        required_review: None,
        check_options: vec![],
    }
}

async fn child_body() {
    let server = ollama().await;
    let config = format!(
        r#"
agents:
  - id: lead
    name: Lead
    provider: ollama
    model: budget-model
    tools: [read_file, edit_file, bash]
    sampling:
      max_tokens: 4096
providers:
  ollama:
    base_url: {}
consolidation:
  enabled: false
"#,
        server.uri()
    );
    let config =
        axocoatl_config::parse_config(&config, std::path::Path::new("budget.yaml")).unwrap();
    let daemon = AxocoatlDaemon::bootstrap_headless(config).await.unwrap();
    let work = tempfile::tempdir().unwrap();
    let workspace = daemon
        .workspace_store
        .lock()
        .await
        .register(work.path(), Some("Budget"))
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
            "Budget",
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
    let _token = daemon
        .session_dispatch_lifecycles
        .retain_native_session(ownership.clone(), receipt)
        .unwrap();

    // A new Session's team: no limits yet, and the suggestion that fills
    // them: the fix loadout's Agent budget, free on a local model, for 24
    // hours.
    let view = daemon.session_team(&session.id).await.unwrap();
    assert!(!view.approved);
    assert_eq!(view.slots[0].limits, None);
    let budget = view.suggested_budget.clone().unwrap();
    assert_eq!(budget.from, "fix");
    assert_eq!(
        budget.agent,
        GrantLimits {
            activations: 4,
            invocations: 400,
            tokens: 4_000_000,
            cost_microunits: 10_000_000,
        }
    );
    assert_eq!(
        budget.helper,
        GrantLimits {
            activations: 3,
            invocations: 120,
            tokens: 1_500_000,
            cost_microunits: 3_000_000,
        }
    );
    assert_eq!(budget.free_providers, ["ollama"]);
    let day_ms = 24 * 60 * 60 * 1000;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    // The expiry is a lifetime the app turns into an instant when it fills a
    // slot in, not an instant taken at each read.
    assert_eq!(budget.expires_in_ms, day_ms);
    let json = serde_json::to_value(&view).unwrap();
    assert_eq!(json["suggested_budget"]["agent"]["tokens"], 4_000_000);
    assert_eq!(json["suggested_budget"]["expires_in_ms"], 86_400_000);
    assert!(json["suggested_budget"].get("expires_at_ms").is_none());
    // Reading the same team again returns the same view, so a read cannot
    // tell a changed team from an unchanged one by a drifting expiry.
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    assert_eq!(
        serde_json::to_value(daemon.session_team(&session.id).await.unwrap()).unwrap(),
        json
    );

    // Below one call: refused with the numbers.
    let refused = daemon
        .preview_session_team(
            &session.id,
            edit_with_tokens(&view, "budget-one-call", 10_000, true),
        )
        .await
        .err()
        .expect("refused")
        .to_string();
    assert!(
        refused.contains(
            "Lead reserves 36,864 tokens per model call (32,768 context + 4,096 output), more \
             than its token limit allows; a turn with tools needs at least 73,728"
        ),
        "{refused}"
    );

    // One call but not a call and the one after it: refused with the numbers.
    let refused = daemon
        .preview_session_team(
            &session.id,
            edit_with_tokens(&view, "budget-one-round", 50_000, true),
        )
        .await
        .err()
        .expect("refused");
    assert!(
        matches!(refused, DaemonError::InvalidRequest(_)),
        "{refused:?}"
    );
    assert_eq!(
        refused.to_string(),
        "Lead's token limit of 50,000 is too small: Lead reserves 36,864 tokens per model call \
         (32,768 context + 4,096 output), and a turn with tools needs at least 73,728, for a \
         call and the one after it. Raise its token limit to at least 73,728."
    );

    // The suggestion itself previews and applies as it is.
    let mut suggested = edit_with_tokens(&view, "budget-suggested", 4_000_000, true);
    for slot in &mut suggested.slots {
        let mut limits = budget.agent.clone();
        limits.cost_microunits = 0;
        slot.limits = Some(limits);
    }
    let preview = daemon
        .preview_session_team(&session.id, suggested.clone())
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    daemon
        .apply_session_team(
            &session.id,
            SessionTeamApply {
                edit: suggested,
                review_digest: preview.review_digest,
            },
        )
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    let applied = daemon.session_team(&session.id).await.unwrap();
    assert!(applied.approved);
    // An applied team reads the same twice too, suggestion included.
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    assert_eq!(
        serde_json::to_value(daemon.session_team(&session.id).await.unwrap()).unwrap(),
        serde_json::to_value(&applied).unwrap()
    );

    // Lowering the limit of the slot kept as it was (same definition, same
    // conversation) is checked too.
    let refused = daemon
        .preview_session_team(
            &session.id,
            edit_with_tokens(&applied, "budget-lowered", 10_000, false),
        )
        .await
        .err()
        .expect("refused")
        .to_string();
    assert!(
        refused.starts_with("Lead's token limit of 10,000 is too small: Lead reserves 36,864"),
        "{refused}"
    );

    // An expiry more than a year away is refused by name.
    let mut far = edit_with_tokens(&applied, "budget-far", 4_000_000, false);
    far.slots[0].expires_at_ms = Some(now_ms + 400 * day_ms);
    let refused = daemon
        .preview_session_team(&session.id, far)
        .await
        .err()
        .expect("refused")
        .to_string();
    assert_eq!(
        refused,
        "Lead's budget expiry is more than a year away; choose an expiry within a year"
    );

    daemon.delete_session(&session.id).await.unwrap();
    daemon.shutdown().await.unwrap();
}

#[tokio::test]
async fn team_budget_suggests_working_limits_and_refuses_ones_a_turn_cannot_use() {
    const CHILD: &str = "AXOCOATL_TEST_SESSION_TEAM_BUDGET_CHILD";
    if std::env::var_os(CHILD).is_some() {
        child_body().await;
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let bin = root.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let podman = bin.join("podman");
    std::fs::write(
        &podman,
        r#"#!/bin/sh
case "$*" in
  --version) printf 'podman version 5.0.0\n' ;;
  'machine list --format json') printf '[{"Running":true}]\n' ;;
  'info --format json') printf '{}\n' ;;
  'ps '*) ;;
  'rm '*|'volume rm '*|'network rm '*) ;;
  *) printf 'unexpected Podman command: %s\n' "$*" >&2; exit 1 ;;
esac
"#,
    )
    .unwrap();
    std::fs::set_permissions(&podman, std::fs::Permissions::from_mode(0o700)).unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(120),
        tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "bootstrap::session_team_budget_tests::team_budget_suggests_working_limits_and_refuses_ones_a_turn_cannot_use",
                "--nocapture",
            ])
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
