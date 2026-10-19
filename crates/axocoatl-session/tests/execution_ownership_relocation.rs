#![cfg(unix)]
//! A data root copied (`cp -a` of a cold backup restored in place, a
//! migrated Mac) or moved since its format manifest was written. 1.4.0
//! refused every such root at daemon admission, because the manifest records
//! the digest of the root's path, the root's inode and the retained lock
//! file's inode, and a copy has new inodes and a move a new path. Admission
//! now re-records those three identities, keeping the ownership id, while
//! it holds every lease; every other difference is still refused unchanged.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use axocoatl_core::SecureDir;
use axocoatl_session::execution_ownership::{
    inspect_format_ownership, DataRootFormatOwnership, FormatManifest, FormatOwnershipReport,
    LegacyFormatOwnership, OwnershipError, RecordedRelocation, UpgradedFormatOwnership,
    LEGACY_LOCK_NAME, REBIND_MANIFEST_NAME, RETIRED_LOCK_NAME,
};
use sha2::{Digest, Sha256};

const COPY: RecordedRelocation = RecordedRelocation {
    path_changed: false,
    directory_changed: true,
    lock_file_changed: true,
};
const MOVE: RecordedRelocation = RecordedRelocation {
    path_changed: true,
    directory_changed: false,
    lock_file_changed: false,
};
const COPY_ELSEWHERE: RecordedRelocation = RecordedRelocation {
    path_changed: true,
    directory_changed: true,
    lock_file_changed: true,
};
const COPIED_INTO: RecordedRelocation = RecordedRelocation {
    path_changed: false,
    directory_changed: false,
    lock_file_changed: true,
};

/// `cp -a FROM TO`, exactly as the backup page tells people to copy a root.
fn copy_tree(from: &Path, to: &Path) {
    let status = std::process::Command::new("cp")
        .arg("-a")
        .arg(from)
        .arg(to)
        .status()
        .unwrap();
    assert!(
        status.success(),
        "cp -a {} {}",
        from.display(),
        to.display()
    );
}

/// A private directory `parent/name` holding an installed boundary whose
/// owner has exited, with a file standing in for Session state.
fn installed_root(parent: &Path, name: &str) -> (PathBuf, FormatManifest) {
    let root = parent.join(name);
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    let opened = SecureDir::open_existing_all(&root).unwrap();
    let manifest = DataRootFormatOwnership::acquire_for_root(&opened)
        .unwrap()
        .into_upgraded()
        .unwrap()
        .manifest()
        .clone();
    fs::create_dir(root.join("sessions")).unwrap();
    fs::write(root.join("sessions/ses-1.json"), b"{\"id\":\"ses-1\"}").unwrap();
    (root, manifest)
}

fn admit(
    root: &Path,
) -> Result<(DataRootFormatOwnership, Option<RecordedRelocation>), OwnershipError> {
    DataRootFormatOwnership::acquire_for_root_reporting(&SecureDir::open_existing_all(root)?)
}

fn written_manifest(root: &Path) -> FormatManifest {
    serde_json::from_slice(&fs::read(root.join(LEGACY_LOCK_NAME).join("format.json")).unwrap())
        .unwrap()
}

