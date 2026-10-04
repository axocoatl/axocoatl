//! Owned component directories below a durable canonical Session journal.
//!
//! Each independently opened component root holds its own writer lock. Every
//! child retains that lock, the Session journal's locked directory descriptor,
//! and the installed format guard. No raw path or SecureDir escapes this API.
//! These capabilities authorize storage only, never execution or effect replay.

use std::io;
use std::path::{Component, Path};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use axocoatl_core::{SecureDir, SecureDirEntry};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::execution_ownership::{OwnershipError, UpgradedFormatOwnership};
use crate::execution_store::{DurableSessionIdentity, ExecutionStoreOwner};
use crate::segment_log::{SegmentError, SegmentLog, SegmentSpec};
use crate::turn_contract::{LogicalTurnId, TURN_CONTRACT_SCHEMA_VERSION};

const MAX_FILE_BYTES: usize = 64 * 1024 * 1024;
const MAX_ENTRIES: usize = 65_536;
const MAX_DEPTH: usize = 8;
const JOURNAL_INITIALIZED_FILE: &str = ".journal-initialized.v1.json";
const MAX_INITIALIZATION_BYTES: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionComponent {
    ActivationState,
    ExecutionContent,
    SessionTeam,
    WaysDecisions,
    InvocationAudit,
    ControlAuthority {
        turn_id: LogicalTurnId,
    },
    ControlCommands {
        turn_id: LogicalTurnId,
    },
    /// Session-level egress and web record. A data root that holds it may be
    /// refused by Axocoatl 1.1.2 and earlier, which do not know this kind.
    NetworkRecord,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JournalInitialization {
    schema_version: u32,
    journal_id: String,
    owner: ExecutionStoreOwner,
    component: ExecutionComponent,
    primary: String,
}

impl ExecutionComponent {
    fn directory_name(&self) -> String {
        match self {
            Self::ActivationState => "activation-state".into(),
            Self::ExecutionContent => "execution-content".into(),
            Self::SessionTeam => "session-team".into(),
            Self::WaysDecisions => "ways-decisions".into(),
            Self::InvocationAudit => "invocation-audit".into(),
            Self::NetworkRecord => "network-record".into(),
            Self::ControlAuthority { turn_id } => format!(
                "control-authority-{:x}",
                Sha256::digest(turn_id.as_str().as_bytes())
            ),
            Self::ControlCommands { turn_id } => format!(
                "control-commands-{:x}",
                Sha256::digest(turn_id.as_str().as_bytes())
            ),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum NamespaceError {
    #[error("execution namespace I/O: {0}")]
    Io(#[from] io::Error),
    #[error("execution namespace ownership: {0}")]
    Ownership(#[from] OwnershipError),
    #[error("invalid execution namespace: {0}")]
    Invalid(&'static str),
}

impl From<NamespaceError> for io::Error {
    fn from(error: NamespaceError) -> Self {
        match error {
            NamespaceError::Io(error) => error,
            error => io::Error::other(error),
        }
    }
}

struct ComponentLease {
    ownership: Arc<UpgradedFormatOwnership>,
    session: SecureDir,
    root: SecureDir,
    identity: DurableSessionIdentity,
    component: ExecutionComponent,
    poisoned: AtomicBool,
}

/// An owned root or descendant of one typed component. It is deliberately not
/// Clone; child capabilities retain all ownership through the shared lease.
pub struct OwnedExecutionNamespace {
    lease: Arc<ComponentLease>,
    dir: SecureDir,
    depth: usize,
}

impl OwnedExecutionNamespace {
    /// Called only by the canonical Session store after its journal is durable.
    /// `session` must clone its retained, locked descriptor, never reopen it.
    pub(crate) fn provision(
        session: SecureDir,
        ownership: Arc<UpgradedFormatOwnership>,
        identity: DurableSessionIdentity,
        component: ExecutionComponent,
    ) -> Result<Self, NamespaceError> {
        Self::open_component(session, ownership, identity, component, false)
    }

    pub(crate) fn existing(
        session: SecureDir,
        ownership: Arc<UpgradedFormatOwnership>,
        identity: DurableSessionIdentity,
        component: ExecutionComponent,
        primary: &Path,
    ) -> Result<Self, NamespaceError> {
        let namespace = Self::open_component(session, ownership, identity, component, true)?;
        let marker: JournalInitialization = serde_json::from_slice(
            &namespace.read_limited(JOURNAL_INITIALIZED_FILE, MAX_INITIALIZATION_BYTES)?,
        )
        .map_err(|_| NamespaceError::Invalid("component initialization is unavailable"))?;
        let expected = JournalInitialization {
            schema_version: 1,
            journal_id: namespace.identity().journal_id().into(),
            owner: namespace.identity().owner().clone(),
            component: namespace.component().clone(),
            primary: journal_primary_name(primary)?.into(),
        };
        if marker != expected {
            return Err(NamespaceError::Invalid(
                "existing component initialization differs from canonical identity",
            ));
        }
        namespace.read_limited(primary, MAX_FILE_BYTES)?;
        Ok(namespace)
    }

    fn open_component(
        session: SecureDir,
        ownership: Arc<UpgradedFormatOwnership>,
        identity: DurableSessionIdentity,
        component: ExecutionComponent,
        existing_only: bool,
    ) -> Result<Self, NamespaceError> {
        ownership.verify_installed()?;
        session.verify_ambient_identity()?;
        #[derive(Deserialize)]
        struct JournalIdentity {
            schema_version: u32,
            ownership_id: String,
            journal_id: String,
            owner: ExecutionStoreOwner,
        }
        let bytes = session.read_limited("execution.v2.json", MAX_FILE_BYTES)?;
        let journal: JournalIdentity = serde_json::from_slice(&bytes)
            .map_err(|_| NamespaceError::Invalid("canonical journal identity is unavailable"))?;
        if journal.schema_version != TURN_CONTRACT_SCHEMA_VERSION
            || journal.ownership_id != ownership.manifest().ownership_id
            || journal.journal_id != identity.journal_id()
            || &journal.owner != identity.owner()
        {
            return Err(NamespaceError::Invalid(
                "component owner differs from canonical journal",
            ));
        }
        // Open independently: sharing an existing component descriptor would
        // share its flock and incorrectly admit a second component writer.
        let root = if existing_only {
            session.existing_child(component.directory_name())?
        } else {
            durable_child(&session, component.directory_name())?
        };
        require_private(&root)?;
        lock_component(&root)?;
        let dir = root.clone();
        let namespace = Self {
            lease: Arc::new(ComponentLease {
                ownership,
                session,
                root,
                identity,
                component,
                poisoned: AtomicBool::new(false),
            }),
            dir,
            depth: 0,
        };
        namespace.verify_ambient_identity()?;
        Ok(namespace)
    }

    /// Session-crate store openers retain this namespace beside the returned
    /// descriptor and verify it on every operation. Never export the raw handle.
    pub(crate) fn secure_dir(&self) -> io::Result<SecureDir> {
        self.verify_ambient_identity()?;
        Ok(self.dir.clone())
    }

    pub fn identity(&self) -> &DurableSessionIdentity {
        &self.lease.identity
    }

    pub fn component(&self) -> &ExecutionComponent {
        &self.lease.component
    }

    /// Store openers accept exactly their component root, not an arbitrary child.
    pub fn require_root(&self, component: &ExecutionComponent) -> io::Result<()> {
        self.verify_ambient_identity()?;
        if self.depth != 0 || self.component() != component {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "wrong component root",
            ));
        }
        Ok(())
    }

    /// Called only after the primary journal read returned NotFound. Every
    /// existing entry, including the initialization marker, makes creation
    /// ambiguous and requires explicit recovery instead of an empty reset.
    pub fn check_journal_creation(&self, primary: impl AsRef<Path>) -> io::Result<()> {
        self.require_root(self.component())?;
        check_journal_creation(&self.dir, primary.as_ref())?;
        self.verify_ambient_identity()
    }

    /// Call after validating loaded/new data and before publishing its primary
    /// journal. Legacy unmarked data may acquire this marker only after it has
    /// been validated by its store. This method does not validate journal data.
    ///
    /// Marker-first publication intentionally leaves an interrupted first open
    /// fail-closed: losing the only primary file must never look like a new store.
    pub fn mark_journal_initialized(&self, primary: impl AsRef<Path>) -> io::Result<()> {
        self.require_root(self.component())?;
        self.mutation(|| {
            mark_journal_initialized(
                &self.dir,
                self.identity(),
                self.component(),
                primary.as_ref(),
            )
        })
    }

    pub fn child(&self, name: impl AsRef<Path>) -> io::Result<Self> {
        let name = direct_name(name.as_ref())?;
        self.verify_ambient_identity()?;
        if self.depth >= MAX_DEPTH {
            return Err(bounds());
        }
        let child = self.mutation(|| durable_child(&self.dir, name))?;
        Ok(Self {
            lease: self.lease.clone(),
            dir: child,
            depth: self.depth + 1,
        })
    }

    pub fn existing_child(&self, name: impl AsRef<Path>) -> io::Result<Self> {
        let name = direct_name(name.as_ref())?;
        self.verify_ambient_identity()?;
        if self.depth >= MAX_DEPTH {
            return Err(bounds());
        }
        let child = self.dir.existing_child(name)?;
        require_private(&child)?;
        child.verify_ambient_identity()?;
        self.mutation(|| {
            child.sync_all()?;
            self.dir.sync_all()
        })?;
        Ok(Self {
            lease: self.lease.clone(),
            dir: child,
            depth: self.depth + 1,
        })
    }

    pub fn read_limited(&self, name: impl AsRef<Path>, max_bytes: usize) -> io::Result<Vec<u8>> {
        let name = direct_name(name.as_ref())?;
        if max_bytes > MAX_FILE_BYTES {
            return Err(bounds());
        }
        self.verify_ambient_identity()?;
        let bytes = self.dir.read_limited(name, max_bytes)?;
        self.verify_ambient_identity()?;
        Ok(bytes)
    }

    /// A retained append-only handle to one direct child file, for stores whose
    /// journal is a log rather than a replaced document. Writes through the
    /// handle bypass `mutation`; the store must verify identity itself.
    pub(crate) fn open_append(&self, name: impl AsRef<Path>) -> io::Result<std::fs::File> {
        let name = direct_name(name.as_ref())?;
        self.verify_ambient_identity()?;
        let file = self.mutation(|| self.dir.open_append(name))?;
        self.verify_ambient_identity()?;
        Ok(file)
    }

    /// A bounded read handle to one direct child file.
    pub(crate) fn open_read(
        &self,
        name: impl AsRef<Path>,
        max_bytes: usize,
    ) -> io::Result<std::fs::File> {
        let name = direct_name(name.as_ref())?;
        if max_bytes > MAX_FILE_BYTES {
            return Err(bounds());
        }
        self.verify_ambient_identity()?;
        let file = self.dir.open_file_limited(name, max_bytes)?;
        self.verify_ambient_identity()?;
        Ok(file)
    }

    /// Open a segmented log in this component root, for a store outside this
    /// crate whose records live in one. The log keeps its own descriptor of
    /// the root and its writes bypass `mutation`, as with `open_append`: the
    /// store verifies this namespace before each write and treats a failed
    /// write as uncertain until it reopens.
    pub fn open_segment_log<R, E>(
        &self,
        spec: SegmentSpec,
        meta: serde_json::Value,
        create: bool,
        visit: impl FnMut(u64, R) -> Result<(), E>,
    ) -> Result<SegmentLog, E>
    where
        R: serde::de::DeserializeOwned,
        E: From<SegmentError>,
    {
        self.require_root(self.component())
            .map_err(|error| E::from(SegmentError::Io(error)))?;
        SegmentLog::open(self.dir.clone(), spec, meta, create, visit)
    }

    /// Whether this component root holds any part of a segmented log.
    pub fn segment_log_exists(&self, spec: &SegmentSpec) -> io::Result<bool> {
        self.require_root(self.component())?;
        SegmentLog::exists(&self.dir, spec)
    }

    /// Remove a segmented log whose content can be produced again, such as
    /// one left by an interrupted migration from a single-file store.
    pub fn remove_segment_log(&self, spec: &SegmentSpec) -> io::Result<()> {
        self.require_root(self.component())?;
        self.mutation(|| SegmentLog::remove(&self.dir, spec))
    }

    pub fn atomic_write(&self, name: impl AsRef<Path>, bytes: &[u8]) -> io::Result<()> {
        let name = direct_name(name.as_ref())?;
        if bytes.len() > MAX_FILE_BYTES {
            return Err(bounds());
        }
        self.verify_ambient_identity()?;
        self.mutation(|| self.dir.atomic_write(name, bytes))
    }

    pub fn entries_limited(&self, max_entries: usize) -> io::Result<Vec<SecureDirEntry>> {
        if max_entries > MAX_ENTRIES {
            return Err(bounds());
        }
        self.verify_ambient_identity()?;
        let entries = self.dir.entries_limited(max_entries)?;
        self.verify_ambient_identity()?;
        Ok(entries)
    }

    /// The size of one direct child regular file.
    pub fn file_len(&self, name: impl AsRef<Path>) -> io::Result<u64> {
        let name = direct_name(name.as_ref())?;
        self.verify_ambient_identity()?;
        let len = self.dir.file_len(name)?;
        self.verify_ambient_identity()?;
        Ok(len)
    }

    pub fn is_file(&self, name: impl AsRef<Path>) -> io::Result<bool> {
        let name = direct_name(name.as_ref())?;
        self.verify_ambient_identity()?;
        let found = self.dir.is_file(name)?;
        self.verify_ambient_identity()?;
        Ok(found)
    }

    pub fn sync_all(&self) -> io::Result<()> {
        self.verify_ambient_identity()?;
        self.mutation(|| self.dir.sync_all())
    }

    pub fn verify_ambient_identity(&self) -> io::Result<()> {
        if self.lease.poisoned.load(Ordering::Acquire) {
            return Err(io::Error::other(
                "component write is uncertain; drop every namespace and reopen",
            ));
        }
        self.lease
            .ownership
            .verify_installed()
            .map_err(io::Error::other)?;
        self.lease.session.verify_ambient_identity()?;
        self.lease.root.verify_ambient_identity()?;
        self.dir.verify_ambient_identity()?;
        require_private(&self.lease.session)?;
        require_private(&self.lease.root)?;
        require_private(&self.dir)
    }

    fn mutation<T>(&self, write: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
        let result = write().and_then(|result| {
            self.verify_ambient_identity()?;
            Ok(result)
        });
        if result.is_err() {
            self.lease.poisoned.store(true, Ordering::Release);
        }
        result
    }
}

/// Read an already initialized component through the held canonical owner.
/// This grants no writer lock, creates no directory/marker, and performs no
/// durability recovery. Missing or invalid storage remains a read failure.
pub(crate) fn read_existing_component(
    session: &SecureDir,
    ownership: &UpgradedFormatOwnership,
    identity: &DurableSessionIdentity,
    component: &ExecutionComponent,
    primary: &Path,
    max_bytes: usize,
) -> io::Result<Vec<u8>> {
    if max_bytes == 0 || max_bytes > MAX_FILE_BYTES {
        return Err(bounds());
    }
    let primary = journal_primary_name(primary)?;
    ownership.verify_installed().map_err(io::Error::other)?;
    session.verify_ambient_identity()?;
    require_private(session)?;
    let root = session.existing_child(component.directory_name())?;
    root.verify_ambient_identity()?;
    require_private(&root)?;
    let marker = root.read_limited(JOURNAL_INITIALIZED_FILE, MAX_INITIALIZATION_BYTES)?;
    let actual: JournalInitialization = serde_json::from_slice(&marker)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let expected = JournalInitialization {
        schema_version: 1,
        journal_id: identity.journal_id().into(),
        owner: identity.owner().clone(),
        component: component.clone(),
        primary: primary.into(),
    };
    if actual != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "historical component initialization differs from canonical identity",
        ));
    }
    let bytes = root.read_limited(primary, max_bytes)?;
    root.verify_ambient_identity()?;
    session.verify_ambient_identity()?;
    ownership.verify_installed().map_err(io::Error::other)?;
    if root.read_limited(JOURNAL_INITIALIZED_FILE, MAX_INITIALIZATION_BYTES)? != marker {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "historical component initialization changed during read",
        ));
    }
    Ok(bytes)
}

