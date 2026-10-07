//! Isolated, durable conversion of the controller's legacy format boundary.
//!
//! These guards prove exclusive **format ownership**, not execution readiness.
//! They exclude a live controller holding the existing external/in-root leases;
//! they do not settle containers, remote jobs, or effects orphaned by its death.
//! The execution store retains this guard while writing v2 records.
//! Startup may acquire these guards, but an installed format remains explicitly
//! refused by the daemon until its v2 execution integration is ready.
//!
//! Compatibility is limited to the external-lock protocol shipped in 1.0.0.
//! A pre-external-lock process could retain an old lock descriptor across the
//! conversion; this module makes no downgrade/exclusion claim for that protocol.
//! The original regular lock inode remains at a reserved name after an atomic
//! exchange, while the mandatory legacy pathname becomes a versioned directory.
//! No unlink/mkdir fallback is permitted. A guard is returned only after the
//! validated boundary and its parent have been synced successfully.
//! The existing data root and its ancestors must already be durably provisioned;
//! syncing entries inside that root does not provision its own parent entry.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axocoatl_core::{SecureDir, SecureLeaf};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The mandatory lock pathname checked before reconciliation by legacy 1.0.0.
pub const LEGACY_LOCK_NAME: &str = ".axocoatl-daemon.lock";
/// Prepared directory before exchange; retained original lock file afterwards.
pub const RETIRED_LOCK_NAME: &str = ".axocoatl-daemon.lock.v1";
const MANIFEST_NAME: &str = "format.json";
const FORMAT: &str = "axocoatl-execution-ownership";
const SCHEMA_VERSION: u32 = 2;
const MIGRATION_VERSION: u32 = 1;
const LEGACY_PROTOCOL: &str = "external-and-in-root-flock-v1.0.0";
const MAX_MANIFEST_BYTES: usize = 4096;

