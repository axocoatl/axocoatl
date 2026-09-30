//! Exact, read-only checkpoint capture before changing the Session execution format.
//!
//! The caller must hold format ownership and quiesce legacy actors. The store's
//! transaction lock covers capture, but the returned snapshot is not a writer
//! lease. Its source must be revalidated immediately before a host durably seals
//! it with canonical History. Unknown or private state is retained byte for byte;
//! no historical role, accepted generation, or accounting completeness is guessed.

use super::*;
use axocoatl_core::secure_fs::SecureEntryType;
use sha2::{Digest, Sha256};

const MAX_CAPTURE_AGENTS: usize = 128;
const MAX_CAPTURE_FILES: usize = 4096;
const MAX_CAPTURE_BYTES: usize = 128 * 1024 * 1024;
const ARCHIVE_MAGIC: &[u8] = b"AXOLEGACYCKPT\0\x01";

#[derive(Debug, Serialize)]
struct SourceFile {
    path: String,
    directory_inode: String,
    file_inode: String,
    byte_len: usize,
    sha256: String,
}

#[derive(Debug, Serialize)]
struct SourceManifest {
    schema_version: u32,
    session_id: String,
    base_inode: String,
    data_root_inode: Option<String>,
    files: Vec<SourceFile>,
}

/// Not deserializable or caller-constructible. `archive_bytes` retains all
/// selected source versions, including private orchestration state and any old
/// malformed cache; `checkpoints` exposes only the exact newest valid payloads.
/// A corrupt newest file rejects capture rather than silently losing accounting
/// by falling back to an older checkpoint.
pub struct LegacySessionCheckpointSnapshot {
    session_id: String,
    source_sha256: String,
    data_root_inode: Option<String>,
    archive: Vec<u8>,
    checkpoints: Vec<AgentCheckpoint>,
    source_store: CheckpointStore,
}

impl LegacySessionCheckpointSnapshot {
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub(crate) fn verify_captured_source(&self) -> Result<(), MemoryError> {
        self.verify_current(&self.source_store)
    }

    pub(crate) fn data_root_inode(&self) -> Option<&str> {
        self.data_root_inode.as_deref()
    }

    pub fn source_sha256(&self) -> &str {
        &self.source_sha256
    }

    /// Bounded archive: magic, little-endian u64 JSON-manifest length, manifest,
    /// then exact file bytes in manifest order. It is evidence, not executable state.
    pub fn archive_bytes(&self) -> &[u8] {
        &self.archive
    }

    pub fn checkpoints(&self) -> impl Iterator<Item = &AgentCheckpoint> {
        self.checkpoints.iter()
    }

    pub fn checkpoint(&self, exact_agent_id: &str) -> Option<&AgentCheckpoint> {
        self.checkpoints
            .iter()
            .find(|item| item.agent_id == exact_agent_id)
    }

    /// Recapture beneath the same store lock, including newly added or removed
    /// Session-owned actors and transactions. Equal payloads at a replaced inode
    /// are a changed source. Never holds a lock across an await.
    pub fn verify_current(&self, store: &CheckpointStore) -> Result<(), MemoryError> {
        let current = store.capture_legacy_session_checkpoints(&self.session_id)?;
        if current.source_sha256 != self.source_sha256 {
            return invalid("legacy Session checkpoint source changed before sealing");
        }
        Ok(())
    }
}

impl CheckpointStore {
    /// Verify the actual fixed checkpoint child before a migration caller
    /// reconciles any transaction. No path-prefix claim can authorize a write
    /// to a checkpoint store from another data root.
    pub fn verify_legacy_data_root(&self, root: &SecureDir) -> Result<(), MemoryError> {
        root.verify_ambient_identity()?;
        let expected = root.existing_child("checkpoints")?;
        let actual = self
            .secure_base
            .clone()
            .map(Ok)
            .unwrap_or_else(|| SecureDir::open(&self.base_dir))?;
        actual.verify_ambient_identity()?;
        if expected.inode_identity()? != actual.inode_identity()? {
            return invalid("legacy checkpoint store belongs to another data root");
        }
        Ok(())
    }

