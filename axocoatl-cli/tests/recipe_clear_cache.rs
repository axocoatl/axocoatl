//! `axocoatl recipe clear-cache` against a fake Podman that logs what it is
//! asked: it removes the toolchain cache volume of one repository, as the
//! daemon of the data directory names it, or with `--all` every one that
//! daemon labelled, and says what to do about a volume a container still
//! uses.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

/// A Podman that answers as a running rootless one, lists `volumes` for
/// the toolchain cache filter of `authority`, and refuses to remove
/// `in_use`, as Podman does for a volume a container uses: the container
/// of Session `ses-0123`.
fn fake_podman(bin: &Path, log: &Path, authority: &str, volumes: &[&str], in_use: &str) {
    std::fs::create_dir_all(bin).unwrap();
    let podman = bin.join("podman");
    std::fs::write(
        &podman,
        format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> "{log}"
case "$*" in
  --version) printf 'podman version 5.0.0\n' ;;
  'machine list --format json') printf '[{{"Running":true}}]\n' ;;
  'info --format json') printf '{{}}\n' ;;
  'info --format {{{{.Host.Security.Rootless}}}}') printf 'true\n' ;;
  'volume ls --filter label=io.axocoatl.role=toolchain-cache --filter label=io.axocoatl.runtime-authority={authority} --format {{{{.Name}}}}')
    printf '%s\n' {volumes} ;;
  'volume ls '*) ;;
  'volume rm -- {in_use}')
    printf 'Error: volume {in_use} is being used by the following container(s): 0123abcd: volume is being used\n' >&2
    exit 2 ;;
  'volume rm -- '*) ;;
  'ps --all --filter volume={in_use} --format {{{{.Names}}}}')
    printf 'axo-ses-ses-0123\n' ;;
  *) printf 'unexpected Podman command: %s\n' "$*" >&2; exit 1 ;;
esac
"#,
            log = log.display(),
            volumes = volumes.join(" "),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&podman, std::fs::Permissions::from_mode(0o700)).unwrap();
}

fn clear_cache(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_axocoatl"))
        .args(["recipe", "clear-cache"])
        .args(args)
        .arg("-c")
        .arg(root.join("axocoatl.yaml"))
        .current_dir(root)
        .env("AXOCOATL_DATA_DIR", root.join("data"))
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", root.join("bin").display()),
        )
        .env_remove("CONTAINER_HOST")
        .env_remove("CONTAINER_CONNECTION")
        .output()
        .unwrap()
}

fn calls(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn clear_cache_removes_one_repositorys_volume_or_every_one() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let data = root.join("data");
    std::fs::create_dir(&data).unwrap();
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).unwrap();
    let (repo, other, bare) = (root.join("repo"), root.join("other"), root.join("bare"));
    for dir in [&repo, &other, &bare] {
        std::fs::create_dir(dir).unwrap();
    }
    let secure = axocoatl_core::SecureDir::open_existing_all(&data).unwrap();
    let authority = axocoatl_daemon::bootstrap::runtime_authority_of(&secure);
    let mine = axocoatl_daemon::bootstrap::toolchain_cache_volume_of(&secure, &repo);
    let theirs = axocoatl_daemon::bootstrap::toolchain_cache_volume_of(&secure, &other);
    assert!(mine.starts_with("axo-tc-") && mine != theirs);
    let log = root.join("podman.log");
    fake_podman(
        &root.join("bin"),
        &log,
        &authority,
        &[&mine, &theirs],
        &theirs,
    );

    // One repository: only its volume, found by this data directory's
    // labels, is removed.
    let output = clear_cache(&root, &["--repo", repo.to_str().unwrap()]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("✓ Removed the toolchain cache {mine}; the next run installs its packages again\n")
    );
    let removed: Vec<String> = calls(&log)
        .into_iter()
        .filter(|call| call.starts_with("volume rm"))
        .collect();
    assert_eq!(removed, [format!("volume rm -- {mine}")]);

    // A repository no run made a cache for: nothing to remove.
    std::fs::write(&log, "").unwrap();
    let output = clear_cache(&root, &["--repo", bare.to_str().unwrap()]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!(
            "No toolchain cache to remove for {}: no run of it has made one\n",
            bare.display()
        )
    );
    assert!(!calls(&log).iter().any(|call| call.starts_with("volume rm")));

    // Every one: a volume a container still uses is left, naming the
    // Session whose container it is and what to do, and the command fails.
    std::fs::write(&log, "").unwrap();
    let output = clear_cache(&root, &["--all"]);
    assert_eq!(output.status.code(), Some(5), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout)
        .contains(&format!("Removed the toolchain cache {mine}")));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        format!(
            "✗ The toolchain cache {theirs} was not removed: the container of Session ses-0123 \
             still uses it. Let the run on this repository end, or close that Session (in the \
             workbench, or `axocoatl session close ses-0123`), then run this again.\n"
        )
    );
    let removed: Vec<String> = calls(&log)
        .into_iter()
        .filter(|call| call.starts_with("volume rm"))
        .collect();
    assert_eq!(
        removed,
        [
            format!("volume rm -- {mine}"),
            format!("volume rm -- {theirs}")
        ]
    );
}
