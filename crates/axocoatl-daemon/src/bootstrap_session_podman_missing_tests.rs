//! A new user's first clicks before Podman can run anything: Create
//! session saves nothing, a Session that never started preparing is
//! stopped and deleted without Podman, shutdown is clean, and a failed
//! environment change leaves the Session as it was instead of "closing".
//! On macOS, Podman here is a fake that is installed but has no machine,
//! and whose container commands fail as they do then. Linux has no machine,
//! and an installed Podman that does not answer stops the daemon from
//! starting at all (it cannot prove that no container exposes the data
//! directory), so there a new user's case is Podman not installed: no
//! `podman` on PATH, not even one the host has.
use super::*;
use axocoatl_session::execution_ownership::DataRootFormatOwnership;
use std::os::unix::fs::PermissionsExt;

const CONFIG: &str = r#"
agents:
  - id: lead
    name: Lead
    provider: ollama
    model: fixture-model
    tools: [read_file]
providers:
  ollama:
    base_url: http://127.0.0.1:1
sandbox:
  backend: podman
  network: none
consolidation:
  enabled: false
"#;

fn session_files(data: &std::path::Path) -> Vec<String> {
    std::fs::read_dir(data.join("sessions"))
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| name.starts_with("ses-") && name.ends_with(".json"))
                .collect()
        })
        .unwrap_or_default()
}

async fn child_body() {
    let config =
        axocoatl_config::parse_config(CONFIG, std::path::Path::new("missing.yaml")).unwrap();
    let daemon = AxocoatlDaemon::bootstrap_headless(config).await.unwrap();
    let data = std::path::PathBuf::from(std::env::var_os("AXOCOATL_DATA_DIR").unwrap());
    let work = tempfile::tempdir().unwrap();
    let workspace = daemon
        .open_workspace(
            &work.path().to_string_lossy(),
            Some("Missing machine"),
            false,
        )
        .await
        .unwrap();

    // Create session from the workbench (setup reviewed, nothing to run):
    // refused before anything is saved, with what to do.
    let refused = daemon
        .create_session_in_workspace(
            &workspace.id,
            "First",
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
        .await
        .expect_err("no machine: refused")
        .to_string();
    assert!(refused.contains("No Session was created"), "{refused}");
    if cfg!(target_os = "macos") {
        assert!(refused.contains("podman machine init"), "{refused}");
    } else {
        assert!(refused.contains("podman"), "{refused}");
    }
    assert!(daemon.list_sessions().await.is_empty());
    assert!(
        session_files(&data).is_empty(),
        "{:?}",
        session_files(&data)
    );

    // A Session waiting for a setup decision needs no Podman yet.
    let waiting = daemon
        .create_session_in_workspace(
            &workspace.id,
            "Waiting",
            SessionMode::SingleAgent {
                agent_id: "lead".into(),
            },
            vec![],
            vec![],
            None,
            Some("npm ci".into()),
            false,
            true,
        )
        .await
        .unwrap();
    assert_eq!(
        waiting.environment.state,
        SessionEnvironmentState::AwaitingApproval
    );

    // A Session that was prepared once, whose container cannot be removed
    // while Podman has no machine: its environment change fails and leaves
    // it as it was, still readable, not "closing".
    let prepared = daemon
        .create_session_in_workspace(
            &workspace.id,
            "Prepared before",
            SessionMode::SingleAgent {
                agent_id: "lead".into(),
            },
            vec![],
            vec![],
            None,
            Some("npm ci".into()),
            false,
            true,
        )
        .await
        .unwrap();
    daemon
        .session_store
        .lock()
        .await
        .set_environment(
            &prepared.id,
            SessionEnvironmentState::Failed,
            Some("docker.io/library/alpine:3.20".into()),
            Some(AxocoatlDaemon::podman_runtime_identity(&prepared.id)),
            vec![],
            Some("an earlier preparation failed".into()),
        )
        .unwrap();
    let before = daemon.get_session(&prepared.id).await.unwrap();
    assert!(matches!(
        &daemon._data_dir_lease.ownership,
        DataRootFormatOwnership::Upgraded(_)
    ));
    let changed = daemon
        .configure_session_environment(&prepared.id, None, None, false, true)
        .await;
    assert!(changed.is_err(), "removing its container needs Podman");
    let after = daemon.get_session(&prepared.id).await.unwrap();
    assert_eq!(after.environment.generation, before.environment.generation);
    assert_eq!(after.environment.state, SessionEnvironmentState::Failed);
    let team = daemon.session_team(&prepared.id).await;
    assert!(
        team.is_ok(),
        "the Session must not be left closing: {:?}",
        team.err()
    );

    // Deleting the Session that never started preparing needs no Podman.
    daemon.delete_session(&waiting.id).await.unwrap();
    assert!(daemon.get_session(&waiting.id).await.is_none());

    // Shutdown stops nothing that never ran. (The Session above still has a
    // container to remove, which shutdown would rightly report; return it to
    // never prepared first.)
    daemon
        .session_store
        .lock()
        .await
        .set_environment(
            &prepared.id,
            SessionEnvironmentState::AwaitingApproval,
            None,
            None,
            vec![],
            None,
        )
        .unwrap();
    daemon.shutdown().await.unwrap();
}

#[tokio::test]
async fn without_a_podman_machine_create_saves_nothing_and_shutdown_is_clean() {
    const CHILD: &str = "AXOCOATL_TEST_PODMAN_MISSING_CHILD";
    if std::env::var_os(CHILD).is_some() {
        child_body().await;
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let bin = root.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let podman = bin.join("podman");
    // Installed, no machine.
    std::fs::write(
        &podman,
        r#"#!/bin/sh
case "$*" in
  --version) printf 'podman version 5.6.0\n' ;;
  'machine list --format json') printf '[]\n' ;;
  *) printf 'Error: unable to connect to Podman socket: no such file or directory\n' >&2; exit 125 ;;
esac
"#,
    )
    .unwrap();
    std::fs::set_permissions(&podman, std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = if cfg!(target_os = "macos") {
        format!("{}:/usr/bin:/bin", bin.display())
    } else {
        crate::podman_absent_test_support::path_without_podman(&root.path().join("system"))
    };
    let data = root.path().join("data");
    let result = tokio::time::timeout(
        Duration::from_secs(120),
        tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "bootstrap::session_podman_missing_tests::without_a_podman_machine_create_saves_nothing_and_shutdown_is_clean",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("AXOCOATL_DATA_DIR", &data)
            .env("AXOCOATL_SOCKET_PATH", root.path().join("ipc/daemon.sock"))
            .env("PATH", path)
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
