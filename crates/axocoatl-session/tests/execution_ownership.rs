#![cfg(unix)]

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};

use axocoatl_core::SecureDir;
use axocoatl_session::execution_ownership::{
    DataRootFormatOwnership, LegacyFormatOwnership, OwnershipError, UpgradedFormatOwnership,
    LEGACY_LOCK_NAME, RETIRED_LOCK_NAME,
};
use sha2::{Digest, Sha256};

fn legacy_lock(root: &Path) -> File {
    let file = SecureDir::open(root)
        .unwrap()
        .open_lock_file(LEGACY_LOCK_NAME)
        .unwrap();
    flock(&file);
    file
}

fn flock(file: &File) {
    unsafe extern "C" {
        fn flock(fd: i32, operation: i32) -> i32;
    }
    // SAFETY: the File owns a retained valid descriptor.
    assert_eq!(unsafe { flock(file.as_raw_fd(), 2 | 4) }, 0);
}

fn external_path(root: &Path) -> PathBuf {
    let root = SecureDir::open_existing_all(root).unwrap();
    let bytes = root.path().as_os_str().as_encoded_bytes();
    #[cfg(target_os = "macos")]
    let bytes = {
        use unicode_normalization::UnicodeNormalization;
        String::from_utf8_lossy(bytes)
            .nfd()
            .flat_map(char::to_lowercase)
            .collect::<String>()
            .into_bytes()
    };
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    // SAFETY: geteuid has no arguments or failure sentinel.
    let uid = unsafe { geteuid() };
    PathBuf::from(format!("/tmp/axocoatl-daemon-leases-{uid}"))
        .join(format!("{:x}.lock", Sha256::digest(bytes)))
}

fn inventory(root: &Path) -> BTreeMap<PathBuf, (u32, Vec<u8>)> {
    fn walk(root: &Path, dir: &Path, output: &mut BTreeMap<PathBuf, (u32, Vec<u8>)>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let metadata = path.symlink_metadata().unwrap();
            let bytes = if metadata.is_file() {
                fs::read(&path).unwrap()
            } else if metadata.file_type().is_symlink() {
                fs::read_link(&path)
                    .unwrap()
                    .as_os_str()
                    .as_encoded_bytes()
                    .to_vec()
            } else {
                Vec::new()
            };
            output.insert(
                path.strip_prefix(root).unwrap().to_owned(),
                (metadata.mode(), bytes),
            );
            if metadata.is_dir() {
                walk(root, &path, output);
            }
        }
    }
    let mut output = BTreeMap::new();
    walk(root, root, &mut output);
    output
}

#[test]
fn installed_fence_preserves_original_inode_and_reopens_with_same_identity() {
    let root = tempfile::tempdir().unwrap();
    let legacy = LegacyFormatOwnership::acquire(root.path()).unwrap();
    let old_inode = fs::metadata(root.path().join(LEGACY_LOCK_NAME))
        .unwrap()
        .ino();
    let upgraded = legacy.upgrade().unwrap();
    let identity = upgraded.manifest().clone();
    assert_eq!(identity.schema_version, 2);
    assert!(root.path().join(LEGACY_LOCK_NAME).is_dir());
    assert_eq!(
        fs::metadata(root.path().join(RETIRED_LOCK_NAME))
            .unwrap()
            .ino(),
        old_inode
    );
    assert!(!root.path().join("execution").exists());
    // Exact behavior invoked by the old bootstrap before reconciliation.
    assert!(SecureDir::open(root.path())
        .unwrap()
        .is_file(LEGACY_LOCK_NAME)
        .is_err());
    assert!(matches!(
        UpgradedFormatOwnership::open(root.path()),
        Err(OwnershipError::Busy(_))
    ));
    drop(upgraded);
    let reopened = UpgradedFormatOwnership::open(root.path()).unwrap();
    assert_eq!(reopened.manifest(), &identity);
    assert!(LegacyFormatOwnership::acquire(root.path()).is_err());
}

