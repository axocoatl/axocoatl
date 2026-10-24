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

/// A fake rootless Podman that starts no container. `volume ls` lists
/// `volumes.json` beside `bin` (none when it is missing); `ps` with a
/// runtime authority label and the SearXNG role label lists
/// `ps-<authority>-searxng`; each removal is appended to `removed`.
fn fake_podman(root: &std::path::Path) -> String {
    let bin = root.join("bin");
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
  'info --format {{{{.Host.Security.Rootless}}}}') printf 'true\n' ;;
  'ps '*)
    authority=; role=
    for arg in "$@"; do
      case "$arg" in
        label=io.axocoatl.runtime-authority=*) authority=${{arg#*=}}; authority=${{authority#*=}} ;;
        label=io.axocoatl.role=searxng) role=-searxng ;;
      esac
    done
    if [ -n "$authority" ] && [ -n "$role" ]; then cat "{root}/ps-$authority$role" 2>/dev/null; fi
    true ;;
  'volume ls '*) cat '{volumes}' 2>/dev/null || printf '[]\n' ;;
  'rm '*|'volume rm '*|'network rm '*) echo "$*" >> '{removed}' ;;
  *) printf 'unexpected Podman command: %s\n' "$*" >&2; exit 1 ;;
esac
"#,
            root = root.display(),
            volumes = root.join("volumes.json").display(),
            removed = root.join("removed").display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&podman, std::fs::Permissions::from_mode(0o700)).unwrap();
    format!("{}:/usr/bin:/bin", bin.display())
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

