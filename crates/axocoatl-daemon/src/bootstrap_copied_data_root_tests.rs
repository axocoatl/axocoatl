//! A data root that was copied or moved while the daemon was stopped, as a
//! migrated Mac found it: 1.4.0 refused to start on it ("failed data-directory
//! format ownership admission"), forever, because its format manifest names
//! the inodes and path of the directory it was written in. The daemon now
//! records the new location at start and keeps every Session.
//!
//! Each daemon runs in a child process (bootstrap reads `AXOCOATL_DATA_DIR`
//! from the process environment) with a fake Podman, so no container starts:
//! each run's environment fails at setup, which still makes its Session,
//! Workspace and run record.
use super::*;
use crate::child_daemon_test_support::{
    audited_ollama, log_warnings_to_stderr, run_child, succeeded,
};
use crate::loadout::api::RunRequest;
use std::os::unix::fs::PermissionsExt;

const CHILD: &str = "AXOCOATL_COPIED_DATA_ROOT_TEST_CHILD";
const TEST: &str =
    "bootstrap::copied_data_root_tests::a_copied_or_moved_data_root_starts_and_keeps_its_sessions";
const MODEL: &str = "copied-model:latest";

const LOADOUT: &str = r#"
schema: axocoatl.loadout/1
id: copied
version: 1
name: Copied
kind: custom
params:
  writer_model: { kind: model, required: true }
agents:
  - id: writer
    role: writer
    model: { param: writer_model }
    tools: [read_file]
budgets:
  agent: { activations: 2, invocations: 20, tokens: 100000, cost_usd: 1 }
  wall_clock: 10m
prompt: "{task}"
"#;

fn run_request(repo: &std::path::Path, id: &str) -> RunRequest {
    RunRequest {
        loadout: "copied".into(),
        task: "Say done.".into(),
        repo: repo.display().to_string(),
        params: [("writer_model".to_string(), format!("ollama:{MODEL}"))]
            .into_iter()
            .collect(),
        keep: Default::default(),
        check_command: None,
        setup_command: None,
        environment: Default::default(),
        request_id: id.into(),
    }
}

/// `cp -a FROM TO`, as the backup page says.
fn copy_tree(from: &std::path::Path, to: &std::path::Path) {
    assert!(std::process::Command::new("cp")
        .arg("-a")
        .arg(from)
        .arg(to)
        .status()
        .unwrap()
        .success());
}

fn ownership_id(data: &std::path::Path) -> String {
    let manifest: serde_json::Value = serde_json::from_slice(
        &std::fs::read(data.join(".axocoatl-daemon.lock/format.json")).unwrap(),
    )
    .unwrap();
    manifest["ownership_id"].as_str().unwrap().to_string()
}

/// One daemon start in a child process: `phase` on data root `data`, with
/// repository `repo`. Returns its output.
async fn child(
    root: &std::path::Path,
    phase: &str,
    data: &std::path::Path,
    repo: &std::path::Path,
) -> std::process::Output {
    let ids = root.join("ids.json");
    run_child(
        TEST,
        CHILD,
        phase,
        root,
        data,
        &[
            ("AXOCOATL_COPIED_DATA_ROOT_REPO", repo.as_os_str()),
            ("AXOCOATL_COPIED_DATA_ROOT_IDS", ids.as_os_str()),
        ],
    )
    .await
}