#[test]
fn format_aware_owner_consumes_the_held_legacy_lease_and_reopens_installed_identity() {
    let root = tempfile::tempdir().unwrap();
    let opened = SecureDir::open_existing_all(root.path()).unwrap();
    let owner = DataRootFormatOwnership::acquire_for_root(&opened).unwrap();
    assert!(matches!(&owner, DataRootFormatOwnership::Legacy(_)));
    owner.verify_root(&opened).unwrap();
    let external = owner.external_root().inode_identity().unwrap();
    let legacy_inode = fs::metadata(root.path().join(LEGACY_LOCK_NAME))
        .unwrap()
        .ino();
    assert!(matches!(
        DataRootFormatOwnership::acquire_for_root(&opened),
        Err(OwnershipError::Busy(_))
    ));
    let installed = owner.into_upgraded().unwrap();
    let identity = installed.manifest().clone();
    assert_eq!(
        fs::metadata(root.path().join(RETIRED_LOCK_NAME))
            .unwrap()
            .ino(),
        legacy_inode
    );
    assert!(matches!(
        DataRootFormatOwnership::acquire_for_root(&opened),
        Err(OwnershipError::Busy(_))
    ));
    let retained = installed.clone();
    drop(installed);
    assert!(DataRootFormatOwnership::acquire_for_root(&opened).is_err());
    drop(retained);
    let reopened = DataRootFormatOwnership::acquire_for_root(&opened).unwrap();
    assert!(matches!(&reopened, DataRootFormatOwnership::Upgraded(_)));
    assert_eq!(reopened.external_root().inode_identity().unwrap(), external);
    reopened.verify_root(&opened).unwrap();
    assert_eq!(reopened.into_upgraded().unwrap().manifest(), &identity);
}

#[test]
fn format_dispatcher_accepts_a_directly_opened_dot_spelling_of_the_same_root() {
    let root = tempfile::tempdir().unwrap();
    let supplied = SecureDir::open(root.path().join(".")).unwrap();
    let normalized = SecureDir::open_existing_all(root.path()).unwrap();
    let owner = DataRootFormatOwnership::acquire_for_root(&supplied).unwrap();
    owner.verify_root(&supplied).unwrap();
    owner.verify_root(&normalized).unwrap();
    assert!(matches!(
        DataRootFormatOwnership::acquire_for_root(&normalized),
        Err(OwnershipError::Busy("controller (external lease)"))
    ));
}

#[cfg(target_os = "macos")]
#[test]
fn format_dispatcher_accepts_var_alias_but_refuses_its_replaced_root() {
    let root = tempfile::tempdir_in("/var/tmp").unwrap();
    let supplied = SecureDir::open(root.path()).unwrap();
    let normalized = SecureDir::open_existing_all(root.path()).unwrap();
    assert!(supplied.path().starts_with("/var/tmp"));
    assert!(normalized.path().starts_with("/private/var/tmp"));
    assert_eq!(
        supplied.inode_identity().unwrap(),
        normalized.inode_identity().unwrap()
    );

    let owner = DataRootFormatOwnership::acquire_for_root(&supplied).unwrap();
    owner.verify_root(&supplied).unwrap();
    owner.verify_root(&normalized).unwrap();
    // The old process protocol uses the normalized external key even when the
    // bootstrap capability retains the /var spelling.
    assert!(matches!(
        DataRootFormatOwnership::acquire_for_root(&normalized),
        Err(OwnershipError::Busy("controller (external lease)"))
    ));

    let installed = owner.into_upgraded().unwrap();
    let manifest = installed.manifest().clone();
    assert_eq!(
        manifest.root_authority_sha256,
        external_path(root.path())
            .file_stem()
            .unwrap()
            .to_str()
            .unwrap()
    );
    drop(installed);
    let reopened = DataRootFormatOwnership::acquire_for_root(&supplied).unwrap();
    reopened.verify_root(&normalized).unwrap();
    assert_eq!(reopened.into_upgraded().unwrap().manifest(), &manifest);

    // The same alias spelling must not authorize the replacement inode.
    let moved = root.path().with_extension("retained");
    fs::rename(root.path(), &moved).unwrap();
    fs::create_dir(root.path()).unwrap();
    assert!(DataRootFormatOwnership::acquire_for_root(&supplied).is_err());
    assert!(inventory(root.path()).is_empty());
    fs::remove_dir(root.path()).unwrap();
    fs::rename(moved, root.path()).unwrap();
}