/// The root of an already initialized component, for a reader of a
/// segmented journal, with the same guarantees as [`read_existing_component`]:
/// no writer lock, no directory or marker creation, no recovery. The reader
/// must not write through it.
pub(crate) fn existing_component_root(
    session: &SecureDir,
    ownership: &UpgradedFormatOwnership,
    identity: &DurableSessionIdentity,
    component: &ExecutionComponent,
    primary: &Path,
) -> io::Result<SecureDir> {
    let primary = journal_primary_name(primary)?;
    ownership.verify_installed().map_err(io::Error::other)?;
    session.verify_ambient_identity()?;
    require_private(session)?;
    let root = session.existing_child(component.directory_name())?;
    root.verify_ambient_identity()?;
    require_private(&root)?;
    let marker = root.read_limited(JOURNAL_INITIALIZED_FILE, MAX_INITIALIZATION_BYTES)?;
    let actual: JournalInitialization = serde_json::from_slice(&marker)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let expected = JournalInitialization {
        schema_version: 1,
        journal_id: identity.journal_id().into(),
        owner: identity.owner().clone(),
        component: component.clone(),
        primary: primary.into(),
    };
    if actual != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "historical component initialization differs from canonical identity",
        ));
    }
    Ok(root)
}

/// Read one file from a direct subdirectory of an already initialized
/// component, with the same guarantees as [`read_existing_component`]: no
/// writer lock, no directory or marker creation, no recovery.
#[allow(clippy::too_many_arguments)]
pub(crate) fn read_existing_component_file(
    session: &SecureDir,
    ownership: &UpgradedFormatOwnership,
    identity: &DurableSessionIdentity,
    component: &ExecutionComponent,
    primary: &Path,
    child: &Path,
    name: &Path,
    max_bytes: usize,
) -> io::Result<Vec<u8>> {
    if max_bytes == 0 || max_bytes > MAX_FILE_BYTES {
        return Err(bounds());
    }
    let primary = journal_primary_name(primary)?;
    let child = direct_name(child)?;
    let name = direct_name(name)?;
    ownership.verify_installed().map_err(io::Error::other)?;
    session.verify_ambient_identity()?;
    require_private(session)?;
    let root = session.existing_child(component.directory_name())?;
    root.verify_ambient_identity()?;
    require_private(&root)?;
    let marker = root.read_limited(JOURNAL_INITIALIZED_FILE, MAX_INITIALIZATION_BYTES)?;
    let actual: JournalInitialization = serde_json::from_slice(&marker)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let expected = JournalInitialization {
        schema_version: 1,
        journal_id: identity.journal_id().into(),
        owner: identity.owner().clone(),
        component: component.clone(),
        primary: primary.into(),
    };
    if actual != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "historical component initialization differs from canonical identity",
        ));
    }
    let directory = root.existing_child(child)?;
    require_private(&directory)?;
    let bytes = directory.read_limited(name, max_bytes)?;
    directory.verify_ambient_identity()?;
    root.verify_ambient_identity()?;
    session.verify_ambient_identity()?;
    ownership.verify_installed().map_err(io::Error::other)?;
    Ok(bytes)
}

