// Runtime restoration must release cache guards before teardown or eviction.

#[cfg(unix)]
#[tokio::test]
async fn runtime_restore_releases_primary_and_recovery_cache_before_eviction() {
    use std::os::unix::fs::PermissionsExt;
    const CHILD: &str = "AXOCOATL_TEST_RUNTIME_CACHE_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let mut config = test_config();
        config.agents.clear();
        config.consolidation.enabled = false;
        let daemon = AxocoatlDaemon::bootstrap_headless(config).await.unwrap();
        let work = tempfile::tempdir().unwrap();
        for stale_primary in [false, true] {
            let session = {
                let mut sessions = daemon.session_store.lock().await;
                let session = sessions.create_with_environment(
                    "Runtime restoration cache regression", "wsp-cache-regression", work.path(),
                    SessionMode::SingleAgent { agent_id: "test-agent".into() },
                    vec![], vec![], None, None, false, true,
                ).unwrap();
                // A missing persisted runtime must still fail closed after the
                // stale caches are removed; it must never become a live handle.
                sessions.set_environment(&session.id, SessionEnvironmentState::Ready,
                    None, None, vec![], None).unwrap()
            };
            let sandbox: Arc<dyn Sandbox> = Arc::new(LocalGitSandbox::new(work.path()));
            daemon.attempt_recovery_sandboxes.lock().await
                .insert(session.id.clone(), sandbox.clone());
            if stale_primary {
                daemon.session_sandboxes.lock().await
                    .insert(session.id.clone(), sandbox);
            }
            let result = tokio::time::timeout(Duration::from_secs(2), daemon.ensure_sandbox(&session))
                .await.expect("runtime restoration re-locked a cache it still held");
            let error = result.err().expect("missing runtime must be refused").to_string();
            assert!(error.contains(MISSING_E2B_RUNTIME_ID_ERROR), "{error}");
            assert!(!daemon.session_sandboxes.try_lock().expect("primary cache released")
                .contains_key(&session.id));
            assert!(!daemon.attempt_recovery_sandboxes.try_lock().expect("recovery cache released")
                .contains_key(&session.id));
            assert_eq!(daemon.get_session(&session.id).await.unwrap().environment.state,
                SessionEnvironmentState::Failed);
        }

        // The real Keep cleanup publishes Workspace settlement before this
        // final teardown. If UI restoration already owns the Session start
        // gate, cleanup must wait and then re-read whether recovery still owns
        // that name, rather than deleting its replacement primary sandbox.
        let id = "session-restored-before-recovery-teardown";
        let start = Arc::new(tokio::sync::Mutex::new(()));
        daemon.sandbox_starts.lock().await.insert(id.into(), start.clone());
        let restoration = start.lock().await;
        let recovery: Arc<dyn Sandbox> = Arc::new(LocalGitSandbox::new(work.path()));
        daemon.attempt_recovery_sandboxes.lock().await.insert(id.into(), recovery);
        let cleanup = daemon.stop_attempt_recovery_sandbox_checked(id);
        tokio::pin!(cleanup);
        assert!(tokio::time::timeout(Duration::from_millis(20), &mut cleanup).await.is_err(),
            "cleanup must serialize with the in-progress runtime restoration");
        daemon.attempt_recovery_sandboxes.lock().await.remove(id);
        let replacement: Arc<dyn Sandbox> = Arc::new(LocalGitSandbox::new(work.path()));
        daemon.session_sandboxes.lock().await.insert(id.into(), replacement.clone());
        drop(restoration);
        tokio::time::timeout(Duration::from_secs(2), &mut cleanup).await.unwrap().unwrap();
        assert!(Arc::ptr_eq(daemon.session_sandboxes.lock().await.get(id).unwrap(), &replacement));
        let commands = std::fs::read_to_string(std::env::var_os("AXO_RUNTIME_CACHE_COMMANDS").unwrap()).unwrap();
        assert!(!commands.contains(id), "old recovery cleanup targeted the replacement runtime: {commands}");
        daemon.session_sandboxes.lock().await.remove(id);
        // The intentionally missing durable runtime also prevents shutdown
        // from claiming verified cleanup. Preserve that existing fence.
        let shutdown = daemon.shutdown().await.unwrap_err().to_string();
        assert!(shutdown.contains("durable remote runtime identity is missing"), "{shutdown}");
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let bin = root.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let podman = bin.join("podman");
    std::fs::write(&podman, r#"#!/bin/sh
printf '%s\n' "$*" >> "$AXO_RUNTIME_CACHE_COMMANDS"
case "$*" in
  --version) printf 'podman version 5.0.0\n' ;;
  'machine list --format json') printf '[{"Running":true}]\n' ;;
  'info --format json') printf '{}\n' ;;
  'ps '*|'rm '*) ;;
  *) printf 'unexpected Podman command: %s\n' "$*" >&2; exit 1 ;;
esac
"#).unwrap();
    std::fs::set_permissions(&podman, std::fs::Permissions::from_mode(0o700)).unwrap();
    let result = tokio::time::timeout(Duration::from_secs(60),
        tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "bootstrap::tests::runtime_restore_releases_primary_and_recovery_cache_before_eviction", "--nocapture"])
            .env(CHILD, "1")
            .env("AXOCOATL_DATA_DIR", root.path().join("data"))
            .env("AXOCOATL_SOCKET_PATH", root.path().join("ipc/daemon.sock"))
            .env("AXO_RUNTIME_CACHE_COMMANDS", root.path().join("runtime-commands.txt"))
            .env("PATH", bin).current_dir(root.path()).kill_on_drop(true).output())
        .await.unwrap().unwrap();
    assert!(result.status.success(), "{}\n{}",
        String::from_utf8_lossy(&result.stdout), String::from_utf8_lossy(&result.stderr));
}