#[test]
fn format_dispatcher_resumes_both_existing_process_crash_shapes_under_one_owner() {
    for phase in ["prepared", "exchanged"] {
        let root = tempfile::tempdir().unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "ownership_crash_child", "--ignored"])
            .env("AXO_OWNERSHIP_CRASH_ROOT", root.path())
            .env("AXO_OWNERSHIP_CRASH_PHASE", phase)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(42));
        let opened = SecureDir::open_existing_all(root.path()).unwrap();
        let owner = DataRootFormatOwnership::acquire_for_root(&opened).unwrap();
        assert_eq!(
            matches!(&owner, DataRootFormatOwnership::Legacy(_)),
            phase == "prepared"
        );
        assert!(matches!(
            DataRootFormatOwnership::acquire_for_root(&opened),
            Err(OwnershipError::Busy(_))
        ));
        let installed = owner.into_upgraded().unwrap();
        assert_eq!(installed.manifest().schema_version, 2);
    }
}

#[test]
fn format_dispatcher_rejects_incomplete_preparation_before_returning_legacy_authority() {
    let root = tempfile::tempdir().unwrap();
    let opened = SecureDir::open_existing_all(root.path()).unwrap();
    drop(opened.open_lock_file(LEGACY_LOCK_NAME).unwrap());
    opened.create_child(RETIRED_LOCK_NAME).unwrap();
    let before = inventory(root.path());
    assert!(DataRootFormatOwnership::acquire_for_root(&opened).is_err());
    assert_eq!(inventory(root.path()), before);
}

#[test]
fn format_dispatcher_refuses_replaced_supplied_root_before_creating_any_lock_there() {
    let parent = tempfile::tempdir().unwrap();
    let path = parent.path().join("data");
    fs::create_dir(&path).unwrap();
    let opened = SecureDir::open_existing_all(&path).unwrap();
    fs::rename(&path, parent.path().join("original")).unwrap();
    fs::create_dir(&path).unwrap();
    assert!(DataRootFormatOwnership::acquire_for_root(&opened).is_err());
    assert!(inventory(&path).is_empty());
}

#[test]
fn exact_external_lease_excludes_conversion_before_any_candidate_write() {
    let root = tempfile::tempdir().unwrap();
    let external = external_path(root.path());
    let parent = SecureDir::open_or_create_all(external.parent().unwrap()).unwrap();
    let file = parent
        .open_lock_file(external.file_name().unwrap())
        .unwrap();
    flock(&file);
    assert!(matches!(
        LegacyFormatOwnership::acquire(root.path()),
        Err(OwnershipError::Busy("controller (external lease)"))
    ));
    assert!(inventory(root.path()).is_empty());
}

#[test]
fn active_legacy_lock_owner_cannot_be_overridden_by_a_migration_request() {
    let root = tempfile::tempdir().unwrap();
    let owner = legacy_lock(root.path());
    let before = inventory(root.path());
    assert!(matches!(
        LegacyFormatOwnership::acquire(root.path()),
        Err(OwnershipError::Busy("legacy controller (in-root lease)"))
    ));
    assert_eq!(inventory(root.path()), before);
    drop(owner);
    LegacyFormatOwnership::acquire(root.path())
        .unwrap()
        .upgrade()
        .unwrap();
}