pub(crate) fn check_journal_creation(dir: &SecureDir, primary: &Path) -> io::Result<()> {
    journal_primary_name(primary)?;
    dir.verify_ambient_identity()?;
    // A one-entry scan suffices: more entries also fail without an unbounded
    // directory allocation. No child or temporary file is silently discarded.
    if !dir.entries_limited(1)?.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "missing primary journal in an initialized or populated component",
        ));
    }
    dir.verify_ambient_identity()
}

pub(crate) fn mark_journal_initialized(
    dir: &SecureDir,
    identity: &DurableSessionIdentity,
    component: &ExecutionComponent,
    primary: &Path,
) -> io::Result<()> {
    let primary = journal_primary_name(primary)?;
    dir.verify_ambient_identity()?;
    let expected = JournalInitialization {
        schema_version: 1,
        journal_id: identity.journal_id().into(),
        owner: identity.owner().clone(),
        component: component.clone(),
        primary: primary.into(),
    };
    match dir.read_limited(JOURNAL_INITIALIZED_FILE, MAX_INITIALIZATION_BYTES) {
        Ok(bytes) => {
            let actual: JournalInitialization = serde_json::from_slice(&bytes)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            if actual != expected {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "journal initialization identity or primary mismatch",
                ));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let bytes = serde_json::to_vec(&expected).map_err(io::Error::other)?;
    if bytes.len() > MAX_INITIALIZATION_BYTES {
        return Err(bounds());
    }
    // Re-acknowledge marker durability on reopen, just as each primary store
    // does for a prior write whose directory fsync acknowledgement was lost.
    dir.atomic_write(JOURNAL_INITIALIZED_FILE, &bytes)?;
    dir.verify_ambient_identity()
}

fn journal_primary_name(primary: &Path) -> io::Result<&str> {
    let primary = direct_name(primary)?.to_str().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "journal filename is not UTF-8")
    })?;
    if primary.len() > 128
        || primary == JOURNAL_INITIALIZED_FILE
        || primary.chars().any(char::is_control)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid primary journal filename",
        ));
    }
    Ok(primary)
}

