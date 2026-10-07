//! A copy of a Session's storage, taken before its journals are first
//! converted to segment logs.
//!
//! Axocoatl 1.2 and earlier keep each Session journal in one file. This
//! release converts each such file to a segment log when the journal is first
//! opened, and a release that predates segment logs then refuses the Session.
//! The canonical journal is always the first journal of a Session to open, so
//! before it is converted the whole Session directory, `execution-v2/<key>`
//! under the data root, is copied to `backups/before-segments/<key>/session`,
//! and `backups/before-segments/<key>/backup.json` is written last to mark the
//! copy complete. `<key>` is the SHA-256 of the Session id, the same name the
//! Session has under `execution-v2`.
//!
//! A complete backup is never replaced, so it keeps the Session as it was
//! before any of its journals was converted, including journals converted
//! later. A copy cut short by a crash has no `backup.json`; it is removed and
//! taken again. If the copy cannot be made, nothing is converted and the
//! Session does not open.
//!
//! Restoring is a directory swap made while the daemon is stopped: move
//! `execution-v2/<key>` aside and copy `backups/before-segments/<key>/session`
//! in its place (see the Upgrade page of the documentation).

use std::io;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use axocoatl_core::{SecureDir, SecureEntryType};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::execution_ownership::UpgradedFormatOwnership;
use crate::execution_store::{ExecutionStoreError, ExecutionStoreOwner};

/// Directory under the data root that holds automatic backups.
pub const BACKUPS_DIR: &str = "backups";
/// Directory under [`BACKUPS_DIR`] for copies taken before the conversion to
/// segment logs.
pub const BEFORE_SEGMENTS_DIR: &str = "before-segments";
/// The copied Session directory inside one backup.
pub const SESSION_COPY_DIR: &str = "session";
/// Written last; its presence marks a complete backup.
pub const MANIFEST_FILE: &str = "backup.json";
const FORMAT: &str = "axocoatl-session-backup";
const REASON: &str = "before-segments";
const SCHEMA_VERSION: u32 = 1;
const MAX_MANIFEST_BYTES: usize = 16 * 1024;
/// Deeper than any Session store nests its directories.
const MAX_DEPTH: usize = 16;

/// `backup.json`: what was copied, from where and when.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SegmentBackupManifest {
    pub format: String,
    pub schema_version: u32,
    pub reason: String,
    pub session_id: String,
    pub workspace_id: String,
    /// The copied directory, relative to the data root.
    pub source: String,
    pub created_unix_seconds: u64,
    /// The Axocoatl version that made the copy.
    pub axocoatl_version: String,
    pub files: u64,
    pub bytes: u64,
}

/// The directory name of a Session under `execution-v2` and under
/// `backups/before-segments`.
pub fn session_key(session_id: &str) -> String {
    format!("{:x}", Sha256::digest(session_id.as_bytes()))
}

/// Copy `session`, the locked canonical directory of `owner`'s Session, to
/// its backup unless a complete backup already exists. Returns whether a copy
/// was made by this call.
pub(crate) fn back_up_before_conversion(
    ownership: &UpgradedFormatOwnership,
    owner: &ExecutionStoreOwner,
    session: &SecureDir,
) -> Result<bool, ExecutionStoreError> {
    let backups = ownership.before_segments_backup_directory()?;
    let key = session_key(owner.session_id.as_str());
    if backups.has_exact_directory(&key)? {
        let existing = backups.existing_child(&key)?;
        if existing.has_exact_file(MANIFEST_FILE)? {
            let manifest: SegmentBackupManifest =
                serde_json::from_slice(&existing.read_limited(MANIFEST_FILE, MAX_MANIFEST_BYTES)?)?;
            if manifest.format != FORMAT || manifest.session_id != owner.session_id.as_str() {
                return Err(ExecutionStoreError::Invalid(
                    "the Session's backup names another Session",
                ));
            }
            return Ok(false);
        }
        // A copy cut short by a crash: take it again from the start.
        drop(existing);
        backups.remove_dir_all(&key)?;
        backups.sync_all()?;
    }
    let target = backups.create_child(&key)?;
    backups.sync_all()?;
    let copy = target.create_child(SESSION_COPY_DIR)?;
    let mut totals = Totals::default();
    copy_tree(session, &copy, 0, &mut totals)?;
    target.sync_all()?;
    let manifest = SegmentBackupManifest {
        format: FORMAT.into(),
        schema_version: SCHEMA_VERSION,
        reason: REASON.into(),
        session_id: owner.session_id.as_str().into(),
        workspace_id: owner.workspace_id.clone(),
        source: format!("execution-v2/{key}"),
        created_unix_seconds: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs()),
        axocoatl_version: env!("CARGO_PKG_VERSION").into(),
        files: totals.files,
        bytes: totals.bytes,
    };
    let mut bytes = serde_json::to_vec_pretty(&manifest)?;
    bytes.push(b'\n');
    target.atomic_write(MANIFEST_FILE, &bytes)?;
    backups.sync_all()?;
    Ok(true)
}

#[derive(Default)]
struct Totals {
    files: u64,
    bytes: u64,
}

/// Copy every file and directory below `source` into the empty `target`,
/// syncing each file and directory. Anything else (a symlink, a socket) is
/// refused rather than skipped, so a backup is never silently partial.
fn copy_tree(
    source: &SecureDir,
    target: &SecureDir,
    depth: usize,
    totals: &mut Totals,
) -> io::Result<()> {
    if depth > MAX_DEPTH {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{} nests deeper than a Session backup copies",
                source.path().display()
            ),
        ));
    }
    let mut entries = source.entries()?;
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    for entry in entries {
        let name = Path::new(&entry.name);
        match entry.file_type {
            SecureEntryType::Directory => {
                let from = source.existing_child(name)?;
                let to = target.create_child(&entry.name)?;
                copy_tree(&from, &to, depth + 1, totals)?;
            }
            SecureEntryType::File => {
                let mut from = source.open_file_limited(name, usize::MAX)?;
                let mut to = target.create_new(name)?;
                totals.bytes += io::copy(&mut from, &mut to)?;
                to.sync_all()?;
                totals.files += 1;
            }
            SecureEntryType::Other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "{} is neither a file nor a directory, so the Session cannot be backed up",
                        source.path().join(name).display()
                    ),
                ));
            }
        }
    }
    target.sync_all()
}