#[test]
fn independently_opened_root_lock_excludes_even_replaced_lock_files() {
    let root = tempfile::tempdir().unwrap();
    let opened = SecureDir::open(root.path()).unwrap();
    opened.try_lock_exclusive().unwrap();
    // Equivalent to a daemon retaining its root inode after both children
    // have been replaced. A cloned descriptor would falsely acquire this lock.
    drop(opened.open_lock_file(LEGACY_LOCK_NAME).unwrap());
    assert!(matches!(
        LegacyFormatOwnership::acquire(root.path()),
        Err(OwnershipError::Busy("controller (data-root inode)"))
    ));
    assert!(!root.path().join(RETIRED_LOCK_NAME).exists());
}

#[test]
fn concurrent_converters_elect_one_owner_without_a_missing_legacy_name() {
    let root = tempfile::tempdir().unwrap();
    let start = Arc::new(Barrier::new(8));
    let hold = Arc::new(Barrier::new(8));
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let path = root.path().to_owned();
            let start = start.clone();
            let hold = hold.clone();
            std::thread::spawn(move || {
                start.wait();
                let result =
                    LegacyFormatOwnership::acquire(&path).and_then(LegacyFormatOwnership::upgrade);
                hold.wait();
                result.is_ok()
            })
        })
        .collect();
    assert_eq!(
        handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|won| *won)
            .count(),
        1
    );
    assert!(root.path().join(LEGACY_LOCK_NAME).is_dir());
    UpgradedFormatOwnership::open(root.path()).unwrap();
}

#[test]
fn complete_preparation_survives_controller_death_before_exchange() {
    let root = tempfile::tempdir().unwrap();
    let prepared = LegacyFormatOwnership::acquire(root.path())
        .unwrap()
        .prepare()
        .unwrap();
    let identity = prepared.manifest().clone();
    assert!(root.path().join(LEGACY_LOCK_NAME).is_file());
    assert!(root.path().join(RETIRED_LOCK_NAME).is_dir());
    drop(prepared);
    assert!(UpgradedFormatOwnership::open(root.path()).is_err());
    let recovered = LegacyFormatOwnership::acquire(root.path())
        .unwrap()
        .upgrade()
        .unwrap();
    assert_eq!(recovered.manifest(), &identity);
}

#[test]
fn complete_exchange_survives_controller_death_before_durability_acknowledgment() {
    let root = tempfile::tempdir().unwrap();
    let prepared = LegacyFormatOwnership::acquire(root.path())
        .unwrap()
        .prepare()
        .unwrap();
    let identity = prepared.manifest().clone();
    // The exact production exchange primitive, intentionally without the final
    // parent fsync/guard issuance. This is a process-crash cut, not a power-loss
    // simulation; reopening must perform the omitted durability barriers.
    SecureDir::open(root.path())
        .unwrap()
        .exchange_file_and_directory(LEGACY_LOCK_NAME, RETIRED_LOCK_NAME)
        .unwrap();
    drop(prepared);
    let recovered = UpgradedFormatOwnership::open(root.path()).unwrap();
    assert_eq!(recovered.manifest(), &identity);
}

#[test]
fn actual_process_exit_releases_leases_at_both_conversion_crash_cuts() {
    for phase in ["prepared", "exchanged"] {
        let root = tempfile::tempdir().unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "ownership_crash_child", "--ignored"])
            .env("AXO_OWNERSHIP_CRASH_ROOT", root.path())
            .env("AXO_OWNERSHIP_CRASH_PHASE", phase)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(42));
        let guard = if phase == "prepared" {
            LegacyFormatOwnership::acquire(root.path())
                .unwrap()
                .upgrade()
                .unwrap()
        } else {
            UpgradedFormatOwnership::open(root.path()).unwrap()
        };
        assert_eq!(guard.manifest().schema_version, 2);
    }
}