#[derive(Debug, thiserror::Error)]
pub enum OwnershipError {
    #[error("format ownership I/O: {0}")]
    Io(#[from] io::Error),
    #[error("format ownership manifest: {0}")]
    Json(#[from] serde_json::Error),
    #[error("format ownership is held by another {0}")]
    Busy(&'static str),
    #[error("invalid or ambiguous format ownership: {0}")]
    Invalid(String),
}

/// A complete, exact-format manifest. Deserialization alone is not authority:
/// only a held [`UpgradedFormatOwnership`] validates it against actual inodes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FormatManifest {
    pub format: String,
    pub schema_version: u32,
    pub migration_version: u32,
    pub legacy_protocol: String,
    pub ownership_id: String,
    pub root_authority_sha256: String,
    pub root_inode: String,
    pub legacy_lock_inode: String,
}

#[derive(Debug)]
struct RootLease {
    root: SecureDir,
    external_root: SecureDir,
    _external_file: File,
    legacy_file: File,
}

/// Exclusive legacy-format authority acquired with independent descriptors in
/// exactly the daemon's external-file, legacy-file, root-inode lock order.
/// There is no public constructor from a boolean, borrowed fd, or caller claim.
#[derive(Debug)]
pub struct LegacyFormatOwnership {
    lease: RootLease,
}

/// Complete durable preparation, while all original ownership locks remain held.
/// Dropping it grants no v2 authority. A later holder can explicitly resume it.
#[derive(Debug)]
pub struct PreparedFormatOwnership {
    lease: RootLease,
    manifest: FormatManifest,
}

/// Validated and durably installed schema-2 refusal boundary, held exclusively.
/// This is not Clone and does not expose a writable directory/path capability.
/// A future executor must additionally settle orphaned work before running.
#[derive(Debug)]
pub struct UpgradedFormatOwnership {
    _lease: RootLease,
    manifest: FormatManifest,
}

/// One format-aware controller owner acquired before any runtime reconciliation.
/// Conversion consumes the held legacy lease; it never unlocks and reacquires.
/// This enum proves exclusion only, not readiness to execute an installed format.
#[derive(Debug)]
pub enum DataRootFormatOwnership {
    Legacy(LegacyFormatOwnership),
    Upgraded(Arc<UpgradedFormatOwnership>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RootFormat {
    Legacy,
    Upgraded,
}

impl DataRootFormatOwnership {
    /// Admit the exact already-open bootstrap root using independently opened
    /// lock descriptors. Format detection occurs under the external lease, so
    /// an unsupported/partial shape never reaches cleanup or ordinary stores.
    pub fn acquire_for_root(data_root: &SecureDir) -> Result<Self, OwnershipError> {
        let (lease, format) = RootLease::acquire(data_root.path(), None, Some(data_root))?;
        match format {
            RootFormat::Legacy => {
                if matches!(
                    leaf(&lease.root, RETIRED_LOCK_NAME)?,
                    Some(SecureLeaf::Directory)
                ) {
                    read_manifest(&lease.root, RETIRED_LOCK_NAME)?.validate(&lease)?;
                }
                Ok(Self::Legacy(LegacyFormatOwnership { lease }))
            }
            RootFormat::Upgraded => {
                let manifest = read_manifest(&lease.root, LEGACY_LOCK_NAME)?;
                Ok(Self::Upgraded(Arc::new(finish_installed(lease, manifest)?)))
            }
        }
    }

    /// Retained external control-plane root used by cleanup and sandbox
    /// protection. Returning it does not release any of the owner's locks.
    pub fn external_root(&self) -> &SecureDir {
        match self {
            Self::Legacy(owner) => &owner.lease.external_root,
            Self::Upgraded(owner) => &owner._lease.external_root,
        }
    }

    pub fn verify_root(&self, root: &SecureDir) -> Result<(), OwnershipError> {
        let lease = match self {
            Self::Legacy(owner) => {
                owner.lease.verify()?;
                &owner.lease
            }
            Self::Upgraded(owner) => {
                owner.verify_installed()?;
                &owner._lease
            }
        };
        require_same_root(&lease.root, root)
    }

    /// Controlled migration only. The live daemon must separately establish
    /// quiescence and supported migration/readiness before it uses v2 execution.
    pub fn into_upgraded(self) -> Result<Arc<UpgradedFormatOwnership>, OwnershipError> {
        match self {
            Self::Legacy(owner) => Ok(Arc::new(owner.upgrade()?)),
            Self::Upgraded(owner) => {
                owner.verify_installed()?;
                Ok(owner)
            }
        }
    }
}

impl LegacyFormatOwnership {
    /// Own an existing, durably provisioned private data root, refusing active
    /// legacy/new owners and any already-upgraded or ambiguous boundary. Missing
    /// roots are not created, and ancestor provisioning belongs to the host.
    pub fn acquire(path: impl AsRef<Path>) -> Result<Self, OwnershipError> {
        let (lease, _) = RootLease::acquire(path.as_ref(), Some(RootFormat::Legacy), None)?;
        Ok(Self { lease })
    }

    /// Durably prepare the incompatible directory without changing the legacy
    /// pathname. A complete prior preparation is resumed; incomplete evidence
    /// is never overwritten or silently removed.
    pub fn prepare(self) -> Result<PreparedFormatOwnership, OwnershipError> {
        self.lease.verify()?;
        let manifest = match leaf(&self.lease.root, RETIRED_LOCK_NAME)? {
            None => {
                let candidate = self.lease.root.create_child(RETIRED_LOCK_NAME)?;
                let manifest = FormatManifest::for_lease(&self.lease)?;
                candidate.atomic_write(MANIFEST_NAME, &serde_json::to_vec(&manifest)?)?;
                candidate.sync_all()?;
                self.lease.legacy_file.sync_all()?;
                self.lease.root.sync_all()?;
                manifest
            }
            Some(SecureLeaf::Directory) => {
                let manifest = read_manifest(&self.lease.root, RETIRED_LOCK_NAME)?;
                manifest.validate(&self.lease)?;
                self.lease
                    .root
                    .existing_child(RETIRED_LOCK_NAME)?
                    .sync_all()?;
                self.lease.legacy_file.sync_all()?;
                self.lease.root.sync_all()?;
                manifest
            }
            Some(_) => return invalid("legacy root has a non-directory preparation"),
        };
        self.lease.verify()?;
        Ok(PreparedFormatOwnership {
            lease: self.lease,
            manifest,
        })
    }

    /// Prepare and install under this exact held ownership capability.
    pub fn upgrade(self) -> Result<UpgradedFormatOwnership, OwnershipError> {
        self.prepare()?.install()
    }
}

impl PreparedFormatOwnership {
    /// Atomically swap the prepared directory with the original legacy lock.
    /// If exchange or any durability barrier fails, no upgraded guard escapes.
    /// On retry, use `LegacyFormatOwnership` for the old shape or
    /// `UpgradedFormatOwnership::open` for the installed shape; never guess.
    pub fn install(self) -> Result<UpgradedFormatOwnership, OwnershipError> {
        self.lease.verify()?;
        let actual = read_manifest(&self.lease.root, RETIRED_LOCK_NAME)?;
        actual.validate(&self.lease)?;
        if actual != self.manifest {
            return invalid("prepared manifest changed while ownership was held");
        }
        require_file_identity(&self.lease.root, LEGACY_LOCK_NAME, &self.manifest)?;
        self.lease
            .root
            .exchange_file_and_directory(LEGACY_LOCK_NAME, RETIRED_LOCK_NAME)?;
        finish_installed(self.lease, self.manifest)
    }

    pub fn manifest(&self) -> &FormatManifest {
        &self.manifest
    }
}

impl UpgradedFormatOwnership {
    pub(crate) fn verify_root(&self, root: &SecureDir) -> Result<(), OwnershipError> {
        self.verify_installed()?;
        require_same_root(&self._lease.root, root)
    }

    pub(crate) fn sessions_directory(&self) -> Result<SecureDir, OwnershipError> {
        self.verify_installed()?;
        let directory = self._lease.root.existing_child("sessions")?;
        #[cfg(unix)]
        directory.require_owner_and_private_writes(effective_uid())?;
        directory.verify_ambient_identity()?;
        Ok(directory)
    }

    /// Reopen an installed boundary, including recovery after exchange but
    /// before its parent sync. Validate both sides and repair durability before
    /// returning authority. Missing/partial/unsupported manifests are refused.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, OwnershipError> {
        let (lease, _) = RootLease::acquire(path.as_ref(), Some(RootFormat::Upgraded), None)?;
        let manifest = read_manifest(&lease.root, LEGACY_LOCK_NAME)?;
        finish_installed(lease, manifest)
    }

    pub fn manifest(&self) -> &FormatManifest {
        &self.manifest
    }

    /// Existing-only canonical legacy history location for the Session crate's
    /// owned snapshot reader. No caller path or raw public capability can select
    /// a different source, and missing history is never silently created here.
    pub(crate) fn legacy_history_directory(&self) -> Result<SecureDir, OwnershipError> {
        self.verify_installed()?;
        let history = self._lease.root.existing_child("session-history")?;
        #[cfg(unix)]
        history.require_owner_and_private_writes(effective_uid())?;
        history.verify_ambient_identity()?;
        self.verify_installed()?;
        Ok(history)
    }

    pub(crate) fn verify_installed(&self) -> Result<(), OwnershipError> {
        self._lease.verify()?;
        let actual = read_manifest(&self._lease.root, LEGACY_LOCK_NAME)?;
        actual.validate(&self._lease)?;
        if actual != self.manifest {
            return invalid("installed manifest changed while ownership was held");
        }
        require_file_identity(&self._lease.root, RETIRED_LOCK_NAME, &actual)?;
        Ok(())
    }

    /// Reopen an existing canonical namespace without creating a replacement.
    pub(crate) fn existing_session_directory(
        &self,
        session_id: &str,
    ) -> Result<SecureDir, OwnershipError> {
        self.verify_installed()?;
        let stores = self._lease.root.existing_child("execution-v2")?;
        let key = format!("{:x}", Sha256::digest(session_id.as_bytes()));
        let session = stores.existing_child(&key)?;
        #[cfg(unix)]
        session.require_owner_and_private_writes(effective_uid())?;
        session.verify_ambient_identity()?;
        self.verify_installed()?;
        Ok(session)
    }

    /// `backups/before-segments` under the held root, where a Session's
    /// directory is copied before its journals are first converted to segment
    /// logs. Created on first use; each entry is synced before it is returned.
    pub(crate) fn before_segments_backup_directory(&self) -> Result<SecureDir, OwnershipError> {
        self.verify_installed()?;
        let backups = durable_child(&self._lease.root, crate::segment_backup::BACKUPS_DIR)?;
        let directory = durable_child(&backups, crate::segment_backup::BEFORE_SEGMENTS_DIR)?;
        self.verify_installed()?;
        Ok(directory)
    }

    /// Only the canonical store can provision writable children under the held
    /// boundary. Each parent entry is synced before a storage capability escapes.
    pub(crate) fn session_directory(&self, session_id: &str) -> Result<SecureDir, OwnershipError> {
        self.verify_installed()?;
        let stores = durable_child(&self._lease.root, "execution-v2")?;
        let key = format!("{:x}", Sha256::digest(session_id.as_bytes()));
        let session = durable_child(&stores, &key)?;
        self.verify_installed()?;
        Ok(session)
    }
}

fn durable_child(parent: &SecureDir, name: &str) -> Result<SecureDir, OwnershipError> {
    let child = match parent.existing_child(name) {
        Ok(child) => child,
        Err(error) if error.kind() == io::ErrorKind::NotFound => match parent.create_child(name) {
            Ok(child) => child,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                parent.existing_child(name)?
            }
            Err(error) => return Err(error.into()),
        },
        Err(error) => return Err(error.into()),
    };
    #[cfg(unix)]
    child.require_owner_and_private_writes(effective_uid())?;
    child.sync_all()?;
    parent.sync_all()?;
    Ok(child)
}

fn finish_installed(
    lease: RootLease,
    manifest: FormatManifest,
) -> Result<UpgradedFormatOwnership, OwnershipError> {
    lease.verify()?;
    manifest.validate(&lease)?;
    require_file_identity(&lease.root, RETIRED_LOCK_NAME, &manifest)?;
    if read_manifest(&lease.root, LEGACY_LOCK_NAME)? != manifest {
        return invalid("installed manifest differs from the prepared manifest");
    }
    lease.root.existing_child(LEGACY_LOCK_NAME)?.sync_all()?;
    lease.legacy_file.sync_all()?;
    lease.root.sync_all()?;
    lease.verify()?;
    Ok(UpgradedFormatOwnership {
        _lease: lease,
        manifest,
    })
}

impl RootLease {
    fn acquire(
        path: &Path,
        expected: Option<RootFormat>,
        supplied_root: Option<&SecureDir>,
    ) -> Result<(Self, RootFormat), OwnershipError> {
        #[cfg(not(unix))]
        {
            let _ = (path, expected, supplied_root);
            Err(io::Error::new(io::ErrorKind::Unsupported, "Unix ownership locks required").into())
        }
        #[cfg(unix)]
        {
            // Always reopen independently. Cloning a SecureDir could share the
            // current controller's locked open-file description and falsely
            // acquire its own inode lock.
            let root = SecureDir::open_existing_all(path)?;
            root.require_owner_and_private_writes(effective_uid())?;
            root.verify_ambient_identity()?;
            if let Some(supplied) = supplied_root {
                require_same_root(&root, supplied)?;
            }
            let external_root = SecureDir::open_or_create_all(external_root_path())?;
            external_root.require_owner_and_private_writes(effective_uid())?;
            external_root.restrict_owner_only()?;
            let external_file = external_root.open_lock_file(external_name(&root))?;
            lock(&external_file, "controller (external lease)")?;

            let boundary = leaf(&root, LEGACY_LOCK_NAME)?;
            let retired = leaf(&root, RETIRED_LOCK_NAME)?;
            let format = match (&boundary, &retired) {
                (Some(SecureLeaf::Directory), Some(SecureLeaf::Regular { .. })) => {
                    RootFormat::Upgraded
                }
                (None, None)
                | (Some(SecureLeaf::Regular { .. }), None | Some(SecureLeaf::Directory)) => {
                    RootFormat::Legacy
                }
                _ => return invalid("expected a complete legacy/prepared or installed boundary"),
            };
            if expected.is_some_and(|expected| expected != format) {
                return invalid("ownership boundary differs from requested format");
            }
            let lock_name = match format {
                RootFormat::Legacy => LEGACY_LOCK_NAME,
                RootFormat::Upgraded => RETIRED_LOCK_NAME,
            };
            let legacy_file = root.open_lock_file(lock_name)?;
            lock(&legacy_file, "legacy controller (in-root lease)")?;
            root.lock_exclusive_waiting(axocoatl_core::LOCK_INHERITANCE_GRACE)
                .map_err(|error| {
                    if error.kind() == io::ErrorKind::WouldBlock {
                        OwnershipError::Busy("controller (data-root inode)")
                    } else {
                        OwnershipError::Io(error)
                    }
                })?;
            let lease = Self {
                root,
                external_root,
                _external_file: external_file,
                legacy_file,
            };
            lease.verify()?;
            lease.root.restrict_owner_only()?;
            if let Some(supplied) = supplied_root {
                require_same_root(&lease.root, supplied)?;
            }
            Ok((lease, format))
        }
    }

    fn verify(&self) -> Result<(), OwnershipError> {
        self.root.verify_ambient_identity()?;
        self.external_root.verify_ambient_identity()?;
        Ok(())
    }
}

fn require_same_root(opened: &SecureDir, supplied: &SecureDir) -> Result<(), OwnershipError> {
    supplied.verify_ambient_identity()?;
    opened.verify_ambient_identity()?;
    // A directly opened bootstrap capability may retain an ambient spelling
    // such as /var/... while the independent nofollow walk normalizes it to
    // /private/var/... on macOS. Both names must still resolve to their held
    // inode; their spelling is not additional evidence of directory identity.
    // The lease key and manifest use only the independently normalized root.
    if opened.inode_identity()? != supplied.inode_identity()? {
        return invalid("acquired ownership differs from the supplied bootstrap root");
    }
    Ok(())
}

impl FormatManifest {
    fn for_lease(lease: &RootLease) -> Result<Self, OwnershipError> {
        Ok(Self {
            format: FORMAT.into(),
            schema_version: SCHEMA_VERSION,
            migration_version: MIGRATION_VERSION,
            legacy_protocol: LEGACY_PROTOCOL.into(),
            ownership_id: uuid::Uuid::new_v4().to_string(),
            root_authority_sha256: root_authority(&lease.root),
            root_inode: lease.root.inode_identity()?,
            legacy_lock_inode: file_identity(&lease.legacy_file)?,
        })
    }

    fn validate(&self, lease: &RootLease) -> Result<(), OwnershipError> {
        if self.format != FORMAT
            || self.schema_version != SCHEMA_VERSION
            || self.migration_version != MIGRATION_VERSION
            || self.legacy_protocol != LEGACY_PROTOCOL
            || uuid::Uuid::parse_str(&self.ownership_id)
                .ok()
                .filter(|id| !id.is_nil() && id.to_string() == self.ownership_id)
                .is_none()
            || self.root_authority_sha256 != root_authority(&lease.root)
            || self.root_inode != lease.root.inode_identity()?
            || self.legacy_lock_inode != file_identity(&lease.legacy_file)?
        {
            return invalid("unsupported, incomplete, or differently owned manifest");
        }
        Ok(())
    }
}

fn read_manifest(root: &SecureDir, name: &str) -> Result<FormatManifest, OwnershipError> {
    let directory = root.existing_child(name)?;
    #[cfg(unix)]
    directory.require_owner_and_private_writes(effective_uid())?;
    let entries = directory.entries_limited(2)?;
    if entries.len() != 1 || entries[0].name != MANIFEST_NAME {
        return invalid("ownership directory must contain exactly its complete manifest");
    }
    Ok(serde_json::from_slice(
        &directory.read_limited(MANIFEST_NAME, MAX_MANIFEST_BYTES)?,
    )?)
}

fn leaf(root: &SecureDir, name: &str) -> Result<Option<SecureLeaf>, OwnershipError> {
    // Lock contents have no authority. Bound the read so a corrupt lock cannot
    // make ownership admission allocate arbitrary memory.
    let leaf = root.read_leaf_limited(name, MAX_MANIFEST_BYTES)?;
    if matches!(leaf, Some(SecureLeaf::Symlink { .. })) {
        return invalid("ownership boundary contains a symbolic link");
    }
    Ok(leaf)
}

fn require_file_identity(
    root: &SecureDir,
    name: &str,
    manifest: &FormatManifest,
) -> Result<(), OwnershipError> {
    let file = root.open_file_limited(name, MAX_MANIFEST_BYTES)?;
    if file_identity(&file)? != manifest.legacy_lock_inode {
        return invalid("legacy lock inode differs from the owned conversion");
    }
    Ok(())
}

fn file_identity(file: &File) -> io::Result<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata()?;
        Ok(format!("{}:{}", metadata.dev(), metadata.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = file;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Unix inode identity required",
        ))
    }
}

