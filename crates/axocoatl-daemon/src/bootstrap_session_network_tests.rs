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

async fn child_body() {
    let mut config = axocoatl_config::AxocoatlConfig::default();
    config.agents.clear();
    config.consolidation.enabled = false;
    let daemon = AxocoatlDaemon::bootstrap_headless(config).await.unwrap();
    let work = tempfile::tempdir().unwrap();
    let workspace = daemon
        .workspace_store
        .lock()
        .await
        .register(work.path(), Some("Network record"))
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
    let id = session.id.clone();

    // A Session that never had network activity reads as empty, and the read
    // creates nothing.
    let view = daemon.session_network(&id, None, None).await.unwrap();
    assert_eq!(view.mode, "bridge");
    assert!(view.events.is_empty() && view.policies.is_empty() && view.sidecar.is_none());
    assert_eq!(view.record.events, 0);
    assert_eq!(view.record.max_events, 50_000);
    assert_eq!(view.next_after, None);
    assert!(!daemon.session_network_records.is_open(&id).await);

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
        .append_control(
            &id,
            NetworkEvent::Limit {
                what: LimitKind::RecordFull,
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

#[tokio::test]
async fn session_network_view_reads_records_and_releases_them_with_the_session() {
    const CHILD: &str = "AXOCOATL_TEST_SESSION_NETWORK_CHILD";
    if std::env::var_os(CHILD).is_some() {
        child_body().await;
        return;
    }
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
            .args([
                "--exact",
                "bootstrap::session_network_tests::session_network_view_reads_records_and_releases_them_with_the_session",
                "--nocapture",
            ])
            .env(CHILD, "1")
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