#[test]
#[ignore = "subprocess helper; run only through the crash-cut test"]
fn ownership_crash_child() {
    let path = PathBuf::from(std::env::var_os("AXO_OWNERSHIP_CRASH_ROOT").unwrap());
    let phase = std::env::var("AXO_OWNERSHIP_CRASH_PHASE").unwrap();
    assert!(inventory(&path).is_empty());
    let _prepared = LegacyFormatOwnership::acquire(&path)
        .unwrap()
        .prepare()
        .unwrap();
    if phase == "exchanged" {
        SecureDir::open(&path)
            .unwrap()
            .exchange_file_and_directory(LEGACY_LOCK_NAME, RETIRED_LOCK_NAME)
            .unwrap();
    } else {
        assert_eq!(phase, "prepared");
    }
    // Deliberately skip Rust destructors. No upgraded guard was issued, and no
    // final directory-sync acknowledgment occurs in the exchanged case.
    std::process::exit(42);
}

#[test]
fn incomplete_or_ambiguous_conversion_is_refused_without_repair_by_guessing() {
    for shape in [
        "empty_preparation",
        "missing_legacy",
        "two_files",
        "two_directories",
    ] {
        let root = tempfile::tempdir().unwrap();
        let data = SecureDir::open(root.path()).unwrap();
        match shape {
            "empty_preparation" => {
                drop(data.open_lock_file(LEGACY_LOCK_NAME).unwrap());
                data.create_child(RETIRED_LOCK_NAME).unwrap();
            }
            "missing_legacy" => {
                data.create_child(RETIRED_LOCK_NAME).unwrap();
            }
            "two_files" => {
                drop(data.open_lock_file(LEGACY_LOCK_NAME).unwrap());
                drop(data.open_lock_file(RETIRED_LOCK_NAME).unwrap());
            }
            _ => {
                data.create_child(LEGACY_LOCK_NAME).unwrap();
                data.create_child(RETIRED_LOCK_NAME).unwrap();
            }
        }
        let before = inventory(root.path());
        assert!(
            LegacyFormatOwnership::acquire(root.path())
                .and_then(LegacyFormatOwnership::upgrade)
                .is_err(),
            "{shape}"
        );
        assert!(
            UpgradedFormatOwnership::open(root.path()).is_err(),
            "{shape}"
        );
        assert_eq!(inventory(root.path()), before, "{shape}");
    }
}

#[test]
fn invalid_incomplete_or_future_installed_manifests_never_issue_authority() {
    for corruption in [
        "missing",
        "partial",
        "unknown_field",
        "future_schema",
        "future_migration",
        "legacy_protocol",
        "different_root",
        "different_inode",
        "missing_id",
        "oversized",
        "extra_file",
    ] {
        let root = tempfile::tempdir().unwrap();
        let upgraded = LegacyFormatOwnership::acquire(root.path())
            .unwrap()
            .upgrade()
            .unwrap();
        let path = root.path().join(LEGACY_LOCK_NAME).join("format.json");
        let mut data = serde_json::to_value(upgraded.manifest()).unwrap();
        drop(upgraded);
        match corruption {
            "missing" => {
                fs::remove_file(&path).unwrap();
            }
            "partial" => {
                fs::write(&path, b"{\"schema_version\":2").unwrap();
            }
            "oversized" => {
                fs::write(&path, vec![b' '; 4097]).unwrap();
            }
            "extra_file" => {
                fs::write(path.parent().unwrap().join("unexpected"), b"x").unwrap();
            }
            other => {
                match other {
                    "unknown_field" => data["future"] = true.into(),
                    "future_schema" => data["schema_version"] = 3.into(),
                    "future_migration" => data["migration_version"] = 2.into(),
                    "legacy_protocol" => data["legacy_protocol"] = "v0.1".into(),
                    "different_root" => data["root_authority_sha256"] = "0".repeat(64).into(),
                    "different_inode" => data["legacy_lock_inode"] = "1:2".into(),
                    "missing_id" => {
                        data.as_object_mut().unwrap().remove("ownership_id");
                    }
                    _ => unreachable!(),
                }
                fs::write(&path, serde_json::to_vec(&data).unwrap()).unwrap();
            }
        }
        let before = inventory(root.path());
        assert!(
            UpgradedFormatOwnership::open(root.path()).is_err(),
            "{corruption}"
        );
        assert_eq!(inventory(root.path()), before, "{corruption}");
    }
}

