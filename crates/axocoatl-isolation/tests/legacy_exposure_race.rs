//! Exercise startup inventory through the real CLI boundary in isolated children.
#![cfg(unix)]

use axocoatl_isolation::SessionSandbox;
use std::{os::unix::fs::PermissionsExt, path::PathBuf, time::Duration};

#[tokio::test]
async fn legacy_inventory_reconciles_only_proven_disappearance() {
    const MODE: &str = "AXOCOATL_EXPOSURE_RACE_MODE";
    if let Ok(mode) = std::env::var(MODE) {
        let root = PathBuf::from(std::env::var_os("AXOCOATL_EXPOSURE_ROOT").unwrap());
        let result =
            SessionSandbox::remove_data_root_exposing_containers(&root, &"e".repeat(64)).await;
        if mode == "absent" {
            assert_eq!(result.unwrap(), vec!["b".repeat(64)]);
        } else {
            assert!(
                result.is_err(),
                "uncertain or still-present IDs cannot be ignored"
            );
        }
        return;
    }

    for mode in ["absent", "uncertain", "present"] {
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let program = bin.join("podman");
        std::fs::write(&program, r#"#!/bin/sh
printf '%s\n' "$*" >> "$AXOCOATL_EXPOSURE_CALLS"
case "$*" in
  --version) printf 'podman version 5.0.0\n' ;;
  'machine list --format json') printf '[{"Running":true}]\n' ;;
  'info --format json') printf '{}\n' ;;
  'ps -a --no-trunc --filter name=axo-ses- --format '*)
    printf '%s\taxo-ses-gone\n%s\taxo-ses-retained\n' "$GONE_ID" "$KEPT_ID" ;;
  'inspect --type container --format json '*)
    if [ "$6" = "$KEPT_ID" ] && [ "$#" -eq 6 ]; then
      printf '[{"Id":"%s","Mounts":[{"Type":"bind","Source":"%s"}]}]\n' "$KEPT_ID" "$AXOCOATL_EXPOSURE_ROOT"
    else
      printf 'inspection failed during concurrent cleanup\n' >&2; exit 125
    fi ;;
  'container exists '*)
    if [ "$3" = "$KEPT_ID" ]; then exit 0; fi
    case "$AXOCOATL_EXPOSURE_RACE_MODE" in
      absent) exit 1 ;;
      uncertain) printf 'engine unavailable\n' >&2; exit 125 ;;
      present) exit 0 ;;
    esac ;;
  'rm '*)
    case "$*" in *"$KEPT_ID") exit 0 ;; esac
    printf 'wrong removal target\n' >&2; exit 125 ;;
  *) printf 'unexpected command: %s\n' "$*" >&2; exit 125 ;;
esac
"#).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let calls = root.path().join("calls");
        let output = tokio::time::timeout(
            Duration::from_secs(60),
            tokio::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "legacy_inventory_reconciles_only_proven_disappearance",
                    "--nocapture",
                ])
                .env(MODE, mode)
                .env("AXOCOATL_EXPOSURE_ROOT", root.path())
                .env("AXOCOATL_EXPOSURE_CALLS", &calls)
                .env("GONE_ID", "a".repeat(64))
                .env("KEPT_ID", "b".repeat(64))
                .env("PATH", &bin)
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
        assert_eq!(
            calls
                .lines()
                .filter(|line| line.starts_with("inspect "))
                .count(),
            if mode == "absent" { 2 } else { 1 }
        );
        let removals = calls
            .lines()
            .filter(|line| line.starts_with("rm "))
            .collect::<Vec<_>>();
        assert_eq!(removals.len(), usize::from(mode == "absent"));
        assert!(removals.iter().all(|line| line.ends_with(&"b".repeat(64))));
    }
}
