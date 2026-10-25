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
//!
//! A manifest records the root it was written for: the digest of its path,
//! its directory inode, and the inode of the retained lock file. A copied root
//! (a restored cold backup, a migrated Mac) has new inodes, and a moved root a
//! new path. Daemon admission ([`DataRootFormatOwnership::acquire_for_root`])
//! re-records such a manifest for the root it holds, keeping its ownership id,
//! when every other field is exact and only those identities differ, and only
//! once this process holds every lease. Exclusion never depended on the
//! recorded identities: it is the external lease and the two in-root locks,
//! all held before the manifest is read. The path-based reopen APIs stay
//! exact and refuse such a manifest.

use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axocoatl_core::{SecureDir, SecureLeaf};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The mandatory lock pathname checked before reconciliation by legacy 1.0.0.
pub const LEGACY_LOCK_NAME: &str = ".axocoatl-daemon.lock";
/// Prepared directory before exchange; retained original lock file afterwards.
pub const RETIRED_LOCK_NAME: &str = ".axocoatl-daemon.lock.v1";
/// Reserved data-root name of the manifest that re-records a copied or moved
/// root. It is written and synced here, then renamed over the boundary's
/// `format.json`, so a lock directory always holds exactly one complete
/// manifest. Admission removes one an interrupted re-recording left behind.
pub const REBIND_MANIFEST_NAME: &str = ".axocoatl-daemon.lock.rebind";
const MANIFEST_NAME: &str = "format.json";
const REFUSED_MANIFEST: &str = "unsupported, incomplete, or differently owned manifest";
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

/// What admission re-recorded for a root copied or moved since its manifest
/// was written. The ownership id, and every journal bound to it, is unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordedRelocation {
    /// The root is at another path than the recorded one.
    pub path_changed: bool,
    /// The root is another directory than the recorded one: a copy (a
    /// restored backup, a migrated Mac) or a volume mounted as another device.
    pub directory_changed: bool,
    /// The retained lock file is another file than the recorded one. A copy
    /// replaces it; so does copying a backup's files back into the same
    /// directory.
    pub lock_file_changed: bool,
}

impl RecordedRelocation {
    /// What happened to the root, for a log line or `doctor`.
    pub fn describe(&self) -> &'static str {
        match (
            self.path_changed,
            self.directory_changed,
            self.lock_file_changed,
        ) {
            (true, true, _) => "it is a copy at another path",
            (false, true, _) => {
                "it is a copy at the same path, such as a restored backup or a migrated Mac"
            }
            (true, false, false) => "it is the same directory at another path",
            (true, false, true) => {
                "it is the same directory at another path, with its files copied back into it"
            }
            (false, false, _) => "its files were copied back into the same directory",
        }
    }
}

/// A relocation admission recorded (see
/// [`DataRootFormatOwnership::acquire_for_root_relocating`]), with what the
/// manifest named before.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relocated {
    pub recorded: RecordedRelocation,
    /// The digest of the path the root was recorded at before
    /// (`root_authority_sha256`): the runtime authority a daemon on that
    /// path labelled its Podman containers and volumes with.
    pub previous_root_authority: String,
}

/// How a manifest compares with the root it is read from.
enum Admission {
    Exact,
    Relocated(RecordedRelocation),
    Refused,
}

/// The identities a manifest records, as they are for the root read now.
struct CurrentIdentity {
    root_authority: String,
    root_inode: String,
    lock_inode: String,
}

impl CurrentIdentity {
    fn of_lease(lease: &RootLease) -> Result<Self, OwnershipError> {
        Ok(Self {
            root_authority: root_authority(&lease.root),
            root_inode: lease.root.inode_identity()?,
            lock_inode: file_identity(&lease.legacy_file)?,
        })
    }
}

impl DataRootFormatOwnership {
    /// Admit the exact already-open bootstrap root using independently opened
    /// lock descriptors. Format detection occurs under the external lease, so
    /// an unsupported/partial shape never reaches cleanup or ordinary stores.
    /// A copied or moved root is re-recorded; see [`Self::acquire_for_root_reporting`].
    pub fn acquire_for_root(data_root: &SecureDir) -> Result<Self, OwnershipError> {
        Self::acquire_for_root_reporting(data_root).map(|(owner, _)| owner)
    }

