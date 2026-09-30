//! Read-only capture of the exact legacy journal beneath held format ownership.
//!
//! Missing, partial, or unsupported history is never evidence of an empty
//! conversation. This does not retire in-process legacy writers: the host must
//! quiesce those before conversion, and the first seal revalidates the source.

use std::fmt;
use std::io::{self, Read};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Write;

use crate::execution_ownership::UpgradedFormatOwnership;
use crate::execution_store::{DurableSessionIdentity, ExecutionStoreError};
use crate::turn_ledger::{SessionTurn, SessionTurnStore};

const FILE: &str = "turns.v1.jsonl";
/// Migration scans the shared journal, then bounds the selected Session alone.
const MAX_SOURCE_BYTES: usize = 256 * 1024 * 1024;
const MAX_SESSION_TURNS: usize = 2048;
const MAX_SELECTED_BYTES: usize = 32 * 1024 * 1024 - 4096;

/// Recorded source identity, not a caller-mintable ownership capability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyHistorySource {
    pub ledger_schema_version: u32,
    pub ownership_id: String,
    pub root_inode: String,
    pub directory_inode: String,
    pub file_inode: String,
    pub byte_len: u64,
    pub sha256: String,
}

impl LegacyHistorySource {
    pub(crate) fn validate(&self) -> bool {
        self.ledger_schema_version == 1
            && !self.ownership_id.is_empty()
            && !self.root_inode.is_empty()
            && !self.directory_inode.is_empty()
            && !self.file_inode.is_empty()
            && self.byte_len <= MAX_SOURCE_BYTES as u64
            && self.sha256.len() == 64
            && self
                .sha256
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    }
}

/// A fresh fold from the fixed existing source, bound to the canonical owner.
/// There is no public path-based or deserializing constructor. Holding this
/// snapshot retains format ownership until it is dropped.
pub struct OwnedLegacyHistorySnapshot {
    identity: DurableSessionIdentity,
    ownership: Arc<UpgradedFormatOwnership>,
    source: LegacyHistorySource,
    turns: Vec<SessionTurn>,
}

impl fmt::Debug for OwnedLegacyHistorySnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OwnedLegacyHistorySnapshot")
            .field("identity", &self.identity)
            .field("source", &self.source)
            .field("turn_count", &self.turns.len())
            .finish()
    }
}

impl OwnedLegacyHistorySnapshot {
    pub fn identity(&self) -> &DurableSessionIdentity {
        &self.identity
    }

    pub fn source(&self) -> &LegacyHistorySource {
        &self.source
    }

    pub fn turns(&self) -> &[SessionTurn] {
        &self.turns
    }

    pub(crate) fn capture(
        ownership: Arc<UpgradedFormatOwnership>,
        identity: DurableSessionIdentity,
    ) -> Result<Self, ExecutionStoreError> {
        let (directory, source, bytes) = read_source(&ownership)?;
        let legacy = SessionTurnStore::decode_existing_snapshot(directory, bytes)
            .map_err(|error| ExecutionStoreError::LegacyHistory(error.to_string()))?;
        let session = identity.owner().session_id.as_str();
        if legacy.session_turn_count(session) > MAX_SESSION_TURNS {
            return Err(ExecutionStoreError::Capacity);
        }
        let selected: Vec<_> = legacy.borrowed_session_turns(session).collect();
        let mut bound = SourceByteLimit { written: 0 };
        serde_json::to_writer(&mut bound, &selected).map_err(|_| ExecutionStoreError::Capacity)?;
        let turns = selected.into_iter().cloned().collect();
        Ok(Self {
            identity,
            ownership,
            source,
            turns,
        })
    }

    pub(crate) fn verify_current(&self) -> Result<(), ExecutionStoreError> {
        verify_source(&self.ownership, &self.source)
    }
}

pub(crate) fn verify_source(
    ownership: &UpgradedFormatOwnership,
    expected: &LegacyHistorySource,
) -> Result<(), ExecutionStoreError> {
    let (_, actual, _) = read_source(ownership)?;
    if &actual != expected {
        return Err(ExecutionStoreError::Invalid(
            "legacy source changed before sealing",
        ));
    }
    Ok(())
}

fn read_source(
    ownership: &UpgradedFormatOwnership,
) -> Result<(axocoatl_core::SecureDir, LegacyHistorySource, Vec<u8>), ExecutionStoreError> {
    let directory = ownership.legacy_history_directory()?;
    let mut file = directory.open_file_limited(FILE, MAX_SOURCE_BYTES)?;
    let before = file.metadata()?;
    let inode = file_identity(&before)?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(MAX_SOURCE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_SOURCE_BYTES || bytes.len() as u64 != before.len() {
        return Err(ExecutionStoreError::Invalid(
            "legacy source changed or exceeded capture limit",
        ));
    }
    if !bytes.is_empty() && !bytes.ends_with(b"\n") {
        return Err(ExecutionStoreError::Invalid(
            "legacy source has an unresolved partial tail",
        ));
    }
    // Re-establish durability without repairing or rewriting any legacy bytes.
    file.sync_all()?;
    directory.sync_all()?;
    let current = directory.open_file_limited(FILE, MAX_SOURCE_BYTES)?;
    if file_identity(&current.metadata()?)? != inode || current.metadata()?.len() != before.len() {
        return Err(ExecutionStoreError::Invalid(
            "legacy source was replaced during capture",
        ));
    }
    // A second read also catches same-inode writes while the first read ran.
    if directory.read_limited(FILE, MAX_SOURCE_BYTES)? != bytes {
        return Err(ExecutionStoreError::Invalid(
            "legacy source changed during capture",
        ));
    }
    directory.verify_ambient_identity()?;
    let named = directory.open_file_limited(FILE, MAX_SOURCE_BYTES)?;
    if file_identity(&named.metadata()?)? != inode || named.metadata()?.len() != before.len() {
        return Err(ExecutionStoreError::Invalid(
            "legacy source was replaced during verification",
        ));
    }
    ownership.verify_installed()?;
    let source = LegacyHistorySource {
        ledger_schema_version: 1,
        ownership_id: ownership.manifest().ownership_id.clone(),
        root_inode: ownership.manifest().root_inode.clone(),
        directory_inode: directory.inode_identity()?,
        file_inode: inode,
        byte_len: bytes.len() as u64,
        sha256: format!("{:x}", Sha256::digest(&bytes)),
    };
    Ok((directory, source, bytes))
}

fn file_identity(metadata: &std::fs::Metadata) -> io::Result<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != effective_uid() || metadata.mode() & 0o022 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "legacy history is not privately writable by its owner",
            ));
        }
        Ok(format!("{}:{}", metadata.dev(), metadata.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "legacy inode authority requires Unix",
        ))
    }
}

struct SourceByteLimit {
    written: usize,
}
impl Write for SourceByteLimit {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_SELECTED_BYTES.saturating_sub(self.written) {
            return Err(io::Error::other(
                "selected legacy history exceeds retention capacity",
            ));
        }
        self.written += bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(unix)]
fn effective_uid() -> u32 {
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    // SAFETY: geteuid has no arguments and does not mutate caller memory.
    unsafe { geteuid() }
}