fn durable_child(parent: &SecureDir, name: impl AsRef<Path>) -> io::Result<SecureDir> {
    let name = direct_name(name.as_ref())?;
    let child = match parent.existing_child(name) {
        Ok(child) => child,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            match parent.create_child(name.as_os_str()) {
                Ok(child) => child,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    parent.existing_child(name)?
                }
                Err(error) => return Err(error),
            }
        }
        Err(error) => return Err(error),
    };
    require_private(&child)?;
    child.verify_ambient_identity()?;
    child.sync_all()?;
    parent.sync_all()?;
    parent.verify_ambient_identity()?;
    Ok(child)
}

fn direct_name(name: &Path) -> io::Result<&Path> {
    let mut components = name.components();
    if !matches!(components.next(), Some(Component::Normal(value)) if value == name.as_os_str())
        || components.next().is_some()
        || name.as_os_str().as_encoded_bytes().len() > 255
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected one bounded direct child name",
        ));
    }
    Ok(name)
}

fn bounds() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "execution namespace bound exceeded",
    )
}

fn require_private(dir: &SecureDir) -> io::Result<()> {
    #[cfg(unix)]
    {
        dir.require_owner_and_private_writes(effective_uid())
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Unix ownership required",
        ))
    }
}

fn lock_component(dir: &SecureDir) -> io::Result<()> {
    #[cfg(unix)]
    {
        dir.lock_exclusive_waiting(axocoatl_core::LOCK_INHERITANCE_GRACE)
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Unix ownership required",
        ))
    }
}