/// The runtime authorities `previous-runtime-authorities.json` in `data`
/// keeps, and whether it names `data` as the directory it was written in.
fn kept_previous(data: &std::path::Path) -> (Vec<String>, bool) {
    let kept: serde_json::Value = serde_json::from_slice(
        &std::fs::read(data.join("previous-runtime-authorities.json")).unwrap(),
    )
    .unwrap();
    let here = SecureDir::open(data).unwrap().inode_identity().unwrap();
    (
        serde_json::from_value(kept["authorities"].clone()).unwrap(),
        kept["directory"] == here.as_str(),
    )
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
    // This test's Podman lists volumes and SearXNG containers and records
    // removals; `run_child` keeps a `podman` already written.
    if !root.join("bin/podman").exists() {
        fake_podman(root);
    }
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
    let closed = ids.with_file_name("closed");
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
        if phase == "moved" {
            // A closed Session, for the copy made later.
            daemon.close_session(&next.session_id).await.unwrap();
            std::fs::write(&closed, &next.session_id).unwrap();
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

    // The same root moved to another path. Its Sessions' Podman volumes
    // carry the runtime authority of the path it had, which it keeps while
    // one of them does (an open Session's here), and then forgets.
    let before = local_runtime_authority(&SecureDir::open(&data).unwrap()).unwrap();
    let session: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("ids.json")).unwrap()).unwrap();
    std::fs::write(
        root.join("volumes.json"),
        serde_json::to_vec(&serde_json::json!([{
            "Name": format!("axo-egr-{}", session["session"].as_str().unwrap()),
            "Labels": {"io.axocoatl.runtime-authority": before},
        }]))
        .unwrap(),
    )
    .unwrap();
    // A daemon killed there left its SearXNG, which carries it too: removed
    // once it has exited. A running one is left while a daemon runs at that
    // path (a data root made there since, which holds the path's lease
    // here), and the authority kept for it; removed once none does.
    std::fs::write(
        root.join(format!("ps-{before}-searxng")),
        "axo-searxng-exited\texited\naxo-searxng-running\trunning\n",
    )
    .unwrap();
    let at_old_path = axocoatl_session::execution_ownership::PathLease::hold(&before)
        .unwrap()
        .unwrap();
    let moved = root.join("moved");
    std::fs::rename(&data, &moved).unwrap();
    succeeded(&child(&root, "moved", &moved, &repo).await);
    let previous = moved.join("previous-runtime-authorities.json");
    assert_eq!(kept_previous(&moved), (vec![before.clone()], true));
    let removed = std::fs::read_to_string(root.join("removed")).unwrap();
    assert!(removed.contains(" axo-searxng-exited\n"), "{removed}");
    assert!(!removed.contains("axo-searxng-running"), "{removed}");
    // The volume goes; the running SearXNG keeps the authority.
    std::fs::remove_file(root.join("volumes.json")).unwrap();
    succeeded(&child(&root, "restarted", &moved, &repo).await);
    assert!(previous.exists());
    drop(at_old_path);
    succeeded(&child(&root, "restarted", &moved, &repo).await);
    let removed = std::fs::read_to_string(root.join("removed")).unwrap();
    assert!(removed.contains(" axo-searxng-running\n"), "{removed}");
    assert!(!previous.exists());
    std::fs::remove_file(root.join(format!("ps-{before}-searxng"))).unwrap();
    // What its volumes carry from now on, until a daemon starts on it at
    // another path.
    let original = local_runtime_authority(&SecureDir::open(&moved).unwrap()).unwrap();
    std::fs::rename(&moved, &data).unwrap();
    // What the original was moved from is not a copy's: a copy forgets it.
    let original_previous = serde_json::to_vec(&serde_json::json!({
        "directory": SecureDir::open(&data).unwrap().inode_identity().unwrap(),
        "authorities": ["0".repeat(64)],
    }))
    .unwrap();
    std::fs::write(
        data.join("previous-runtime-authorities.json"),
        &original_previous,
    )
    .unwrap();

    // A migration: the root and the repository both copied to new paths.
    // The copy's Sessions are the original's too (the same ids), and the
    // original, which may still run, owns the volumes that carry its
    // authority: the copy never removes one, an open Session's nor a closed
    // one's, and keeps none of the original's authorities as its own, only
    // as the ones it was copied from, while a volume carries one.
    let closed = std::fs::read_to_string(root.join("closed")).unwrap();
    let open_volume = format!("axo-egr-{}", session["session"].as_str().unwrap());
    let closed_volume = format!("axo-egi-{closed}");
    std::fs::write(
        root.join("volumes.json"),
        serde_json::to_vec(&serde_json::json!([
            {"Name": open_volume, "Labels": {"io.axocoatl.runtime-authority": original}},
            {"Name": closed_volume, "Labels": {"io.axocoatl.runtime-authority": original}},
        ]))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(root.join("removed"), "").unwrap();
    // The volumes removed so far (each start also removes its Sessions'
    // containers by name, and an egress container is named as its volume).
    let removed_volumes = || {
        std::fs::read_to_string(root.join("removed"))
            .unwrap()
            .lines()
            .filter(|line| line.starts_with("volume rm "))
            .flat_map(|line| line.split(' ').skip(2).map(str::to_string))
            .collect::<Vec<_>>()
    };
    let migrated = root.join("migrated");
    std::fs::create_dir(&migrated).unwrap();
    copy_tree(&data, &migrated.join("data"));
    copy_tree(&repo, &root.join("outside/repo-copy"));
    let copy_start = || async {
        succeeded(
            &child(
                &root,
                "copied_repository",
                &migrated.join("data"),
                &root.join("outside/repo-copy"),
            )
            .await,
        )
    };
    copy_start().await;
    assert_eq!(ownership_id(&migrated.join("data")), id);
    assert!(!migrated
        .join("data/previous-runtime-authorities.json")
        .exists());
    let removed = removed_volumes();
    assert!(
        !removed.contains(&open_volume) && !removed.contains(&closed_volume),
        "{removed:?}"
    );
    let copied_from = migrated.join("data/copied-from-runtime-authorities.json");
    assert_eq!(
        serde_json::from_slice::<Vec<String>>(&std::fs::read(&copied_from).unwrap()).unwrap(),
        std::slice::from_ref(&original)
    );
    // `session record-location --original-deleted` while a daemon runs on
    // the original (it holds its path's lease): it was not deleted, and its
    // authority stays the original's.
    let original_runs = axocoatl_session::execution_ownership::PathLease::hold(&original)
        .unwrap()
        .unwrap();
    let recorded = AxocoatlDaemon::record_data_root_location(&migrated.join("data"), true).unwrap();
    assert_eq!(
        (recorded.location, recorded.adopted, recorded.in_use),
        (None, Some(0), 1)
    );
    assert_eq!(
        serde_json::from_slice::<Vec<String>>(&std::fs::read(&copied_from).unwrap()).unwrap(),
        std::slice::from_ref(&original)
    );
    assert!(!migrated
        .join("data/previous-runtime-authorities.json")
        .exists());
    drop(original_runs);
    // The original was deleted (a move by copying): `session record-location
    // --original-deleted` makes its authority this copy's previous one, as a
    // move's, and the next start removes the closed Session's volume and
    // keeps the open one's.
    let recorded = AxocoatlDaemon::record_data_root_location(&migrated.join("data"), true).unwrap();
    assert_eq!(recorded.location, None);
    assert_eq!((recorded.adopted, recorded.in_use), (Some(1), 0));
    assert!(!copied_from.exists());
    copy_start().await;
    let removed = removed_volumes();
    assert!(
        removed.contains(&closed_volume) && !removed.contains(&open_volume),
        "{removed:?}"
    );
    assert_eq!(
        kept_previous(&migrated.join("data")),
        (vec![original.clone()], true)
    );

    // A copy whose first start recorded its location and stopped before it
    // moved the original's authorities (the file still names the original's
    // directory): it still never takes them for its own.
    let stopped = root.join("stopped");
    std::fs::create_dir(&stopped).unwrap();
    copy_tree(&data, &stopped.join("data"));
    let stopped = stopped.join("data");
    AxocoatlDaemon::record_data_root_location(&stopped, false).unwrap();
    std::fs::write(
        stopped.join("previous-runtime-authorities.json"),
        serde_json::to_vec(&serde_json::json!({
            "directory": SecureDir::open(&data).unwrap().inode_identity().unwrap(),
            "authorities": [original],
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::remove_file(stopped.join("copied-from-runtime-authorities.json")).unwrap();
    std::fs::write(root.join("removed"), "").unwrap();
    succeeded(
        &child(
            &root,
            "copied_repository",
            &stopped,
            &root.join("outside/repo-copy"),
        )
        .await,
    );
    let removed = removed_volumes();
    assert!(
        !removed.contains(&open_volume) && !removed.contains(&closed_volume),
        "{removed:?}"
    );
    assert!(!stopped.join("previous-runtime-authorities.json").exists());
    assert_eq!(
        serde_json::from_slice::<Vec<String>>(
            &std::fs::read(stopped.join("copied-from-runtime-authorities.json")).unwrap()
        )
        .unwrap(),
        std::slice::from_ref(&original)
    );
    std::fs::remove_file(root.join("volumes.json")).unwrap();

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