#[test]
fn filesystem_conflict_before_exchange_does_not_replace_the_legacy_boundary() {
    let root = tempfile::tempdir().unwrap();
    let prepared = LegacyFormatOwnership::acquire(root.path())
        .unwrap()
        .prepare()
        .unwrap();
    fs::remove_file(root.path().join(RETIRED_LOCK_NAME).join("format.json")).unwrap();
    assert!(prepared.install().is_err());
    assert!(root.path().join(LEGACY_LOCK_NAME).is_file());
    assert!(!root.path().join("execution").exists());
}

#[test]
fn missing_or_replaced_retained_lock_refuses_installed_root() {
    for replace in [false, true] {
        let root = tempfile::tempdir().unwrap();
        drop(
            LegacyFormatOwnership::acquire(root.path())
                .unwrap()
                .upgrade()
                .unwrap(),
        );
        let retired = root.path().join(RETIRED_LOCK_NAME);
        // Retain old inode so allocator reuse cannot make the test ambiguous.
        let old = File::open(&retired).unwrap();
        fs::remove_file(&retired).unwrap();
        if replace {
            fs::write(&retired, b"").unwrap();
        }
        let before = inventory(root.path());
        assert!(UpgradedFormatOwnership::open(root.path()).is_err());
        assert_eq!(inventory(root.path()), before);
        drop(old);
    }
}

#[test]
fn linked_paths_or_replaced_root_cannot_redirect_conversion() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("root");
    fs::create_dir(&root).unwrap();
    let prepared = LegacyFormatOwnership::acquire(&root)
        .unwrap()
        .prepare()
        .unwrap();
    fs::rename(&root, parent.path().join("original")).unwrap();
    fs::create_dir(&root).unwrap();
    assert!(prepared.install().is_err());
    assert!(inventory(&root).is_empty());
    let linked = parent.path().join("linked");
    symlink(&root, &linked).unwrap();
    assert!(LegacyFormatOwnership::acquire(&linked).is_err());
    symlink(parent.path().join("outside"), root.join(LEGACY_LOCK_NAME)).unwrap();
    assert!(LegacyFormatOwnership::acquire(&root).is_err());
    assert!(!parent.path().join("outside").exists());
}

#[test]
fn equivalent_root_spellings_contend_and_missing_roots_are_not_created() {
    let root = tempfile::tempdir().unwrap();
    let _owner = LegacyFormatOwnership::acquire(root.path()).unwrap();
    assert!(matches!(
        LegacyFormatOwnership::acquire(root.path().join(".")),
        Err(OwnershipError::Busy(_))
    ));
    let absent = root.path().join("absent");
    assert!(LegacyFormatOwnership::acquire(&absent).is_err());
    assert!(!absent.exists());
}

/// Called only by the private released-binary fixture. Uses production migration
/// to provision an empty disposable root, then seeds non-executable read evidence
/// while the format guard is held. No live daemon or provider is involved.
#[test]
#[ignore = "explicit private released-binary compatibility probe only"]
fn provision_disposable_ownership_fixture() {
    let path = PathBuf::from(std::env::var_os("AXO_OWNERSHIP_FIXTURE_ROOT").unwrap());
    assert!(path.starts_with("/private/tmp"));
    assert!(path.components().any(|part| part
        .as_os_str()
        .to_string_lossy()
        .starts_with("axocoatl-ownership-")));
    assert!(inventory(&path).is_empty());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    let guard = LegacyFormatOwnership::acquire(&path)
        .unwrap()
        .upgrade()
        .unwrap();
    fs::create_dir_all(path.join("execution/v2")).unwrap();
    fs::write(
        path.join("execution/v2/unfinished.json"),
        b"{\"fixture\":true,\"state\":\"needs_attention\"}\n",
    )
    .unwrap();
    fs::create_dir(path.join("session-history")).unwrap();
    fs::write(path.join("session-history/turns.v1.jsonl"), b"").unwrap();
    println!(
        "production ownership fixture {}",
        guard.manifest().ownership_id
    );
}