fn root_authority(root: &SecureDir) -> String {
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

#[cfg(unix)]
fn external_name(root: &SecureDir) -> String {
    format!("{}.lock", root_authority(root))
}

#[cfg(unix)]
fn external_root_path() -> PathBuf {
    // Exact daemon protocol; do not honor TMPDIR or an injectable lock root.
    PathBuf::from("/tmp").join(format!("axocoatl-daemon-leases-{}", effective_uid()))
}

#[cfg(unix)]
fn effective_uid() -> u32 {
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    // SAFETY: no arguments, no failure sentinel on supported Unix platforms.
    unsafe { geteuid() }
}

#[cfg(unix)]
fn lock(file: &File, owner: &'static str) -> Result<(), OwnershipError> {
    use axocoatl_core::{lock_file_exclusive_waiting, LOCK_INHERITANCE_GRACE};
    // flock(LOCK_EX | LOCK_NB), matching the existing daemon and released CLI.
    // A lease reacquired while another thread starts a process can find its
    // previous lock still shared with that child until it execs, so wait out
    // that window before reporting the lease busy.
    lock_file_exclusive_waiting(file, LOCK_INHERITANCE_GRACE).map_err(|error| {
        if error.kind() == io::ErrorKind::WouldBlock {
            OwnershipError::Busy(owner)
        } else {
            error.into()
        }
    })
}

fn invalid<T>(message: &str) -> Result<T, OwnershipError> {
    Err(OwnershipError::Invalid(message.into()))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;

    #[test]
    fn owned_legacy_history_accessor_is_existing_only_and_refuses_redirected_source() {
        let root = tempfile::tempdir().unwrap();
        let installed = LegacyFormatOwnership::acquire(root.path())
            .unwrap()
            .upgrade()
            .unwrap();
        assert!(installed.legacy_history_directory().is_err());
        assert!(!root.path().join("session-history").exists());
        let opened = SecureDir::open_existing_all(root.path()).unwrap();
        let history = opened.create_child("session-history").unwrap();
        history
            .atomic_write("turns.v1.jsonl", b"owned history bytes")
            .unwrap();
        history.sync_all().unwrap();
        opened.sync_all().unwrap();
        let source = installed.legacy_history_directory().unwrap();
        assert_eq!(
            source.read_limited("turns.v1.jsonl", 100).unwrap(),
            b"owned history bytes"
        );
        let outside = tempfile::tempdir().unwrap();
        fs::rename(
            root.path().join("session-history"),
            root.path().join("moved-history"),
        )
        .unwrap();
        symlink(outside.path(), root.path().join("session-history")).unwrap();
        assert!(installed.legacy_history_directory().is_err());
        assert!(fs::read_dir(outside.path()).unwrap().next().is_none());
    }
}
