//! `axocoatl run --repo` keeps to the Workspace scope rules, end to end: a
//! real `axocoatl serve` on a loopback port refuses a run in its own data
//! directory or in the file system root with exit code 3 and the reason,
//! and creates no Workspace.
//!
//! The daemon starts with no agents, so it needs no model download. It still
//! probes the configured Podman connection at startup; on a Mac, set
//! CONTAINER_CONNECTION to a running machine.
#![cfg(unix)]

mod support;

use std::path::Path;
use std::process::Output;

use support::{axocoatl, client, free_port, Daemon};

fn run(root: &Path, config: &Path, port: u16, repo: &Path) -> Output {
    axocoatl(root)
        .args(["run", "fix", "--task", "Fix the failing test.", "--repo"])
        .arg(repo)
        .args([
            "--model",
            "writer=ollama:writer-model:latest",
            "--model",
            "reviewer=ollama:reviewer-model:latest",
            "--check",
            "true",
            "--url",
            &format!("http://127.0.0.1:{port}"),
            "-c",
        ])
        .arg(config)
        .output()
        .unwrap()
}

#[tokio::test]
async fn a_run_in_axocoatls_own_directory_or_the_root_is_refused() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let port = free_port();
    let config = root.join("axocoatl.yaml");
    std::fs::write(
        &config,
        format!(
            "agents: []\nserver:\n  host: 127.0.0.1\n  port: {port}\nsandbox:\n  network: none\nconsolidation:\n  enabled: false\n"
        ),
    )
    .unwrap();
    let mut daemon = Daemon::start(&root, &config, port, 0).await;
    let inside = root.join("data").join("inside");
    std::fs::create_dir(&inside).unwrap();

    for (repo, says) in [
        (inside.as_path(), "is inside Axocoatl's data directory"),
        (Path::new("/"), "is the root of the file system"),
    ] {
        let output = run(&root, &config, port, repo);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(3), "{stderr}");
        assert!(output.stdout.is_empty(), "{output:?}");
        assert!(stderr.starts_with("axocoatl run: "), "{stderr}");
        assert!(stderr.contains(says), "{stderr}");
        assert!(!stderr.contains("· run "), "{stderr}");
    }

    let token = std::fs::read_to_string(root.join("data").join("local-api-token"))
        .unwrap()
        .trim()
        .to_string();
    let workspaces: serde_json::Value = client(port)
        .get(format!("http://127.0.0.1:{port}/api/workspaces"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(workspaces, serde_json::json!([]), "{workspaces}");
    daemon.stop();
}
