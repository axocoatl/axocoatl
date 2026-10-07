//! The record bundle: one JSON Lines file that carries a run's whole record
//! (`axocoatl run --record <file>`, `GET /api/runs/{run_id}/record`).
//!
//! Line 1 is a [`BundleHeader`]; every following line until the last is a
//! [`BundleSection`]; the last line is a [`BundleEnd`] whose `sha256` is the
//! SHA-256 of every byte before it and whose `lines` counts the section
//! lines. `axocoatl record verify <file>` checks both. Sections, in order:
//! `manifest`, `loadout`, `outcome`, `session`, `team`, `turn` (one per turn,
//! the control-plane projection), `history` (the Session export), `network`
//! (one per network-record event), `run_event` (one per run event).
//!
//! Owner: workstream `core`.

use serde::{Deserialize, Serialize};

/// `schema` of the header line.
pub const RECORD_BUNDLE_SCHEMA: &str = "axocoatl.record-bundle/1";
/// File extension written by `axocoatl run --record` when none is given.
pub const RECORD_BUNDLE_EXTENSION: &str = "axorecord.jsonl";
/// Media type of `GET /api/runs/{run_id}/record`.
pub const RECORD_BUNDLE_MEDIA_TYPE: &str = "application/vnd.axocoatl.record+jsonl";

/// Section names, in the order they appear.
pub const BUNDLE_SECTIONS: [&str; 9] = [
    "manifest",
    "loadout",
    "outcome",
    "session",
    "team",
    "turn",
    "history",
    "network",
    "run_event",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleHeader {
    pub schema: String,
    pub run_id: String,
    pub session_id: String,
    pub created_at_ms: u64,
    /// The Axocoatl version that wrote the bundle.
    pub axocoatl_version: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BundleSection {
    pub section: String,
    pub data: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleEnd {
    pub section: String,
    pub lines: u64,
    pub sha256: String,
}

/// What verification found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleSummary {
    pub header: BundleHeader,
    pub lines: u64,
    pub sections: Vec<(String, u64)>,
}

#[derive(Debug, thiserror::Error)]
pub enum BundleError {
    #[error("record bundle: not implemented: {0}")]
    NotImplemented(&'static str),
    #[error("record bundle: {0}")]
    Invalid(String),
    #[error("record bundle: {0}")]
    Io(#[from] std::io::Error),
}

/// Writes a bundle line by line, hashing as it goes.
pub struct BundleWriter<W: std::io::Write> {
    _out: W,
}

impl<W: std::io::Write> BundleWriter<W> {
    pub fn new(_out: W, _header: &BundleHeader) -> Result<Self, BundleError> {
        Err(BundleError::NotImplemented("BundleWriter::new"))
    }

    pub fn section(
        &mut self,
        _section: &str,
        _data: &serde_json::Value,
    ) -> Result<(), BundleError> {
        Err(BundleError::NotImplemented("BundleWriter::section"))
    }

    /// Write the end line and return the writer.
    pub fn finish(self) -> Result<W, BundleError> {
        Err(BundleError::NotImplemented("BundleWriter::finish"))
    }
}

/// Verify a bundle's order, count and digest.
pub fn verify_bundle(_input: impl std::io::BufRead) -> Result<BundleSummary, BundleError> {
    Err(BundleError::NotImplemented("verify_bundle"))
}