fn names(directory: &Path) -> Vec<String> {
    let mut names: Vec<_> = fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn identity(path: &Path) -> String {
    let metadata = fs::symlink_metadata(path).unwrap();
    format!("{}:{}", metadata.dev(), metadata.ino())
}

/// The external lease name of `root`, which is also its recorded path digest.
fn authority(root: &Path) -> String {
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
    format!("{:x}", Sha256::digest(bytes))
}

fn inventory(root: &Path) -> BTreeMap<PathBuf, (u32, u64, Vec<u8>)> {
    fn walk(root: &Path, dir: &Path, output: &mut BTreeMap<PathBuf, (u32, u64, Vec<u8>)>) {
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
                (metadata.mode(), metadata.ino(), bytes),
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

/// The re-recorded root is exact afterwards: one manifest in the lock
/// directory, no reserved file, the same ownership id, and identities that
/// name this directory, this path and this retained lock file.
fn assert_recorded_here(root: &Path, original: &FormatManifest) -> FormatManifest {
    let manifest = written_manifest(root);
    assert_eq!(manifest.ownership_id, original.ownership_id);
    assert_eq!(manifest.root_authority_sha256, authority(root));
    assert_eq!(manifest.root_inode, identity(root));
    assert_eq!(
        manifest.legacy_lock_inode,
        identity(&root.join(RETIRED_LOCK_NAME))
    );
    assert_eq!(names(&root.join(LEGACY_LOCK_NAME)), ["format.json"]);
    assert!(!root.join(REBIND_MANIFEST_NAME).exists());
    assert_eq!(
        fs::read(root.join("sessions/ses-1.json")).unwrap(),
        b"{\"id\":\"ses-1\"}"
    );
    // A 1.0.0 controller still cannot use the root: its lock path is a
    // directory, never a file it could lock.
    let data = SecureDir::open(root).unwrap();
    assert!(data.is_file(LEGACY_LOCK_NAME).is_err());
    assert!(data.open_lock_file(LEGACY_LOCK_NAME).is_err());
    // Exact from now on: nothing more is re-recorded, and the strict
    // path-based reopen accepts it.
    let (owner, relocation) = admit(root).unwrap();
    assert_eq!(relocation, None);
    assert_eq!(owner.into_upgraded().unwrap().manifest(), &manifest);
    assert_eq!(
        UpgradedFormatOwnership::open(root).unwrap().manifest(),
        &manifest
    );
    manifest
}

#[test]
fn a_cold_backup_restored_in_place_is_re_recorded_and_keeps_its_ownership_id() {
    let parent = tempfile::tempdir().unwrap();
    let (root, original) = installed_root(parent.path(), "data");
    let backup = parent.path().join("backup");
    copy_tree(&root, &backup);
    fs::remove_dir_all(&root).unwrap();
    copy_tree(&backup, &root);
    assert_eq!(written_manifest(&root), original);
    // What 1.4.0 refused at every start: the strict reopen still refuses it
    // and changes nothing.
    let before = inventory(&root);
    assert!(UpgradedFormatOwnership::open(&root).is_err());
    assert_eq!(inventory(&root), before);
    assert_eq!(
        inspect_format_ownership(&root),
        FormatOwnershipReport::Installed {
            relocation: Some(COPY),
            interrupted_rebind: false
        }
    );

    let (owner, relocation) = admit(&root).unwrap();
    assert_eq!(relocation, Some(COPY));
    let held = owner.into_upgraded().unwrap();
    assert_eq!(held.manifest().ownership_id, original.ownership_id);
    assert_eq!(
        held.manifest().root_authority_sha256,
        original.root_authority_sha256
    );
    assert_ne!(held.manifest().root_inode, original.root_inode);
    assert_eq!(held.manifest(), &written_manifest(&root));
    drop(held);
    assert_recorded_here(&root, &original);
    assert_eq!(
        inspect_format_ownership(&root),
        FormatOwnershipReport::Installed {
            relocation: None,
            interrupted_rebind: false
        }
    );
    // The backup itself is untouched.
    assert_eq!(
        fs::read(backup.join(LEGACY_LOCK_NAME).join("format.json")).unwrap(),
        serde_json::to_vec(&original).unwrap()
    );
}

#[test]
fn a_moved_root_records_only_its_new_path() {
    let parent = tempfile::tempdir().unwrap();
    let (root, original) = installed_root(parent.path(), "data");
    let moved = parent.path().join("moved");
    fs::rename(&root, &moved).unwrap();
    let (owner, relocation) = admit(&moved).unwrap();
    assert_eq!(relocation, Some(MOVE));
    drop(owner);
    let manifest = assert_recorded_here(&moved, &original);
    assert_ne!(
        manifest.root_authority_sha256,
        original.root_authority_sha256
    );
    assert_eq!(manifest.root_inode, original.root_inode);
    assert_eq!(manifest.legacy_lock_inode, original.legacy_lock_inode);
    // Moving it back is the same: its path is recorded again.
    fs::rename(&moved, &root).unwrap();
    assert_eq!(admit(&root).unwrap().1, Some(MOVE));
    assert_eq!(written_manifest(&root), original);
}

#[test]
fn a_copy_at_another_path_is_re_recorded_and_the_original_is_untouched() {
    let parent = tempfile::tempdir().unwrap();
    let (root, original) = installed_root(parent.path(), "data");
    let copy = parent.path().join("migrated");
    copy_tree(&root, &copy);
    let (owner, relocation) = admit(&copy).unwrap();
    assert_eq!(relocation, Some(COPY_ELSEWHERE));
    // Two directories, two owners: the original is still exactly its own.
    let (original_owner, original_relocation) = admit(&root).unwrap();
    assert_eq!(original_relocation, None);
    assert_eq!(
        original_owner.into_upgraded().unwrap().manifest(),
        &original
    );
    drop(owner);
    assert_recorded_here(&copy, &original);
    assert_eq!(written_manifest(&root), original);
}

#[test]
fn an_interrupted_re_recording_is_removed_at_the_next_start() {
    // The reserved file written, the crash before its rename: the lock
    // directory still holds the old manifest, alone.
    let parent = tempfile::tempdir().unwrap();
    let (root, original) = installed_root(parent.path(), "data");
    let copy = parent.path().join("copy");
    copy_tree(&root, &copy);
    fs::write(copy.join(REBIND_MANIFEST_NAME), b"{\"half\":").unwrap();
    assert_eq!(
        inspect_format_ownership(&copy),
        FormatOwnershipReport::Installed {
            relocation: Some(COPY_ELSEWHERE),
            interrupted_rebind: true
        }
    );
    assert_eq!(admit(&copy).unwrap().1, Some(COPY_ELSEWHERE));
    assert_recorded_here(&copy, &original);

    // A crash after the rename leaves nothing to remove; one before it on a
    // root that is exact again (moved back) leaves only the reserved file.
    fs::write(root.join(REBIND_MANIFEST_NAME), b"anything").unwrap();
    assert_eq!(admit(&root).unwrap().1, None);
    assert!(!root.join(REBIND_MANIFEST_NAME).exists());
    assert_eq!(written_manifest(&root), original);

    // Something else at the reserved name is refused, never followed or
    // removed, and the manifest is left as it was.
    for shape in ["directory", "link", "hard_link"] {
        let copy = parent.path().join(format!("copy-{shape}"));
        copy_tree(&root, &copy);
        let outside = parent.path().join(format!("outside-{shape}"));
        fs::write(&outside, b"keep").unwrap();
        match shape {
            "directory" => fs::create_dir(copy.join(REBIND_MANIFEST_NAME)).unwrap(),
            "link" => symlink(&outside, copy.join(REBIND_MANIFEST_NAME)).unwrap(),
            _ => fs::hard_link(&outside, copy.join(REBIND_MANIFEST_NAME)).unwrap(),
        }
        if shape == "link" {
            // A link is removed without following it.
            assert_eq!(admit(&copy).unwrap().1, Some(COPY_ELSEWHERE));
            assert!(fs::symlink_metadata(copy.join(REBIND_MANIFEST_NAME)).is_err());
            assert_eq!(fs::read(&outside).unwrap(), b"keep");
            continue;
        }
        let before = inventory(&copy);
        assert!(admit(&copy).is_err(), "{shape}");
        assert_eq!(inventory(&copy), before, "{shape}");
        assert_eq!(written_manifest(&copy), original, "{shape}");
        assert_eq!(fs::read(&outside).unwrap(), b"keep");
    }
}

#[test]
fn every_other_difference_in_a_copied_root_is_still_refused_unchanged() {
    let parent = tempfile::tempdir().unwrap();
    let (root, original) = installed_root(parent.path(), "data");
    for corruption in [
        "missing",
        "partial",
        "oversized",
        "extra_file",
        "unknown_field",
        "future_schema",
        "future_migration",
        "legacy_protocol",
        "other_format",
        "missing_id",
        "nil_id",
        "uppercase_id",
        "malformed_authority",
        "malformed_root_inode",
        "malformed_lock_inode",
        "lock_directory_link",
        "retained_lock_directory",
        "retained_lock_link",
        "retained_lock_missing",
    ] {
        let copy = parent.path().join(corruption);
        copy_tree(&root, &copy);
        let manifest = copy.join(LEGACY_LOCK_NAME).join("format.json");
        let mut data = serde_json::to_value(&original).unwrap();
        let mut rewrite = true;
        match corruption {
            "missing" => {
                fs::remove_file(&manifest).unwrap();
                rewrite = false;
            }
            "partial" => {
                fs::write(&manifest, b"{\"schema_version\":2").unwrap();
                rewrite = false;
            }
            "oversized" => {
                fs::write(&manifest, vec![b' '; 4097]).unwrap();
                rewrite = false;
            }
            "extra_file" => {
                fs::write(copy.join(LEGACY_LOCK_NAME).join("unexpected"), b"x").unwrap();
                rewrite = false;
            }
            "unknown_field" => data["future"] = true.into(),
            "future_schema" => data["schema_version"] = 3.into(),
            "future_migration" => data["migration_version"] = 2.into(),
            "legacy_protocol" => data["legacy_protocol"] = "v0.1".into(),
            "other_format" => data["format"] = "something-else".into(),
            "missing_id" => {
                data.as_object_mut().unwrap().remove("ownership_id");
            }
            "nil_id" => data["ownership_id"] = uuid::Uuid::nil().to_string().into(),
            "uppercase_id" => {
                data["ownership_id"] = original.ownership_id.to_uppercase().into();
            }
            "malformed_authority" => data["root_authority_sha256"] = "not-a-digest".into(),
            "malformed_root_inode" => data["root_inode"] = "garbage".into(),
            "malformed_lock_inode" => data["legacy_lock_inode"] = "1:".into(),
            "lock_directory_link" => {
                fs::rename(copy.join(LEGACY_LOCK_NAME), copy.join("elsewhere")).unwrap();
                symlink("elsewhere", copy.join(LEGACY_LOCK_NAME)).unwrap();
                rewrite = false;
            }
            "retained_lock_directory" => {
                fs::remove_file(copy.join(RETIRED_LOCK_NAME)).unwrap();
                fs::create_dir(copy.join(RETIRED_LOCK_NAME)).unwrap();
                rewrite = false;
            }
            "retained_lock_link" => {
                fs::remove_file(copy.join(RETIRED_LOCK_NAME)).unwrap();
                symlink("sessions/ses-1.json", copy.join(RETIRED_LOCK_NAME)).unwrap();
                rewrite = false;
            }
            "retained_lock_missing" => {
                fs::remove_file(copy.join(RETIRED_LOCK_NAME)).unwrap();
                rewrite = false;
            }
            _ => unreachable!(),
        }
        if rewrite {
            fs::write(&manifest, serde_json::to_vec(&data).unwrap()).unwrap();
        }
        let before = inventory(&copy);
        assert!(admit(&copy).is_err(), "{corruption}");
        assert_eq!(inventory(&copy), before, "{corruption}");
        assert!(
            matches!(
                inspect_format_ownership(&copy),
                FormatOwnershipReport::Refused(_)
            ),
            "{corruption}"
        );
        assert_eq!(inventory(&copy), before, "{corruption}");
    }
}

/// Empty `root` and copy the backup's files back into the same directory,
/// as `rm -rf data/* data/.*; cp -a backup/. data/` does: the directory is
/// the same, every file in it is new.
fn copy_back_into(backup: &Path, root: &Path) {
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.symlink_metadata().unwrap().is_dir() {
            fs::remove_dir_all(&path).unwrap();
        } else {
            fs::remove_file(&path).unwrap();
        }
    }
    assert!(names(root).is_empty());
    copy_tree(&backup.join("."), root);
}

#[test]
fn a_backup_copied_back_into_the_same_directory_is_re_recorded() {
    for moved in [false, true] {
        let parent = tempfile::tempdir().unwrap();
        let (mut root, original) = installed_root(parent.path(), "data");
        let backup = parent.path().join("backup");
        copy_tree(&root, &backup);
        let directory = identity(&root);
        // Keep the old lock inode so the allocator cannot hand it back.
        let old = File::open(root.join(RETIRED_LOCK_NAME)).unwrap();
        copy_back_into(&backup, &root);
        assert_eq!(identity(&root), directory);
        let mut expected = COPIED_INTO;
        if moved {
            let to = parent.path().join("moved");
            fs::rename(&root, &to).unwrap();
            root = to;
            expected.path_changed = true;
        }
        assert_eq!(admit(&root).unwrap().1, Some(expected), "moved: {moved}");
        let manifest = assert_recorded_here(&root, &original);
        assert_eq!(manifest.root_inode, original.root_inode);
        drop(old);
    }
}

#[test]
fn a_replaced_lock_file_never_admits_a_second_owner_of_a_held_directory() {
    let parent = tempfile::tempdir().unwrap();
    let (root, original) = installed_root(parent.path(), "data");
    let (owner, relocation) = admit(&root).unwrap();
    assert_eq!(relocation, None);
    // Replace the retained lock file the owner holds, then reach the same
    // directory through its old path and a new one.
    let retired = root.join(RETIRED_LOCK_NAME);
    let old = File::open(&retired).unwrap();
    fs::remove_file(&retired).unwrap();
    fs::write(&retired, b"").unwrap();
    assert!(matches!(
        admit(&root),
        Err(OwnershipError::Busy("controller (external lease)"))
    ));
    let moved = parent.path().join("moved");
    fs::rename(&root, &moved).unwrap();
    // A new path has its own external lease, and the new lock file is free,
    // but the directory's own lock is held: refused before anything is read
    // or re-recorded.
    assert!(matches!(
        admit(&moved),
        Err(OwnershipError::Busy("controller (data-root inode)"))
    ));
    assert_eq!(written_manifest(&moved), original);
    assert!(!moved.join(REBIND_MANIFEST_NAME).exists());
    drop(owner);
    assert_eq!(
        admit(&moved).unwrap().1,
        Some(RecordedRelocation {
            path_changed: true,
            directory_changed: false,
            lock_file_changed: true,
        })
    );
    assert_recorded_here(&moved, &original);
    drop(old);
}

#[test]
fn a_second_owner_reaching_the_same_directory_through_another_path_is_excluded() {
    let parent = tempfile::tempdir().unwrap();
    let (root, original) = installed_root(parent.path(), "data");
    let (owner, relocation) = admit(&root).unwrap();
    assert_eq!(relocation, None);
    // The directory moves while it is owned. The new path has its own
    // external lease, free, but the in-root lock file is held: no second
    // owner, and nothing is re-recorded for it.
    let moved = parent.path().join("moved");
    fs::rename(&root, &moved).unwrap();
    assert!(matches!(
        admit(&moved),
        Err(OwnershipError::Busy("legacy controller (in-root lease)"))
    ));
    assert_eq!(written_manifest(&moved), original);
    assert!(!moved.join(REBIND_MANIFEST_NAME).exists());
    // Doctor reads it without a lease, so it reports what the next start
    // will do, and changes nothing.
    assert_eq!(
        inspect_format_ownership(&moved),
        FormatOwnershipReport::Installed {
            relocation: Some(MOVE),
            interrupted_rebind: false
        }
    );
    drop(owner);
    // Once the owner is gone, the next owner records the new path, and then
    // excludes an owner through the old one.
    let (second, relocation) = admit(&moved).unwrap();
    assert_eq!(relocation, Some(MOVE));
    fs::rename(&moved, &root).unwrap();
    assert!(matches!(
        admit(&root),
        Err(OwnershipError::Busy("legacy controller (in-root lease)"))
    ));
    drop(second);
}

#[test]
fn a_copied_interrupted_conversion_is_re_recorded_and_then_finished() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("data");
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    let prepared = LegacyFormatOwnership::acquire(&root)
        .unwrap()
        .prepare()
        .unwrap();
    let original = prepared.manifest().clone();
    drop(prepared);
    let copy = parent.path().join("copy");
    copy_tree(&root, &copy);
    assert_eq!(
        inspect_format_ownership(&copy),
        FormatOwnershipReport::Prepared {
            relocation: Some(COPY_ELSEWHERE),
            interrupted_rebind: false
        }
    );
    let (owner, relocation) = admit(&copy).unwrap();
    assert_eq!(relocation, Some(COPY_ELSEWHERE));
    assert!(matches!(&owner, DataRootFormatOwnership::Legacy(_)));
    assert_eq!(names(&copy.join(RETIRED_LOCK_NAME)), ["format.json"]);
    let installed = owner.into_upgraded().unwrap();
    assert_eq!(installed.manifest().ownership_id, original.ownership_id);
    assert_eq!(installed.manifest().root_inode, identity(&copy));
    assert!(copy.join(LEGACY_LOCK_NAME).is_dir());
    assert!(copy.join(RETIRED_LOCK_NAME).is_file());
}

#[test]
fn doctor_reads_every_shape_without_changing_it() {
    let parent = tempfile::tempdir().unwrap();
    let empty = parent.path().join("empty");
    fs::create_dir(&empty).unwrap();
    assert_eq!(
        inspect_format_ownership(&empty),
        FormatOwnershipReport::Unrecorded
    );
    let legacy = parent.path().join("legacy");
    fs::create_dir(&legacy).unwrap();
    fs::write(legacy.join(LEGACY_LOCK_NAME), b"").unwrap();
    assert_eq!(
        inspect_format_ownership(&legacy),
        FormatOwnershipReport::Legacy
    );
    assert!(matches!(
        inspect_format_ownership(parent.path().join("absent")),
        FormatOwnershipReport::Refused(_)
    ));
    let (root, _) = installed_root(parent.path(), "data");
    let before = inventory(&root);
    // Read while an owner holds it, as while the daemon runs.
    let owner = admit(&root).unwrap();
    assert_eq!(
        inspect_format_ownership(&root),
        FormatOwnershipReport::Installed {
            relocation: None,
            interrupted_rebind: false
        }
    );
    drop(owner);
    assert_eq!(inventory(&root), before);
    let ambiguous = parent.path().join("ambiguous");
    fs::create_dir(&ambiguous).unwrap();
    fs::create_dir(ambiguous.join(LEGACY_LOCK_NAME)).unwrap();
    fs::create_dir(ambiguous.join(RETIRED_LOCK_NAME)).unwrap();
    assert!(matches!(
        inspect_format_ownership(&ambiguous),
        FormatOwnershipReport::Refused(_)
    ));
}