#[cfg(unix)]
fn effective_uid() -> u32 {
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    // SAFETY: geteuid has no arguments or failure sentinel on supported Unix.
    unsafe { geteuid() }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::execution_ownership::LegacyFormatOwnership;
    use crate::execution_store::SessionExecutionStore;
    use crate::turn_contract::SessionId;
    use std::fs;
    use std::os::unix::fs::symlink;

    fn setup() -> (
        tempfile::TempDir,
        Arc<UpgradedFormatOwnership>,
        SessionExecutionStore,
    ) {
        let root = tempfile::tempdir().unwrap();
        let ownership = Arc::new(
            LegacyFormatOwnership::acquire(root.path())
                .unwrap()
                .upgrade()
                .unwrap(),
        );
        let store = SessionExecutionStore::open(
            ownership.clone(),
            ExecutionStoreOwner {
                workspace_id: "workspace".into(),
                session_id: SessionId::new("session").unwrap(),
            },
        )
        .unwrap();
        (root, ownership, store)
    }

    #[test]
    fn a_network_record_directory_leaves_every_other_component_openable() {
        let (_root, _ownership, store) = setup();
        let record = store
            .component_namespace(ExecutionComponent::NetworkRecord)
            .unwrap();
        record
            .check_journal_creation("network-record.v1.jsonl")
            .unwrap();
        let mut file = record.open_append("network-record.v1.jsonl").unwrap();
        std::io::Write::write_all(&mut file, b"{}\n").unwrap();
        record
            .mark_journal_initialized("network-record.v1.jsonl")
            .unwrap();
        assert!(store
            .path()
            .parent()
            .unwrap()
            .join("network-record")
            .join("network-record.v1.jsonl")
            .is_file());
        let turn_id = LogicalTurnId::new("turn").unwrap();
        for component in [
            ExecutionComponent::ActivationState,
            ExecutionComponent::ExecutionContent,
            ExecutionComponent::SessionTeam,
            ExecutionComponent::WaysDecisions,
            ExecutionComponent::InvocationAudit,
            ExecutionComponent::ControlAuthority {
                turn_id: turn_id.clone(),
            },
            ExecutionComponent::ControlCommands {
                turn_id: turn_id.clone(),
            },
        ] {
            let namespace = store.component_namespace(component.clone()).unwrap();
            namespace.require_root(&component).unwrap();
        }
        // The record holds its own writer lock; a second writer is refused,
        // while the existing reader path still finds the marker.
        assert!(store
            .component_namespace(ExecutionComponent::NetworkRecord)
            .is_err());
        drop(file);
        drop(record);
        let reopened = store
            .existing_component_namespace(
                ExecutionComponent::NetworkRecord,
                Path::new("network-record.v1.jsonl"),
            )
            .unwrap();
        let mut read = reopened.open_read("network-record.v1.jsonl", 1024).unwrap();
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut read, &mut bytes).unwrap();
        assert_eq!(bytes, b"{}\n");
        assert!(reopened.open_append("../escape").is_err());
        assert!(reopened
            .open_read("network-record.v1.jsonl", MAX_FILE_BYTES + 1)
            .is_err());
    }

    #[test]
    fn marker_precedes_primary_and_prevents_reset_after_interrupted_first_publication() {
        let (_root, _ownership, store) = setup();
        let namespace = store
            .component_namespace(ExecutionComponent::ExecutionContent)
            .unwrap();
        namespace.check_journal_creation("content.json").unwrap();
        namespace.mark_journal_initialized("content.json").unwrap();
        let marker = namespace
            .read_limited(JOURNAL_INITIALIZED_FILE, MAX_INITIALIZATION_BYTES)
            .unwrap();
        let decoded: JournalInitialization = serde_json::from_slice(&marker).unwrap();
        assert_eq!(decoded.owner, *store.owner());
        assert_eq!(decoded.journal_id, store.identity().unwrap().journal_id());
        assert_eq!(decoded.component, ExecutionComponent::ExecutionContent);
        assert_eq!(decoded.primary, "content.json");
        assert!(!namespace.is_file("content.json").unwrap());
        drop(namespace);
        let namespace = store
            .component_namespace(ExecutionComponent::ExecutionContent)
            .unwrap();
        assert!(namespace.check_journal_creation("content.json").is_err());
        // A store may not infer a new empty history from this crash shape.
        assert_eq!(
            namespace
                .read_limited(JOURNAL_INITIALIZED_FILE, MAX_INITIALIZATION_BYTES)
                .unwrap(),
            marker
        );
    }

    #[test]
    fn initialization_marker_requires_exact_primary_component_identity_and_known_schema() {
        for corruption in ["primary", "component", "journal", "schema", "unknown_field"] {
            let (_root, _ownership, store) = setup();
            let namespace = store
                .component_namespace(ExecutionComponent::ExecutionContent)
                .unwrap();
            // Previously valid, unmarked primary data can acquire the marker
            // only when its opener has already validated that data.
            namespace
                .atomic_write("content.json", b"validated old data")
                .unwrap();
            namespace.mark_journal_initialized("content.json").unwrap();
            namespace.mark_journal_initialized("content.json").unwrap();
            let mut marker: serde_json::Value = serde_json::from_slice(
                &namespace
                    .read_limited(JOURNAL_INITIALIZED_FILE, MAX_INITIALIZATION_BYTES)
                    .unwrap(),
            )
            .unwrap();
            match corruption {
                "primary" => marker["primary"] = "other.json".into(),
                "component" => marker["component"]["kind"] = "activation_state".into(),
                "journal" => marker["journal_id"] = "foreign-journal".into(),
                "schema" => marker["schema_version"] = 99.into(),
                _ => marker["extra"] = true.into(),
            }
            let changed = serde_json::to_vec(&marker).unwrap();
            namespace
                .atomic_write(JOURNAL_INITIALIZED_FILE, &changed)
                .unwrap();
            assert!(
                namespace.mark_journal_initialized("content.json").is_err(),
                "{corruption}"
            );
            drop(namespace);
            let namespace = store
                .component_namespace(ExecutionComponent::ExecutionContent)
                .unwrap();
            assert_eq!(
                namespace
                    .read_limited(JOURNAL_INITIALIZED_FILE, MAX_INITIALIZATION_BYTES)
                    .unwrap(),
                changed
            );
        }
    }

    #[test]
    fn journal_creation_refuses_unrelated_entries_and_initialization_is_root_only() {
        let (_root, _ownership, store) = setup();
        let namespace = store
            .component_namespace(ExecutionComponent::ExecutionContent)
            .unwrap();
        let child = namespace.child("objects").unwrap();
        assert!(namespace.check_journal_creation("content.json").is_err());
        assert!(child.check_journal_creation("content.json").is_err());
        assert!(child.mark_journal_initialized("content.json").is_err());
        assert!(!child.is_file(JOURNAL_INITIALIZED_FILE).unwrap());
        assert!(!namespace.is_file(JOURNAL_INITIALIZED_FILE).unwrap());
        assert!(namespace
            .mark_journal_initialized(JOURNAL_INITIALIZED_FILE)
            .is_err());
    }

    #[test]
    fn descendant_holds_component_session_and_format_ownership_after_parents_drop() {
        let (root, ownership, store) = setup();
        let owner = store.owner().clone();
        let namespace = store
            .component_namespace(ExecutionComponent::ActivationState)
            .unwrap();
        let child = namespace.child("objects").unwrap();
        child.atomic_write("kept", b"durable").unwrap();
        drop(namespace);
        assert!(store
            .component_namespace(ExecutionComponent::ActivationState)
            .is_err());
        drop(store);
        assert!(SessionExecutionStore::open(ownership.clone(), owner).is_err());
        drop(ownership);
        assert!(UpgradedFormatOwnership::open(root.path()).is_err());
        assert_eq!(child.read_limited("kept", 100).unwrap(), b"durable");
        drop(child);
        UpgradedFormatOwnership::open(root.path()).unwrap();
    }

    #[test]
    fn typed_roots_bounds_and_independent_component_locks_are_enforced() {
        let (_root, _ownership, store) = setup();
        let namespace = store
            .component_namespace(ExecutionComponent::ActivationState)
            .unwrap();
        namespace
            .require_root(&ExecutionComponent::ActivationState)
            .unwrap();
        assert!(namespace
            .require_root(&ExecutionComponent::ExecutionContent)
            .is_err());
        let child = namespace.child("objects").unwrap();
        assert!(child
            .require_root(&ExecutionComponent::ActivationState)
            .is_err());
        for name in [
            "../escape",
            "nested/name",
            "./alias",
            "alias/",
            "/absolute",
            "",
        ] {
            assert!(namespace.child(name).is_err(), "{name}");
            assert!(namespace.atomic_write(name, b"no").is_err(), "{name}");
        }
        assert!(namespace
            .read_limited("too-much", MAX_FILE_BYTES + 1)
            .is_err());
        assert!(namespace.entries_limited(MAX_ENTRIES + 1).is_err());
        let content = store
            .component_namespace(ExecutionComponent::ExecutionContent)
            .unwrap();
        assert_eq!(content.identity(), namespace.identity());
        content.atomic_write("content", b"other component").unwrap();
        assert!(!namespace.is_file("content").unwrap());
    }

    #[test]
    fn uncertain_mutation_blocks_all_children_until_every_old_capability_is_dropped() {
        let (_root, _ownership, store) = setup();
        let namespace = store
            .component_namespace(ExecutionComponent::ActivationState)
            .unwrap();
        let objects = namespace.child("objects").unwrap();
        objects.atomic_write("before", b"before").unwrap();
        namespace.child("blocked-file").unwrap();
        assert!(namespace
            .atomic_write("blocked-file", b"cannot replace directory")
            .is_err());
        assert!(objects.read_limited("before", 100).is_err());
        drop(namespace);
        assert!(store
            .component_namespace(ExecutionComponent::ActivationState)
            .is_err());
        drop(objects);
        let reopened = store
            .component_namespace(ExecutionComponent::ActivationState)
            .unwrap();
        assert_eq!(
            reopened
                .existing_child("objects")
                .unwrap()
                .read_limited("before", 100)
                .unwrap(),
            b"before"
        );
    }

    #[test]
    fn moved_component_and_symlinked_children_do_not_escape_retained_ownership() {
        let (_root, _ownership, store) = setup();
        let namespace = store
            .component_namespace(ExecutionComponent::ActivationState)
            .unwrap();
        let root_path = store.path().parent().unwrap().join("activation-state");
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), root_path.join("linked")).unwrap();
        assert!(namespace.existing_child("linked").is_err());
        assert!(fs::read_dir(outside.path()).unwrap().next().is_none());
        fs::rename(&root_path, root_path.with_file_name("moved-artifacts")).unwrap();
        fs::create_dir(&root_path).unwrap();
        assert!(namespace.atomic_write("refused", b"no").is_err());
        assert!(!root_path.join("refused").exists());
    }
}
