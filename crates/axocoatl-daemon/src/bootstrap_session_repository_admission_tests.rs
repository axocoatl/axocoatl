use super::*;
use axocoatl_session::execution_ownership::DataRootFormatOwnership;
use std::os::unix::fs::PermissionsExt;

#[tokio::test]
async fn native_owner_rejects_ready_e2b_before_runtime_preparation() {
    const CHILD: &str = "AXOCOATL_TEST_NATIVE_E2B_ADMISSION_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let mut config = axocoatl_config::AxocoatlConfig::default();
        config.agents.clear();
        config.consolidation.enabled = false;
        let daemon = AxocoatlDaemon::bootstrap_headless(config).await.unwrap();
        let work = tempfile::tempdir().unwrap();
        let workspace = daemon
            .workspace_store
            .lock()
            .await
            .register(work.path(), Some("Remote admission"))
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
                "Remote admission",
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
        let token = daemon
            .session_dispatch_lifecycles
            .retain_native_session(ownership.clone(), receipt)
            .unwrap();
        let runtime = SessionRuntimeIdentity {
            backend: "e2b".into(),
            id: "remote-admission-runtime".into(),
            remote_root: Some("/home/user/repository".into()),
            control_plane: Some("https://api.e2b.test".into()),
            data_plane_domain: Some("e2b.test".into()),
            authority_fingerprint: None,
            ownership_token: None,
            cleanup_confirmed: false,
        };
        let ready = daemon
            .session_store
            .lock()
            .await
            .set_environment(
                &session.id,
                SessionEnvironmentState::Ready,
                Some("e2b:base".into()),
                Some(runtime.clone()),
                vec![],
                None,
            )
            .unwrap();
        require_session_environment_ready(&ready).unwrap();

        // Runtime restoration must acquire this map before it can reconnect
        // or provision anything. Keep it locked to prove initial native owner
        // admission rejects the backend before entering ensure_sandbox.
        let starts = daemon.sandbox_starts.lock().await;
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            daemon.pending_session_repository_owner(&token),
        )
        .await
        .expect("native E2B rejection entered runtime preparation");
        let error = result
            .err()
            .expect("native E2B owner must be refused")
            .to_string();
        assert!(
            error.contains("requires local Podman process supervision"),
            "{error}"
        );
        assert!(
            error.contains("E2B repository execution is supported only for legacy Sessions"),
            "{error}"
        );
        assert!(starts.is_empty());
        drop(starts);
        assert!(daemon.session_sandboxes.lock().await.is_empty());
        assert!(daemon.attempt_recovery_sandboxes.lock().await.is_empty());
        let after = daemon.get_session(&session.id).await.unwrap();
        assert_eq!(after.environment, ready.environment);
        assert!(daemon
            .attempt_operation_for_workspace(&workspace.id)
            .await
            .try_lock()
            .is_ok());

        // This fixture never created a remote runtime. Retire only its fake
        // durable identity so normal shutdown cannot contact an external API.
        let mut cleaned = runtime;
        cleaned.cleanup_confirmed = true;
        daemon
            .session_store
            .lock()
            .await
            .set_environment(
                &session.id,
                SessionEnvironmentState::Ready,
                Some("e2b:base".into()),
                Some(cleaned),
                vec![],
                None,
            )
            .unwrap();
        daemon.shutdown().await.unwrap();
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
  *) printf 'unexpected Podman command: %s\n' "$*" >&2; exit 1 ;;
esac
"#,
    )
    .unwrap();
    std::fs::set_permissions(&podman, std::fs::Permissions::from_mode(0o700)).unwrap();
    let result = tokio::time::timeout(Duration::from_secs(60),
        tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "bootstrap::session_repository::tests::admission_tests::native_owner_rejects_ready_e2b_before_runtime_preparation", "--nocapture"])
            .env(CHILD, "1")
            .env("AXOCOATL_DATA_DIR", root.path().join("data"))
            .env("AXOCOATL_SOCKET_PATH", root.path().join("ipc/daemon.sock"))
            .env("PATH", bin).current_dir(root.path()).kill_on_drop(true).output())
        .await.unwrap().unwrap();
    assert!(
        result.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}