    /// [`Self::acquire_for_root`], saying whether the root's manifest was
    /// re-recorded because the root was copied or moved since it was written.
    ///
    /// Only the three recorded identities are rewritten, only after every
    /// lease is held, and only when every other field and the boundary's
    /// shape are exact and the recorded identities are well formed. Holding
    /// the leases is what excludes any other controller of this directory:
    /// the root-inode lock, whatever path it is reached by, and the retained
    /// lock file, which a 1.0 controller also takes.
    pub fn acquire_for_root_reporting(
        data_root: &SecureDir,
    ) -> Result<(Self, Option<RecordedRelocation>), OwnershipError> {
        Self::acquire_for_root_relocating(data_root)
            .map(|(owner, relocated)| (owner, relocated.map(|relocated| relocated.recorded)))
    }

    /// [`Self::acquire_for_root_reporting`], with the root authority the
    /// manifest recorded before it was re-recorded.
    pub fn acquire_for_root_relocating(
        data_root: &SecureDir,
    ) -> Result<(Self, Option<Relocated>), OwnershipError> {
        let (lease, format) = RootLease::acquire(data_root.path(), None, Some(data_root))?;
        match format {
            RootFormat::Legacy => {
                let mut relocation = None;
                if matches!(
                    leaf(&lease.root, RETIRED_LOCK_NAME)?,
                    Some(SecureLeaf::Directory)
                ) {
                    relocation = admit_manifest(&lease, RETIRED_LOCK_NAME)?.1;
                }
                Ok((Self::Legacy(LegacyFormatOwnership { lease }), relocation))
            }
            RootFormat::Upgraded => {
                let (manifest, relocation) = admit_manifest(&lease, LEGACY_LOCK_NAME)?;
                Ok((
                    Self::Upgraded(Arc::new(finish_installed(lease, manifest)?)),
                    relocation,
                ))
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

    /// Whether this data root holds native History for `session_id`: a
    /// Session it created, open, closed or deleted (Delete keeps the
    /// Session's native journals). `false` for any id it never created, and
    /// whenever the store cannot be verified.
    pub fn holds_session_history(&self, session_id: &str) -> bool {
        self.existing_session_directory(session_id).is_ok()
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

/// Read the manifest in `directory` (the installed boundary, or a complete
/// preparation) for this held lease. One exact in every field but the
/// identities of the root it was recorded for is re-recorded for this root
/// first (see [`rebind`]); anything else is refused without any change.
fn admit_manifest(
    lease: &RootLease,
    directory: &str,
) -> Result<(FormatManifest, Option<Relocated>), OwnershipError> {
    let manifest = read_manifest(&lease.root, directory)?;
    let current = CurrentIdentity::of_lease(lease)?;
    match manifest.classify(&current) {
        Admission::Exact => {
            remove_interrupted_rebind(&lease.root)?;
            Ok((manifest, None))
        }
        Admission::Relocated(recorded) => {
            let previous_root_authority = manifest.root_authority_sha256.clone();
            Ok((
                rebind(lease, directory, manifest, current)?,
                Some(Relocated {
                    recorded,
                    previous_root_authority,
                }),
            ))
        }
        Admission::Refused => invalid(REFUSED_MANIFEST),
    }
}

/// Re-record `manifest` for the root this lease holds, keeping its ownership
/// id. Runs only while this process holds the external lease for this path,
/// the in-root lock file and the root-inode lock, so no other controller can
/// be using this directory, through this path or any other.
///
/// The lock directory always holds exactly one complete manifest: the new one
/// is written and synced at [`REBIND_MANIFEST_NAME`] in the root, renamed over
/// `<directory>/format.json`, and both directories are synced. A crash leaves
/// the old manifest or the new one, and at most the reserved file, which the
/// next admission removes. Nothing is ever unlinked from the lock directory.
fn rebind(
    lease: &RootLease,
    directory: &str,
    manifest: FormatManifest,
    current: CurrentIdentity,
) -> Result<FormatManifest, OwnershipError> {
    lease.verify()?;
    // The record names the lock file this process holds, so that must be the
    // file at its name now.
    let held = if directory == LEGACY_LOCK_NAME {
        RETIRED_LOCK_NAME
    } else {
        LEGACY_LOCK_NAME
    };
    if file_identity(&lease.root.open_file_limited(held, MAX_MANIFEST_BYTES)?)?
        != current.lock_inode
    {
        return invalid("held lock file differs from the one at its name");
    }
    let rebound = FormatManifest {
        root_authority_sha256: current.root_authority,
        root_inode: current.root_inode,
        legacy_lock_inode: current.lock_inode,
        ..manifest
    };
    let bytes = serde_json::to_vec(&rebound)?;
    remove_interrupted_rebind(&lease.root)?;
    let written = (|| -> Result<(), OwnershipError> {
        let mut file = lease.root.create_new(REBIND_MANIFEST_NAME)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        let boundary = lease.root.existing_child(directory)?;
        // Replaces the one regular `format.json` admitted above in one step;
        // syncs the boundary, then the root.
        lease
            .root
            .rename_leaf_to(REBIND_MANIFEST_NAME, &boundary, MANIFEST_NAME)?;
        boundary.sync_all()?;
        lease.root.sync_all()?;
        Ok(())
    })();
    if let Err(error) = written {
        let _ = remove_interrupted_rebind(&lease.root);
        return Err(error);
    }
    lease.verify()?;
    let recorded = read_manifest(&lease.root, directory)?;
    if recorded != rebound {
        return invalid("re-recorded manifest differs from the one written");
    }
    recorded.validate(lease)?;
    Ok(recorded)
}

/// Remove the reserved file an interrupted re-recording left in the root. A
/// directory there, or a file with another link, is refused, not followed.
fn remove_interrupted_rebind(root: &SecureDir) -> Result<(), OwnershipError> {
    match root.remove_leaf(REBIND_MANIFEST_NAME) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// A data root's format ownership as `axocoatl doctor` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormatOwnershipReport {
    /// No ownership boundary yet: the daemon records one when it first starts.
    Unrecorded,
    /// A 1.0 root, owned through its lock file alone.
    Legacy,
    /// A conversion that was prepared and interrupted; the next start resumes
    /// it, after re-recording a copied or moved root.
    Prepared {
        relocation: Option<RecordedRelocation>,
        interrupted_rebind: bool,
    },
    /// The installed boundary. With a relocation, the next start records the
    /// root's new location.
    Installed {
        relocation: Option<RecordedRelocation>,
        interrupted_rebind: bool,
    },
    /// The daemon refuses to start on this root, for this reason.
    Refused(String),
}

/// What daemon admission would do with the root at `path`, read without
/// taking any lease and without changing anything, so a running daemon is
/// unaffected. The checks are those of [`DataRootFormatOwnership::acquire_for_root`]
/// on the boundary's shape and manifest; a busy lease is not reported.
pub fn inspect_format_ownership(path: impl AsRef<Path>) -> FormatOwnershipReport {
    inspect(path.as_ref()).unwrap_or_else(|error| FormatOwnershipReport::Refused(error.to_string()))
}

fn inspect(path: &Path) -> Result<FormatOwnershipReport, OwnershipError> {
    // The same normalized walk the lease takes, so the same path digest.
    let root = SecureDir::open_existing_all(path)?;
    #[cfg(unix)]
    root.require_owner_and_private_writes(effective_uid())?;
    let (directory, lock) = match (
        leaf(&root, LEGACY_LOCK_NAME)?,
        leaf(&root, RETIRED_LOCK_NAME)?,
    ) {
        (None, None) => return Ok(FormatOwnershipReport::Unrecorded),
        (Some(SecureLeaf::Regular { .. }), None) => return Ok(FormatOwnershipReport::Legacy),
        (Some(SecureLeaf::Regular { .. }), Some(SecureLeaf::Directory)) => {
            (RETIRED_LOCK_NAME, LEGACY_LOCK_NAME)
        }
        (Some(SecureLeaf::Directory), Some(SecureLeaf::Regular { .. })) => {
            (LEGACY_LOCK_NAME, RETIRED_LOCK_NAME)
        }
        _ => return invalid("expected a complete legacy/prepared or installed boundary"),
    };
    let manifest = read_manifest(&root, directory)?;
    let current = CurrentIdentity {
        root_authority: root_authority(&root),
        root_inode: root.inode_identity()?,
        lock_inode: file_identity(&root.open_file_limited(lock, MAX_MANIFEST_BYTES)?)?,
    };
    let relocation = match manifest.classify(&current) {
        Admission::Exact => None,
        Admission::Relocated(relocation) => Some(relocation),
        Admission::Refused => return invalid(REFUSED_MANIFEST),
    };
    let interrupted_rebind = !matches!(
        root.read_leaf_limited(REBIND_MANIFEST_NAME, MAX_MANIFEST_BYTES),
        Ok(None)
    );
    Ok(if directory == LEGACY_LOCK_NAME {
        FormatOwnershipReport::Installed {
            relocation,
            interrupted_rebind,
        }
    } else {
        FormatOwnershipReport::Prepared {
            relocation,
            interrupted_rebind,
        }
    })
}

/// The external lease of the data root path whose digest
/// ([`Relocated::previous_root_authority`]) is `authority`, held while this
/// lives: no daemon runs on a data root at that path meanwhile, as each one
/// holds it while it runs.
#[derive(Debug)]
pub struct PathLease {
    _external_root: SecureDir,
    _file: File,
}

impl PathLease {
    /// Hold the lease of the path whose digest is `authority`; `None` when a
    /// daemon at that path holds it.
    pub fn hold(authority: &str) -> Result<Option<Self>, OwnershipError> {
        if authority.len() != 64 || !authority.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return invalid("a data root path digest is 64 hexadecimal digits");
        }
        #[cfg(not(unix))]
        {
            Err(io::Error::new(io::ErrorKind::Unsupported, "Unix ownership locks required").into())
        }
        #[cfg(unix)]
        {
            let external_root = SecureDir::open_or_create_all(external_root_path())?;
            external_root.require_owner_and_private_writes(effective_uid())?;
            external_root.restrict_owner_only()?;
            let file = external_root.open_lock_file(format!("{authority}.lock"))?;
            match lock(&file, "controller (external lease)") {
                Ok(()) => Ok(Some(Self {
                    _external_root: external_root,
                    _file: file,
                })),
                Err(OwnershipError::Busy(_)) => Ok(None),
                Err(error) => Err(error),
            }
        }
    }
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

    /// Exactly this held root's manifest: no re-recording here.
    fn validate(&self, lease: &RootLease) -> Result<(), OwnershipError> {
        match self.classify(&CurrentIdentity::of_lease(lease)?) {
            Admission::Exact => Ok(()),
            Admission::Relocated(_) | Admission::Refused => invalid(REFUSED_MANIFEST),
        }
    }

    fn classify(&self, current: &CurrentIdentity) -> Admission {
        if self.format != FORMAT
            || self.schema_version != SCHEMA_VERSION
            || self.migration_version != MIGRATION_VERSION
            || self.legacy_protocol != LEGACY_PROTOCOL
            || uuid::Uuid::parse_str(&self.ownership_id)
                .ok()
                .filter(|id| !id.is_nil() && id.to_string() == self.ownership_id)
                .is_none()
        {
            return Admission::Refused;
        }
        let relocation = RecordedRelocation {
            path_changed: self.root_authority_sha256 != current.root_authority,
            directory_changed: self.root_inode != current.root_inode,
            lock_file_changed: self.legacy_lock_inode != current.lock_inode,
        };
        if !relocation.path_changed
            && !relocation.directory_changed
            && !relocation.lock_file_changed
        {
            return Admission::Exact;
        }
        if !self.has_recordable_identities() {
            return Admission::Refused;
        }
        Admission::Relocated(relocation)
    }

    /// Whether the recorded identities have the shapes this module writes: a
    /// lowercase SHA-256 and two `device:inode` pairs. Anything else is a
    /// damaged manifest, not one written for another location.
    fn has_recordable_identities(&self) -> bool {
        let digest = self.root_authority_sha256.len() == 64
            && self
                .root_authority_sha256
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'));
        let number = |part: &str| {
            !part.is_empty() && part.len() <= 20 && part.bytes().all(|byte| byte.is_ascii_digit())
        };
        // A signed `dev_t` (macOS) can print with a sign.
        let inode = |value: &str| {
            value.split_once(':').is_some_and(|(device, inode)| {
                number(device.strip_prefix('-').unwrap_or(device)) && number(inode)
            })
        };
        digest && inode(&self.root_inode) && inode(&self.legacy_lock_inode)
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

    /// The lease of a path is the one a daemon on a data root there holds:
    /// held by neither while the other holds it.
    #[test]
    fn a_path_lease_is_held_only_while_no_daemon_runs_at_that_path() {
        let root = crate::private_tempdir_test_support::private_tempdir();
        let authority = root_authority(&SecureDir::open_existing_all(root.path()).unwrap());
        let daemon = LegacyFormatOwnership::acquire(root.path()).unwrap();
        assert!(PathLease::hold(&authority).unwrap().is_none());
        drop(daemon);
        let lease = PathLease::hold(&authority).unwrap().unwrap();
        assert!(matches!(
            LegacyFormatOwnership::acquire(root.path()),
            Err(OwnershipError::Busy(_))
        ));
        drop(lease);
        LegacyFormatOwnership::acquire(root.path()).unwrap();
        assert!(PathLease::hold("../escape").is_err());
    }

    #[test]
    fn owned_legacy_history_accessor_is_existing_only_and_refuses_redirected_source() {
        let root = crate::private_tempdir_test_support::private_tempdir();
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
        let outside = crate::private_tempdir_test_support::private_tempdir();
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