/// The body each child runs.
async fn phase_body(phase: &str) {
    // The parent reads what the daemon logs.
    log_warnings_to_stderr();
    let server = audited_ollama(&[MODEL]).await;
    let config_dir = tempfile::tempdir().unwrap();
    let config_path = config_dir.path().join("axocoatl.yaml");
    std::fs::write(
        &config_path,
        format!(
            "agents: []\nproviders:\n  ollama:\n    base_url: {}\nsandbox:\n  backend: podman\n  network: bridge\nconsolidation:\n  enabled: false\n",
            server.uri()
        ),
    )
    .unwrap();
    std::fs::create_dir(config_dir.path().join("loadouts")).unwrap();
    std::fs::write(config_dir.path().join("loadouts/copied.yaml"), LOADOUT).unwrap();
    let config = axocoatl_config::load_config(&config_path).await.unwrap();
    let repo =
        std::path::PathBuf::from(std::env::var_os("AXOCOATL_COPIED_DATA_ROOT_REPO").unwrap());
    let ids = std::path::PathBuf::from(std::env::var_os("AXOCOATL_COPIED_DATA_ROOT_IDS").unwrap());
    if phase == "refused" {
        let error = AxocoatlDaemon::bootstrap_headless(config)
            .await
            .err()
            .unwrap();
        assert!(
            error
                .to_string()
                .contains("failed data-directory format ownership admission"),
            "{error}"
        );
        return;
    }
    let daemon = AxocoatlDaemon::bootstrap_headless(config).await.unwrap();
    daemon.set_config_path(&config_path);
    assert!(daemon.uses_native_session_history());
    if phase == "create" {
        let (run, _) = daemon
            .admit_loadout_run(run_request(&repo, "run-original"))
            .await
            .unwrap();
        std::fs::write(
            &ids,
            serde_json::to_vec(&serde_json::json!({
                "session": run.session_id,
                "run": run.run_id,
                "workspace": run.workspace_id,
            }))
            .unwrap(),
        )
        .unwrap();
    } else {
        let ids: serde_json::Value = serde_json::from_slice(&std::fs::read(&ids).unwrap()).unwrap();
        let session = ids["session"].as_str().unwrap();
        // The Session made before the copy, its History, its run and its
        // Workspace are all there.
        let kept = daemon.get_session(session).await.unwrap();
        assert_eq!(kept.workspace_id, ids["workspace"].as_str().unwrap());
        assert!(daemon
            .list_sessions()
            .await
            .iter()
            .any(|listed| listed.id == session));
        daemon.list_session_turns(session).await.unwrap();
        let run = daemon
            .loadout_run(ids["run"].as_str().unwrap())
            .await
            .unwrap();
        assert_eq!(run.session_id, session);
        // A new run is admitted, in the repository this phase names.
        let (next, _) = daemon
            .admit_loadout_run(run_request(&repo, &format!("run-{phase}")))
            .await
            .unwrap();
        let session_next = daemon.get_session(&next.session_id).await.unwrap();
        assert_eq!(session_next.working_dir, repo.canonicalize().unwrap());
        if phase == "copied_repository" {
            // The original repository is still at its path, so the copy is
            // another Workspace; the old Session keeps the original.
            assert_ne!(next.workspace_id, kept.workspace_id);
        } else {
            assert_eq!(next.workspace_id, kept.workspace_id);
        }
    }
    daemon.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_copied_or_moved_data_root_starts_and_keeps_its_sessions() {
    if let Some(phase) = std::env::var_os(CHILD) {
        phase_body(phase.to_str().unwrap()).await;
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    let data = root.join("data");
    std::fs::create_dir(&data).unwrap();
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).unwrap();
    let repo = root.join("outside/repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("README.md"), "# repo\n").unwrap();
    succeeded(&child(&root, "create", &data, &repo).await);
    let id = ownership_id(&data);

    // A cold backup restored in place. The restored copy is made while the
    // original still exists, then put in its place: a filesystem that
    // reuses a deleted directory's inodes at once (ext4) would otherwise
    // give the copy the original's identity.
    let backup = root.join("backup");
    std::fs::create_dir(&backup).unwrap();
    copy_tree(&data, &backup.join("data"));
    let restored = root.join("data.restored");
    copy_tree(&backup.join("data"), &restored);
    std::fs::remove_dir_all(&data).unwrap();
    std::fs::rename(&restored, &data).unwrap();
    let output = succeeded(&child(&root, "restored", &data, &repo).await);
    assert!(
        output.contains("was copied or moved since its ownership was recorded"),
        "{output}"
    );
    assert_eq!(ownership_id(&data), id);
    // Recorded once: the next start says nothing.
    let output = succeeded(&child(&root, "restarted", &data, &repo).await);
    assert!(!output.contains("was copied or moved"), "{output}");

    // The same root moved to another path.
    let moved = root.join("moved");
    std::fs::rename(&data, &moved).unwrap();
    succeeded(&child(&root, "moved", &moved, &repo).await);
    std::fs::rename(&moved, &data).unwrap();

    // A migration: the root and the repository both copied to new paths.
    let migrated = root.join("migrated");
    std::fs::create_dir(&migrated).unwrap();
    copy_tree(&data, &migrated.join("data"));
    copy_tree(&repo, &root.join("outside/repo-copy"));
    succeeded(
        &child(
            &root,
            "copied_repository",
            &migrated.join("data"),
            &root.join("outside/repo-copy"),
        )
        .await,
    );
    assert_eq!(ownership_id(&migrated.join("data")), id);

    // A damaged copy is still refused, and left as it was.
    let damaged = root.join("damaged");
    copy_tree(&data, &damaged);
    let manifest = damaged.join(".axocoatl-daemon.lock/format.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
    value["schema_version"] = 3.into();
    std::fs::write(&manifest, serde_json::to_vec(&value).unwrap()).unwrap();
    let before = std::fs::read(&manifest).unwrap();
    succeeded(&child(&root, "refused", &damaged, &repo).await);
    assert_eq!(std::fs::read(&manifest).unwrap(), before);
}