    pub fn capture_legacy_session_checkpoints(
        &self,
        session_id: &str,
    ) -> Result<LegacySessionCheckpointSnapshot, MemoryError> {
        validate_transaction_identity("session", session_id)?;
        if self.transaction_scope.is_some() {
            return invalid("legacy capture requires the committed checkpoint store");
        }
        let _guard = self.transaction_guard()?;
        let base = self
            .secure_base
            .clone()
            .map(Ok)
            .unwrap_or_else(|| SecureDir::open(&self.base_dir))?;
        base.verify_ambient_identity()?;
        let mut files = Vec::new();
        capture_settled_transactions(&base, session_id, &mut files)?;
        let identities = exact_session_identities(&base, session_id)?;
        if identities.len() > MAX_CAPTURE_AGENTS {
            return invalid("legacy checkpoint capture exceeds its Agent bound");
        }
        let mut checkpoints = Vec::new();
        for agent_id in identities {
            let agent = AgentId::new(&agent_id);
            let mut candidates = self.committed_checkpoint_candidates(&base, &agent)?;
            candidates.sort_by(|a, b| b.version.cmp(&a.version).then(b.priority.cmp(&a.priority)));
            if candidates.is_empty() {
                return invalid("recorded legacy Agent has no checkpoint source");
            }
            for (index, candidate) in candidates.into_iter().enumerate() {
                capture_file(
                    &base,
                    &candidate.dir,
                    &candidate.name,
                    MAX_CHECKPOINT_BYTES,
                    &mut files,
                )?;
                if index == 0 {
                    let latest_bytes = &files.last().expect("just captured source").1;
                    let decoded = if latest_bytes.starts_with(CHECKPOINT_MAGIC) {
                        decode_current(latest_bytes)
                    } else {
                        decode_unframed(latest_bytes, &agent, candidate.version).map(|item| item.0)
                    }
                    .map_err(MemoryError::Invalid)?;
                    validate_identity(&decoded, &agent, candidate.version)
                        .map_err(MemoryError::Invalid)?;
                    checkpoints.push(decoded);
                }
            }
        }
        files.sort_by(|a, b| a.0.path.cmp(&b.0.path));
        let byte_len = files
            .iter()
            .try_fold(0_usize, |total, (_, bytes)| total.checked_add(bytes.len()))
            .ok_or_else(|| MemoryError::Invalid("legacy capture size overflow".into()))?;
        if files.len() > MAX_CAPTURE_FILES || byte_len > MAX_CAPTURE_BYTES {
            return invalid("legacy checkpoint archive exceeds its retention bound");
        }
        let (sources, bodies): (Vec<_>, Vec<_>) = files.into_iter().unzip();
        let data_root_inode = standard_data_root_inode(&base)?;
        let manifest = SourceManifest {
            schema_version: 1,
            session_id: session_id.to_owned(),
            base_inode: base.inode_identity()?,
            data_root_inode: data_root_inode.clone(),
            files: sources,
        };
        let header = serde_json::to_vec(&manifest)?;
        if header.len() > 4 * 1024 * 1024 {
            return invalid("legacy checkpoint manifest exceeds its retention bound");
        }
        let mut archive = Vec::with_capacity(ARCHIVE_MAGIC.len() + 8 + header.len() + byte_len);
        archive.extend_from_slice(ARCHIVE_MAGIC);
        archive.extend_from_slice(&(header.len() as u64).to_le_bytes());
        archive.extend_from_slice(&header);
        for bytes in bodies {
            archive.extend_from_slice(&bytes);
        }
        base.verify_ambient_identity()?;
        base.sync_all()?;
        Ok(LegacySessionCheckpointSnapshot {
            session_id: session_id.to_owned(),
            source_sha256: format!("{:x}", Sha256::digest(&archive)),
            data_root_inode,
            archive,
            checkpoints,
            source_store: CheckpointStore {
                base_dir: self.base_dir.clone(),
                secure_base: Some(base),
                policy: self.policy.clone(),
                transaction_scope: None,
                transaction_lock: self.transaction_lock.clone(),
            },
        })
    }
}

fn standard_data_root_inode(base: &SecureDir) -> Result<Option<String>, MemoryError> {
    if base.path().file_name() != Some(std::ffi::OsStr::new("checkpoints")) {
        return Ok(None);
    }
    let parent = base
        .path()
        .parent()
        .ok_or_else(|| MemoryError::Invalid("checkpoint data root is absent".into()))?;
    let root = SecureDir::open(parent)?;
    root.verify_ambient_identity()?;
    let expected = root.existing_child("checkpoints")?;
    if expected.inode_identity()? != base.inode_identity()? {
        return invalid("checkpoint root is not the canonical data-root child");
    }
    Ok(Some(root.inode_identity()?))
}

