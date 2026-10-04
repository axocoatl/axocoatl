use super::*;
use axocoatl_session::execution_ownership::DataRootFormatOwnership;
use axocoatl_session::network_record::{LimitKind, NetworkEvent, SidecarState};
use std::os::unix::fs::PermissionsExt;

fn sidecar(state: SidecarState) -> NetworkEvent {
    NetworkEvent::Sidecar {
        state,
        generation: 1,
        container: Some("axo-egr-test".into()),
        detail: None,
    }
}

async fn native_session(daemon: &AxocoatlDaemon, work: &std::path::Path) -> String {
    let workspace = daemon
        .workspace_store
        .lock()
        .await
        .register(work, Some("Network record"))
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
            "Network record",
            &workspace.id,
            &workspace.canonical_path,
            SessionMode::SingleAgent {
                agent_id: "conversation".into(),
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
    session.id.clone()
}

async fn child_body() {
    let mut config = axocoatl_config::AxocoatlConfig::default();
    config.agents.clear();
    config.consolidation.enabled = false;
    let daemon = AxocoatlDaemon::bootstrap_headless(config).await.unwrap();
    let work = tempfile::tempdir().unwrap();
    let id = native_session(&daemon, work.path()).await;
    // Per-Session allows exist only under network: egress.
    let refused = daemon
        .allow_session_network_host(
            &id,
            crate::session_network::NetworkAllowRequest {
                command_id: "c1".into(),
                scope: "session".into(),
                host: "api.example.com".into(),
                ports: None,
            },
        )
        .await
        .unwrap_err();
    assert!(
        matches!(refused, DaemonError::InvalidRequest(_)),
        "{refused}"
    );

    // A Session that never had network activity reads as empty, and the read
    // creates nothing.
    let view = daemon.session_network(&id, None, None).await.unwrap();
    assert_eq!(view.mode, "bridge");
    assert!(view.events.is_empty() && view.policies.is_empty() && view.sidecar.is_none());
    assert_eq!(view.record.events, 0);
    assert_eq!(view.next_after, None);
    assert!(!daemon.session_network_records.is_open(&id).await);
    assert!(view.warnings.is_empty());

    // A configuration file inside the Session's Workspace is named, one
    // elsewhere is not.
    let elsewhere = tempfile::tempdir().unwrap();
    for (directory, warned) in [(elsewhere.path(), false), (work.path(), true)] {
        let config_path = directory.join("axocoatl.yaml");
        std::fs::write(&config_path, "agents: []\n").unwrap();
        daemon.set_config_path(&config_path);
        let view = daemon.session_network(&id, None, None).await.unwrap();
        let expected: Vec<String> = if warned {
            vec![CONFIG_IN_WORKSPACE_WARNING.to_string()]
        } else {
            Vec::new()
        };
        assert_eq!(view.warnings, expected, "{}", config_path.display());
        let wire = serde_json::to_value(&view).unwrap();
        assert_eq!(wire.get("warnings").is_some(), warned);
    }
    std::fs::remove_file(work.path().join("axocoatl.yaml")).unwrap();

    for state in [
        SidecarState::Starting,
        SidecarState::Ready,
        SidecarState::Stopped,
    ] {
        daemon
            .session_network_records
            .append(&id, sidecar(state))
            .await
            .unwrap();
    }
    let view = daemon.session_network(&id, None, None).await.unwrap();
    assert_eq!(view.events.len(), 3);
    assert_eq!(view.record.events, 3);
    assert_eq!(view.next_after, Some(3));
    let view = daemon.session_network(&id, Some(1), Some(1)).await.unwrap();
    assert_eq!(view.events.len(), 1);
    assert_eq!(view.events[0].seq, 2);
    assert_eq!(view.next_after, Some(2));
    let wire = serde_json::to_value(&view).unwrap();
    assert_eq!(wire["events"][0]["event"]["kind"], "sidecar");
    assert_eq!(wire["events"][0]["event"]["state"], "ready");
    assert!(wire["sidecar"].is_null());
    for limit in [0, 1001] {
        let error = daemon
            .session_network(&id, None, Some(limit))
            .await
            .unwrap_err();
        assert!(
            matches!(error, DaemonError::InvalidRequest(_)),
            "{limit}: {error}"
        );
    }
    let error = daemon
        .session_network("missing-session", None, None)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("not found"), "{error}");

    // Close releases the record with the rest of the Session's stores, so
    // Reopen can take the Session's directory lock again.
    daemon.close_session(&id).await.unwrap();
    assert!(!daemon.session_network_records.is_open(&id).await);
    // A closed Session's record stays readable without a writer.
    let closed = daemon.session_network(&id, None, None).await.unwrap();
    assert_eq!(closed.events.len(), 3);
    assert_eq!(closed.record.events, 3);
    assert!(!daemon.session_network_records.is_open(&id).await);
    daemon.reopen_session(&id).await.unwrap();
    let seq = daemon
        .session_network_records
        .append(
            &id,
            NetworkEvent::Limit {
                what: LimitKind::UnrecordedRefusals,
                detail: "after reopen".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(seq, 4);
    let view = daemon.session_network(&id, Some(3), None).await.unwrap();
    assert_eq!(view.events.len(), 1);

    daemon.delete_session(&id).await.unwrap();
    assert!(!daemon.session_network_records.is_open(&id).await);
    daemon.shutdown().await.unwrap();
}

async fn egress_child_body() {
    use crate::session_network::{NetworkAllowRequest, NetworkRevokeRequest};
    let mut config = axocoatl_config::AxocoatlConfig::default();
    config.agents.clear();
    config.consolidation.enabled = false;
    config.sandbox.network = "egress".into();
    config.sandbox.egress = Some(axocoatl_config::EgressConfigYaml {
        allow: vec![axocoatl_config::EgressAllowYaml::Preset("npm".into())],
        ..Default::default()
    });
    let daemon = AxocoatlDaemon::bootstrap_headless(config).await.unwrap();
    let work = tempfile::tempdir().unwrap();
    let id = native_session(&daemon, work.path()).await;
    let allow = |command: &str, host: &str, ports: Option<Vec<u16>>| NetworkAllowRequest {
        command_id: command.into(),
        scope: "session".into(),
        host: host.into(),
        ports,
    };

    // No runtime is running: the change is recorded and the view shows it.
    let changed = daemon
        .allow_session_network_host(&id, allow("c1", "api.example.com", Some(vec![443, 8443])))
        .await
        .unwrap();
    assert_eq!(changed.revision, 2);
    let view = daemon.session_network(&id, None, None).await.unwrap();
    assert_eq!(view.mode, "egress");
    let session = view
        .policies
        .iter()
        .find(|policy| policy.scope == "session")
        .unwrap();
    assert_eq!(
        (session.revision, session.digest.as_str()),
        (2, changed.digest.as_str())
    );
    assert!(session
        .rules
        .iter()
        .any(|rule| rule.source == "session" && rule.text.contains("api.example.com:443,8443")));
    let wire = serde_json::to_value(&view).unwrap();
    let change = wire["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|line| line["event"]["source"] == "session_allow")
        .unwrap();
    assert_eq!(change["event"]["change"]["command_id"], "c1");
    assert_eq!(change["event"]["actor"], "human");

    // A resend is a conflict; invalid hosts, scopes and ports are refused.
    let duplicate = daemon
        .allow_session_network_host(&id, allow("c1", "api.example.com", None))
        .await
        .unwrap_err();
    assert!(
        matches!(duplicate, DaemonError::SessionConflict(_)),
        "{duplicate}"
    );
    for (host, ports) in [
        ("*.example.com", None),
        ("10.0.0.1", None),
        ("localhost.", Some(vec![0])),
        ("", None),
    ] {
        let invalid = daemon
            .allow_session_network_host(&id, allow("c-bad", host, ports))
            .await
            .unwrap_err();
        assert!(
            matches!(invalid, DaemonError::InvalidRequest(_)),
            "{host}: {invalid}"
        );
    }
    for scope in ["provisioning", "browser", "everything"] {
        let invalid = daemon
            .allow_session_network_host(
                &id,
                NetworkAllowRequest {
                    scope: scope.into(),
                    ..allow("c-scope", "docs.example.com", None)
                },
            )
            .await
            .unwrap_err();
        assert!(
            matches!(invalid, DaemonError::InvalidRequest(_)),
            "{scope}: {invalid}"
        );
    }
    let missing = daemon
        .allow_session_network_host("missing-session", allow("c2", "a.example.com", None))
        .await
        .unwrap_err()
        .to_string();
    assert!(missing.contains("not found"), "{missing}");

    // Revoke removes only this Session's allows.
    let revoked = daemon
        .revoke_session_network_host(
            &id,
            NetworkRevokeRequest {
                command_id: "c3".into(),
                scope: "session".into(),
                host: "api.example.com".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(revoked.revision, 3);
    let config_host = daemon
        .revoke_session_network_host(
            &id,
            NetworkRevokeRequest {
                command_id: "c4".into(),
                scope: "session".into(),
                host: "registry.npmjs.org".into(),
            },
        )
        .await
        .unwrap_err();
    assert!(
        matches!(config_host, DaemonError::InvalidRequest(_)),
        "{config_host}"
    );

    // After Close and Reopen the decision point replays the record: the
    // revision continues and old command ids stay used.
    daemon.close_session(&id).await.unwrap();
    let closed = daemon
        .allow_session_network_host(&id, allow("c5", "b.example.com", None))
        .await
        .unwrap_err();
    assert!(
        matches!(closed, DaemonError::SessionConflict(_)),
        "{closed}"
    );
    daemon.reopen_session(&id).await.unwrap();
    let duplicate = daemon
        .allow_session_network_host(&id, allow("c3", "b.example.com", None))
        .await
        .unwrap_err();
    assert!(
        matches!(duplicate, DaemonError::SessionConflict(_)),
        "{duplicate}"
    );
    let changed = daemon
        .allow_session_network_host(&id, allow("c6", "b.example.com", None))
        .await
        .unwrap();
    assert_eq!(changed.revision, 4);
    daemon.delete_session(&id).await.unwrap();
    daemon.shutdown().await.unwrap();
}

#[tokio::test]
async fn per_session_allows_and_revokes_are_recorded_and_replayed() {
    const CHILD: &str = "AXOCOATL_TEST_SESSION_NETWORK_EGRESS_CHILD";
    if std::env::var_os(CHILD).is_some() {
        egress_child_body().await;
        return;
    }
    run_child(
        CHILD,
        "bootstrap::session_network_tests::per_session_allows_and_revokes_are_recorded_and_replayed",
    )
    .await;
}

#[tokio::test]
async fn session_network_view_reads_records_and_releases_them_with_the_session() {
    const CHILD: &str = "AXOCOATL_TEST_SESSION_NETWORK_CHILD";
    if std::env::var_os(CHILD).is_some() {
        child_body().await;
        return;
    }
    run_child(
        CHILD,
        "bootstrap::session_network_tests::session_network_view_reads_records_and_releases_them_with_the_session",
    )
    .await;
}

/// Run one test body in a child process with its own data root and a fake
/// Podman, because bootstrap reads the process environment.
async fn run_child(child: &str, name: &str) {
    // Bootstrap owns process environment; isolate it from concurrent tests.
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
        Duration::from_secs(60),
        tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env(child, "1")
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
}
