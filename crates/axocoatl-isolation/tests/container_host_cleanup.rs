//! The daemon's start removes the Session containers that expose its data
//! root and the ones its runtime authority owns. With Podman reached through
//! `CONTAINER_HOST` (no `podman machine` listed), that cleanup asks the
//! Podman `CONTAINER_HOST` names, and fails closed when it does not answer.
//! It never returns "nothing to remove" without asking: 1.5.0 did, on macOS,
//! because it took the missing default machine for a dormant Podman.
#![cfg(unix)]

use axocoatl_isolation::SessionSandbox;
use std::{os::unix::fs::PermissionsExt, path::PathBuf, time::Duration};

const MODE: &str = "AXOCOATL_CONTAINER_HOST_CLEANUP_MODE";
const HOST: &str = "unix:///run/axocoatl-test/podman-api.sock";

#[tokio::test]
async fn startup_cleanup_asks_the_podman_container_host_names() {
    if let Ok(mode) = std::env::var(MODE) {
        let root = PathBuf::from(std::env::var_os("AXOCOATL_CH_ROOT").unwrap());
        let authority = "e".repeat(64);
        let exposing =
            SessionSandbox::remove_data_root_exposing_containers(&root, &authority).await;
        let owned = SessionSandbox::remove_owned_containers(&authority).await;
        if mode == "answers" {
            assert_eq!(exposing.unwrap(), vec!["a".repeat(64)]);
            assert_eq!(owned.unwrap(), vec!["axo-ses-owned".to_string()]);
        } else {
            let exposing =
                exposing.expect_err("an unanswered CONTAINER_HOST proves nothing absent");
            let owned = owned.expect_err("an unanswered CONTAINER_HOST proves nothing absent");
            for error in [exposing.to_string(), owned.to_string()] {
                assert!(
                    error.contains(&format!("CONTAINER_HOST={HOST} is set")),
                    "{error}"
                );
            }
        }
        return;
    }

    for mode in ["answers", "down"] {
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let program = bin.join("podman");
        std::fs::write(
            &program,
            r#"#!/bin/sh
printf '%s\n' "$*" >> "$AXOCOATL_CH_CALLS"
case "$*" in
  --version) printf 'podman version 6.1.4\n' ;;
  'machine list --format json') printf '[]\n' ;;
  'info --format json')
    if [ -n "$CONTAINER_HOST" ] && [ "$AXOCOATL_CONTAINER_HOST_CLEANUP_MODE" = answers ]; then
      printf '{}\n'
    else
      printf 'Cannot connect to Podman\n' >&2; exit 125
    fi ;;
  'ps -a --no-trunc --filter name=axo-ses- --format '*)
    printf '%s\taxo-ses-exposing\n' "$EXPOSING_ID" ;;
  'ps -a --no-trunc '*'--filter label='*)
    printf '%s\taxo-ses-owned\n' "$OWNED_ID" ;;
  'inspect --type container --format json '*)
    printf '[{"Id":"%s","Mounts":[{"Type":"bind","Source":"%s"}]}]\n' "$EXPOSING_ID" "$AXOCOATL_CH_ROOT" ;;
  'rm '*) exit 0 ;;
  *) printf 'unexpected command: %s\n' "$*" >&2; exit 125 ;;
esac
"#,
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let calls = root.path().join("calls");
        let output = tokio::time::timeout(
            Duration::from_secs(60),
            tokio::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "startup_cleanup_asks_the_podman_container_host_names",
                    "--nocapture",
                ])
                .env(MODE, mode)
                .env("AXOCOATL_CH_ROOT", root.path())
                .env("AXOCOATL_CH_CALLS", &calls)
                .env("EXPOSING_ID", "a".repeat(64))
                .env("OWNED_ID", "b".repeat(64))
                .env("PATH", &bin)
                .env_remove("CONTAINER_CONNECTION")
                .env("CONTAINER_HOST", HOST)
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            output.status.success(),
            "{mode}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let calls = std::fs::read_to_string(calls).unwrap();
        assert!(
            !calls.lines().any(|line| line.starts_with("machine ")),
            "{mode}: a CONTAINER_HOST setup never asks about machines:\n{calls}"
        );
        let removals = calls
            .lines()
            .filter(|line| line.starts_with("rm "))
            .collect::<Vec<_>>();
        if mode == "answers" {
            assert_eq!(removals.len(), 2, "{calls}");
            assert!(removals.iter().any(|line| line.ends_with(&"a".repeat(64))));
            assert!(removals.iter().any(|line| line.ends_with(&"b".repeat(64))));
        } else {
            assert!(removals.is_empty(), "{calls}");
            assert!(
                !calls.lines().any(|line| line.starts_with("ps ")),
                "{calls}"
            );
        }
    }
}