fn exact_session_identities(
    base: &SecureDir,
    session_id: &str,
) -> Result<BTreeSet<String>, MemoryError> {
    let prefix = format!("{session_id}:");
    let mut identities = BTreeSet::new();
    match base.existing_child("v1") {
        Ok(root) => {
            for entry in root.entries()? {
                if entry.file_type != SecureEntryType::Directory {
                    return invalid("checkpoint identity root contains an unowned entry");
                }
                let dir = root.existing_child(Path::new(&entry.name))?;
                let candidates = checkpoint_candidates(&dir, 1)?;
                if candidates.is_empty() {
                    if !dir.entries()?.is_empty() {
                        return invalid("checkpoint directory contains unrecognized state");
                    }
                    continue;
                }
                let agent_id =
                    discover_checkpoint_identity(&dir, &entry.name)?.ok_or_else(|| {
                        MemoryError::Invalid(
                            "checkpoint ownership cannot be recovered from any source version"
                                .into(),
                        )
                    })?;
                if agent_id.starts_with(&prefix) {
                    validate_source_entries(&dir)?;
                    identities.insert(agent_id);
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    // Before the hashed v1 layout, scoped identities were literal components.
    for entry in base.entries()? {
        let Some(name) = entry.name.to_str().filter(|name| name.starts_with(&prefix)) else {
            continue;
        };
        if entry.file_type != SecureEntryType::Directory
            || legacy_storage_component(name) != Some(name)
        {
            return invalid("legacy Session checkpoint path is not an owned directory");
        }
        let dir = base.existing_child(name)?;
        validate_source_entries(&dir)?;
        if !checkpoint_candidates(&dir, 0)?.is_empty() {
            identities.insert(name.to_owned());
        }
    }
    Ok(identities)
}

fn validate_source_entries(dir: &SecureDir) -> Result<(), MemoryError> {
    for entry in dir.entries()? {
        if entry.file_type != SecureEntryType::File
            || checkpoint_filename_version(Path::new(&entry.name)).is_none()
        {
            return invalid(
                "Session checkpoint directory contains unrecognized or unfinished state",
            );
        }
    }
    Ok(())
}

fn capture_settled_transactions(
    base: &SecureDir,
    session_id: &str,
    files: &mut Vec<(SourceFile, Vec<u8>)>,
) -> Result<(), MemoryError> {
    let session_key = storage_key(session_id);
    let root = match base.existing_child(CheckpointStore::transaction_root().join(&session_key)) {
        Ok(root) => root,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for entry in root.entries()? {
        if entry.file_type != SecureEntryType::Directory {
            return invalid("legacy checkpoint transaction contains an unowned entry");
        }
        let dir = root.existing_child(Path::new(&entry.name))?;
        let bytes = dir.read_limited(
            SESSION_TURN_TRANSACTION_MANIFEST,
            SESSION_TURN_TRANSACTION_MANIFEST_MAX_BYTES,
        )?;
        let manifest: CheckpointTransactionManifest = serde_json::from_slice(&bytes)?;
        require_manifest_storage_keys(&manifest, std::ffi::OsStr::new(&session_key), &entry.name)?;
        if manifest.format_version != SESSION_TURN_TRANSACTION_FORMAT_VERSION
            || manifest.session_id != session_id
            || manifest.state != CheckpointTransactionState::Committed
        {
            return invalid("legacy checkpoint transaction must be resolved before capture");
        }
        // Read directly: the ordinary manifest helper also removes temporary
        // files, which would mutate the source during a read-only capture.
        for child in dir.entries()? {
            if child.name != std::ffi::OsStr::new(SESSION_TURN_TRANSACTION_MANIFEST) {
                return invalid("committed legacy checkpoint transaction retains unfinished state");
            }
        }
        capture_file(
            base,
            &dir,
            std::ffi::OsStr::new(SESSION_TURN_TRANSACTION_MANIFEST),
            SESSION_TURN_TRANSACTION_MANIFEST_MAX_BYTES,
            files,
        )?;
    }
    Ok(())
}

fn capture_file(
    base: &SecureDir,
    dir: &SecureDir,
    name: &std::ffi::OsStr,
    limit: usize,
    files: &mut Vec<(SourceFile, Vec<u8>)>,
) -> Result<(), MemoryError> {
    if files.len() >= MAX_CAPTURE_FILES {
        return invalid("legacy checkpoint capture exceeds its file bound");
    }
    let file = dir.open_file_limited(name, limit)?;
    let file_inode = file_identity(&file.metadata()?)?;
    let bytes = dir.read_limited(name, limit)?;
    let retained_bytes: usize = files.iter().map(|(_, body)| body.len()).sum();
    if bytes.len() > MAX_CAPTURE_BYTES.saturating_sub(retained_bytes) {
        return invalid("legacy checkpoint capture exceeds its byte bound");
    }
    file.sync_all()?;
    dir.sync_all()?;
    let current = dir.open_file_limited(name, limit)?;
    if file_identity(&current.metadata()?)? != file_inode || dir.read_limited(name, limit)? != bytes
    {
        return invalid("legacy checkpoint source changed during capture");
    }
    dir.verify_ambient_identity()?;
    let path = dir
        .path()
        .join(name)
        .strip_prefix(base.path())
        .map_err(|_| MemoryError::Invalid("checkpoint source escaped its root".into()))?
        .to_path_buf();
    let path = path
        .to_str()
        .ok_or_else(|| MemoryError::Invalid("checkpoint source path is not UTF-8".into()))?
        .to_owned();
    files.push((
        SourceFile {
            path,
            directory_inode: dir.inode_identity()?,
            file_inode,
            byte_len: bytes.len(),
            sha256: format!("{:x}", Sha256::digest(&bytes)),
        },
        bytes,
    ));
    Ok(())
}

fn file_identity(metadata: &std::fs::Metadata) -> Result<String, MemoryError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(format!("{}:{}", metadata.dev(), metadata.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        invalid("legacy checkpoint source identity requires Unix")
    }
}

fn invalid<T>(message: &str) -> Result<T, MemoryError> {
    Err(MemoryError::Invalid(message.to_owned()))
}

#[cfg(test)]
#[path = "checkpoint_legacy_capture_tests.rs"]
mod tests;
