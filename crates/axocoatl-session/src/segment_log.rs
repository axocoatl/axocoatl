//! Segmented append-only storage for a per-Session journal whose history grows
//! for the Session's whole life.
//!
//! Layout inside one component directory, for a log named `<name>`:
//!
//! - `<name>.active.jsonl` is the active segment. Its first line is the
//!   segment header and every later line is one record. An append writes one
//!   whole line and syncs it before it is acknowledged, so a crash can leave
//!   only an unfinished last line, which was never acknowledged; reopening
//!   removes it.
//! - `segments/<name>.<index>.jsonl` are sealed segments. A sealed segment is
//!   the exact bytes its active segment had, followed by one seal line with
//!   its record count and the SHA-256 of every byte before it. Sealed files
//!   are written once and never changed.
//!
//! Every header names the digest of the segment before it (`previous`), so a
//! sealed segment cannot be changed, removed or reordered without the chain
//! breaking; opening verifies the whole chain and every seal.
//!
//! Memory stays bounded: the log keeps one small summary per sealed segment,
//! and records are read back one segment at a time. Stores keep their own
//! bounded working set (the active segment and work still in flight), a
//! [`KeyFilter`] per sealed segment to find older records by key, and a
//! [`SegmentCache`] of a few decoded segments.

use std::collections::VecDeque;
use std::hash::{Hash, Hasher};
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use axocoatl_core::SecureDir;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const SCHEMA: u32 = 1;
const SEGMENTS_DIR: &str = "segments";
/// Upper bound on one header or seal line.
const MAX_FRAME_LINE_BYTES: usize = 64 * 1024;
/// Times a reader retries when a writer sealed a segment under it.
const READ_ATTEMPTS: usize = 4;

#[derive(Debug, thiserror::Error)]
pub enum SegmentError {
    #[error("segment storage: {0}")]
    Io(#[from] io::Error),
    #[error("segment JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid segment: {0}")]
    Invalid(&'static str),
    #[error("segment record exceeds the per-record bound")]
    RecordTooLarge,
    #[error("ambiguous segment write; reopen to recover before writing again")]
    RecoveryRequired,
    #[error("the log changed while it was being read")]
    Changed,
}

/// The fixed shape of one log: its file name, its kind (bound into every
/// header) and when its active segment is sealed.
#[derive(Debug, Clone, Copy)]
pub struct SegmentSpec {
    pub name: &'static str,
    pub kind: &'static str,
    /// Seal the active segment once it holds this many bytes...
    pub segment_bytes: usize,
    /// ...or this many records.
    pub segment_records: u64,
    /// No one record line may be longer.
    pub record_bytes: usize,
}

impl SegmentSpec {
    pub fn active_name(&self) -> String {
        format!("{}.active.jsonl", self.name)
    }
    fn sealed_name(&self, index: u64) -> String {
        format!("{}.{index:010}.jsonl", self.name)
    }
    fn sealed_index(&self, file_name: &str) -> Option<u64> {
        let rest = file_name
            .strip_prefix(self.name)?
            .strip_prefix('.')?
            .strip_suffix(".jsonl")?;
        (rest.len() == 10 && rest.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| rest.parse().ok())
            .flatten()
    }
    /// The most bytes an active or sealed segment file can hold.
    fn max_segment_file_bytes(&self) -> usize {
        self.segment_bytes
            .saturating_add(self.record_bytes)
            .saturating_add(2 * MAX_FRAME_LINE_BYTES)
    }
}

/// Written into a store's head file when its records live in a segment log.
/// Its presence also makes an older daemon, which knows only the single-file
/// layout, refuse the store instead of reading part of its history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SegmentsMarker {
    pub schema: u32,
    pub kind: String,
}

impl SegmentsMarker {
    pub fn of(spec: &SegmentSpec) -> Self {
        Self {
            schema: SCHEMA,
            kind: spec.kind.to_owned(),
        }
    }
    pub fn matches(&self, spec: &SegmentSpec) -> bool {
        self == &Self::of(spec)
    }
}

/// First line of every segment. `meta` is the store's identity (owner and
/// journal ids), which every segment must repeat exactly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SegmentHeader {
    pub schema: u32,
    pub kind: String,
    pub index: u64,
    pub previous: Option<String>,
    pub first_sequence: u64,
    pub meta: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SegmentSeal {
    records: u64,
    digest: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
enum Line<R> {
    #[serde(rename = "header")]
    Header(SegmentHeader),
    #[serde(rename = "record")]
    Record(R),
    #[serde(rename = "seal")]
    Seal(SegmentSeal),
}

#[derive(Serialize)]
enum LineRef<'a, R> {
    #[serde(rename = "header")]
    Header(&'a SegmentHeader),
    #[serde(rename = "record")]
    Record(&'a R),
    #[serde(rename = "seal")]
    Seal(&'a SegmentSeal),
}

/// What the log keeps about one sealed segment: enough to find, verify and
/// read it again, never its records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealedSegment {
    pub index: u64,
    pub first_sequence: u64,
    pub records: u64,
    pub digest: String,
}

/// How opening found the active segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SegmentRecovery {
    /// Bytes of an unfinished last line removed from the active segment.
    pub torn_bytes: u64,
    /// A seal that was written before its successor segment existed was
    /// completed.
    pub completed_seal: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Writer { create: bool },
    Reader,
}

/// One segmented append-only log. A writer holds the component directory's
/// exclusive lock for this value's lifetime; a reader never writes.
#[derive(Debug)]
pub struct SegmentLog {
    dir: SecureDir,
    spec: SegmentSpec,
    header: SegmentHeader,
    sealed: Vec<SealedSegment>,
    active_bytes: usize,
    active_records: u64,
    recovery: SegmentRecovery,
    recovery_required: bool,
    reader: bool,
}

impl SegmentLog {
    /// Whether this directory holds any part of a segmented log of `spec`.
    pub fn exists(dir: &SecureDir, spec: &SegmentSpec) -> io::Result<bool> {
        Ok(dir.has_exact_file(spec.active_name())? || dir.has_exact_directory(SEGMENTS_DIR)?)
    }

    /// Remove every file of a log of `spec`. Only for a log whose content can
    /// be produced again from elsewhere, such as an interrupted migration
    /// from a single-file store.
    pub fn remove(dir: &SecureDir, spec: &SegmentSpec) -> io::Result<()> {
        if dir.has_exact_directory(SEGMENTS_DIR)? {
            dir.remove_dir_all(SEGMENTS_DIR)?;
        }
        if dir.has_exact_file(spec.active_name())? {
            dir.remove_file(spec.active_name())?;
        }
        dir.sync_all()
    }

    /// Open the log for writing, verify its chain, and hand every record to
    /// `visit` in order with its sequence (from 1). A missing log is created
    /// only when `create` is true. A torn last line is removed and an
    /// interrupted seal is completed.
    pub fn open<R, E>(
        dir: SecureDir,
        spec: SegmentSpec,
        meta: serde_json::Value,
        create: bool,
        mut visit: impl FnMut(u64, R) -> Result<(), E>,
    ) -> Result<Self, E>
    where
        R: DeserializeOwned,
        E: From<SegmentError>,
    {
        Self::open_indexed(dir, spec, meta, create, |_, sequence, record| {
            visit(sequence, record)
        })
    }

    /// [`SegmentLog::open`], also naming each record's segment index (the
    /// active segment's index is the number of sealed segments).
    pub fn open_indexed<R, E>(
        dir: SecureDir,
        spec: SegmentSpec,
        meta: serde_json::Value,
        create: bool,
        visit: impl FnMut(u64, u64, R) -> Result<(), E>,
    ) -> Result<Self, E>
    where
        R: DeserializeOwned,
        E: From<SegmentError>,
    {
        match open_inner(dir, spec, meta, Mode::Writer { create }, visit) {
            Ok(log) => Ok(log),
            Err(Retry::Failed(error)) => Err(error),
            Err(Retry::Changed) => Err(SegmentError::Changed.into()),
        }
    }

    /// Read every record without writing anything, while a writer may be
    /// appending: an unfinished last line is skipped and an interrupted seal
    /// is read from its sealed file. `start` is called before each attempt,
    /// because a writer that seals a segment under the reader makes it start
    /// over.
    pub fn read<R, E>(
        dir: SecureDir,
        spec: SegmentSpec,
        meta: serde_json::Value,
        start: impl FnMut(),
        mut visit: impl FnMut(u64, R) -> Result<(), E>,
    ) -> Result<Self, E>
    where
        R: DeserializeOwned,
        E: From<SegmentError>,
    {
        Self::read_indexed(dir, spec, meta, start, |_, sequence, record| {
            visit(sequence, record)
        })
    }

    /// [`SegmentLog::read`], also naming each record's segment index.
    pub fn read_indexed<R, E>(
        dir: SecureDir,
        spec: SegmentSpec,
        meta: serde_json::Value,
        mut start: impl FnMut(),
        mut visit: impl FnMut(u64, u64, R) -> Result<(), E>,
    ) -> Result<Self, E>
    where
        R: DeserializeOwned,
        E: From<SegmentError>,
    {
        for _ in 0..READ_ATTEMPTS {
            start();
            let handle = reopen(&dir).map_err(|error| E::from(error.into()))?;
            match open_inner(handle, spec, meta.clone(), Mode::Reader, &mut visit) {
                Ok(log) => return Ok(log),
                Err(Retry::Changed) => continue,
                Err(Retry::Failed(error)) => return Err(error),
            }
        }
        Err(SegmentError::Changed.into())
    }

    pub fn recovery(&self) -> SegmentRecovery {
        self.recovery
    }

    pub fn sealed(&self) -> &[SealedSegment] {
        &self.sealed
    }

    pub fn active_records(&self) -> u64 {
        self.active_records
    }

    pub fn active_bytes(&self) -> usize {
        self.active_bytes
    }

    /// Sequence the next appended record receives.
    pub fn next_sequence(&self) -> u64 {
        self.header.first_sequence + self.active_records
    }

    /// The active segment's index: the number of sealed segments.
    pub fn active_index(&self) -> u64 {
        self.header.index
    }

    /// The sequence of the active segment's first record.
    pub fn active_first_sequence(&self) -> u64 {
        self.header.first_sequence
    }

    /// Encode one record as a line, refusing one longer than the spec allows.
    pub fn encode_record<R: Serialize>(&self, record: &R) -> Result<Vec<u8>, SegmentError> {
        let line = encode_line(&LineRef::Record(record))?;
        if line.len() > self.spec.record_bytes {
            return Err(SegmentError::RecordTooLarge);
        }
        Ok(line)
    }

    /// Append one encoded record line (from [`SegmentLog::encode_record`])
    /// and sync it. Only an `Ok` acknowledges the record. Any failure leaves
    /// the log refusing writes until it is reopened, because the line may or
    /// may not be durable.
    pub fn append_line(&mut self, line: &[u8]) -> Result<(), SegmentError> {
        if self.reader {
            return Err(SegmentError::Invalid("a reader cannot append"));
        }
        if self.recovery_required {
            return Err(SegmentError::RecoveryRequired);
        }
        if line.len() > self.spec.record_bytes || line.last() != Some(&b'\n') {
            return Err(SegmentError::RecordTooLarge);
        }
        if let Err(error) = self.dir.append(self.spec.active_name(), line, true) {
            self.recovery_required = true;
            return Err(error.into());
        }
        self.active_bytes += line.len();
        self.active_records += 1;
        Ok(())
    }

    /// Whether the active segment is full enough to seal.
    pub fn should_seal(&self) -> bool {
        self.active_records > 0
            && (self.active_bytes >= self.spec.segment_bytes
                || self.active_records >= self.spec.segment_records)
    }

    /// Seal the active segment and start the next one. The sealed file is
    /// published first; the new active segment then replaces the old one. A
    /// crash between the two is completed by the next open.
    pub fn seal(&mut self) -> Result<(), SegmentError> {
        if self.reader {
            return Err(SegmentError::Invalid("a reader cannot seal"));
        }
        if self.recovery_required {
            return Err(SegmentError::RecoveryRequired);
        }
        let result = self.seal_inner();
        if result.is_err() {
            self.recovery_required = true;
        }
        result
    }

    fn seal_inner(&mut self) -> Result<(), SegmentError> {
        let active_name = self.spec.active_name();
        let bytes = self
            .dir
            .read_limited(&active_name, self.spec.max_segment_file_bytes())?;
        if bytes.len() != self.active_bytes {
            return Err(SegmentError::Invalid("active segment changed while open"));
        }
        let digest = sha256(&bytes);
        let seal = SegmentSeal {
            records: self.active_records,
            digest: digest.clone(),
        };
        let mut sealed_bytes = bytes;
        sealed_bytes.extend(encode_line(&LineRef::<()>::Seal(&seal))?);
        let segments = match self.dir.existing_child(SEGMENTS_DIR) {
            Ok(segments) => segments,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let segments = self.dir.create_child(SEGMENTS_DIR)?;
                segments.sync_all()?;
                self.dir.sync_all()?;
                segments
            }
            Err(error) => return Err(error.into()),
        };
        segments.atomic_write(self.spec.sealed_name(self.header.index), &sealed_bytes)?;
        let header = SegmentHeader {
            schema: SCHEMA,
            kind: self.spec.kind.to_owned(),
            index: self.header.index + 1,
            previous: Some(digest.clone()),
            first_sequence: self.next_sequence(),
            meta: self.header.meta.clone(),
        };
        let line = encode_line(&LineRef::<()>::Header(&header))?;
        self.dir.atomic_write(&active_name, &line)?;
        self.sealed.push(SealedSegment {
            index: self.header.index,
            first_sequence: self.header.first_sequence,
            records: self.active_records,
            digest,
        });
        self.header = header;
        self.active_bytes = line.len();
        self.active_records = 0;
        Ok(())
    }

    /// Read one sealed segment's records, verifying its digest.
    pub fn read_sealed<R: DeserializeOwned>(
        &self,
        segment: &SealedSegment,
    ) -> Result<Vec<R>, SegmentError> {
        let bytes =
            read_segments_file(&self.dir, &self.spec.sealed_name(segment.index), &self.spec)?;
        let (body, seal) = split_seal(&bytes)?;
        if sha256(body) != segment.digest || seal.digest != segment.digest {
            return Err(SegmentError::Invalid(
                "sealed segment changed after opening",
            ));
        }
        let mut records = Vec::with_capacity(segment.records as usize);
        let header = parse_segment(body, &self.spec, |record| {
            records.push(record);
            Ok::<(), SegmentError>(())
        })?;
        if header.index != segment.index
            || header.first_sequence != segment.first_sequence
            || records.len() as u64 != segment.records
        {
            return Err(SegmentError::Invalid(
                "sealed segment changed after opening",
            ));
        }
        Ok(records)
    }

    /// The sealed segment holding `sequence`, if it is sealed.
    pub fn segment_of(&self, sequence: u64) -> Option<&SealedSegment> {
        let at = self
            .sealed
            .partition_point(|segment| segment.first_sequence + segment.records <= sequence);
        self.sealed
            .get(at)
            .filter(|segment| segment.first_sequence <= sequence)
    }
}

/// A second handle on the same directory for one read attempt.
fn reopen(dir: &SecureDir) -> io::Result<SecureDir> {
    dir.verify_ambient_identity()?;
    SecureDir::open(dir.path())
}

enum Retry<E> {
    Changed,
    Failed(E),
}

impl<E: From<SegmentError>> From<SegmentError> for Retry<E> {
    fn from(error: SegmentError) -> Self {
        Retry::Failed(error.into())
    }
}

fn open_inner<R, E>(
    dir: SecureDir,
    spec: SegmentSpec,
    meta: serde_json::Value,
    mode: Mode,
    mut visit: impl FnMut(u64, u64, R) -> Result<(), E>,
) -> Result<SegmentLog, Retry<E>>
where
    R: DeserializeOwned,
    E: From<SegmentError>,
{
    let failed = |error: SegmentError| Retry::Failed(E::from(error));
    let mut previous = None;
    let mut next_sequence = 1;
    let mut sealed = Vec::new();
    for index in sealed_indexes(&dir, &spec).map_err(failed)? {
        let bytes = match read_segments_file(&dir, &spec.sealed_name(index), &spec) {
            Err(SegmentError::Io(error))
                if mode == Mode::Reader && error.kind() == io::ErrorKind::NotFound =>
            {
                return Err(Retry::Changed);
            }
            other => other.map_err(failed)?,
        };
        let (body, seal) = split_seal(&bytes).map_err(failed)?;
        let digest = sha256(body);
        if seal.digest != digest {
            return Err(failed(SegmentError::Invalid(
                "sealed segment digest mismatch",
            )));
        }
        let mut records = 0;
        let header = parse_segment(body, &spec, |record: R| {
            visit(index, next_sequence + records, record).map_err(Retry::Failed)?;
            records += 1;
            Ok::<(), Retry<E>>(())
        })?;
        check_header(&header, &spec, index, &previous, next_sequence, &meta).map_err(failed)?;
        if records != seal.records {
            return Err(failed(SegmentError::Invalid(
                "sealed segment record count mismatch",
            )));
        }
        sealed.push(SealedSegment {
            index,
            first_sequence: next_sequence,
            records,
            digest: digest.clone(),
        });
        next_sequence += records;
        previous = Some(digest);
    }
    let next_index = sealed.len() as u64;
    let next_header = |previous: Option<String>| SegmentHeader {
        schema: SCHEMA,
        kind: spec.kind.to_owned(),
        index: next_index,
        previous,
        first_sequence: next_sequence,
        meta: meta.clone(),
    };
    let mut recovery = SegmentRecovery::default();
    let active_name = spec.active_name();
    let reader = mode == Mode::Reader;
    let mut bytes = match dir.read_limited(&active_name, spec.max_segment_file_bytes()) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if !sealed.is_empty() || mode != (Mode::Writer { create: true }) {
                return Err(failed(SegmentError::Invalid("active segment is missing")));
            }
            let header = next_header(previous);
            let line = encode_line(&LineRef::<()>::Header(&header)).map_err(failed)?;
            dir.atomic_write(&active_name, &line)
                .map_err(|error| failed(error.into()))?;
            return Ok(SegmentLog {
                dir,
                spec,
                header,
                sealed,
                active_bytes: line.len(),
                active_records: 0,
                recovery,
                recovery_required: false,
                reader,
            });
        }
        Err(error) => return Err(failed(error.into())),
    };
    // An unfinished last line was never acknowledged.
    let complete = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |at| at + 1);
    if complete < bytes.len() {
        recovery.torn_bytes = (bytes.len() - complete) as u64;
        if !reader {
            let file = dir
                .open_append(&active_name)
                .map_err(|error| failed(error.into()))?;
            file.set_len(complete as u64)
                .map_err(|error| failed(error.into()))?;
            file.sync_all().map_err(|error| failed(error.into()))?;
        }
        bytes.truncate(complete);
    }
    if !reader {
        // An append whose sync failed may be visible without being durable.
        // Make what this open observes durable before anything is
        // acknowledged on top of it.
        let file = dir
            .open_append(&active_name)
            .map_err(|error| failed(error.into()))?;
        file.sync_all().map_err(|error| failed(error.into()))?;
        dir.sync_all().map_err(|error| failed(error.into()))?;
    }
    let active_header = peek_header(&bytes).map_err(failed)?;
    if let Some(last) = sealed
        .last()
        .filter(|_| active_header.index + 1 == next_index)
    {
        // Sealed but not yet replaced: the seal holds exactly these bytes.
        let sealed_bytes =
            read_segments_file(&dir, &spec.sealed_name(last.index), &spec).map_err(failed)?;
        if split_seal(&sealed_bytes).map_err(failed)?.0 != bytes.as_slice() {
            return Err(failed(SegmentError::Invalid(
                "active segment differs from its interrupted seal",
            )));
        }
        let header = next_header(previous);
        let line = encode_line(&LineRef::<()>::Header(&header)).map_err(failed)?;
        if !reader {
            dir.atomic_write(&active_name, &line)
                .map_err(|error| failed(error.into()))?;
            recovery.completed_seal = true;
        }
        return Ok(SegmentLog {
            dir,
            spec,
            header,
            sealed,
            active_bytes: line.len(),
            active_records: 0,
            recovery,
            recovery_required: false,
            reader,
        });
    }
    if reader && active_header.index > next_index {
        // Sealed after the listing above; start over.
        return Err(Retry::Changed);
    }
    let mut active_records = 0;
    let header = parse_segment(&bytes, &spec, |record: R| {
        visit(next_index, next_sequence + active_records, record).map_err(Retry::Failed)?;
        active_records += 1;
        Ok::<(), Retry<E>>(())
    })?;
    check_header(&header, &spec, next_index, &previous, next_sequence, &meta).map_err(failed)?;
    Ok(SegmentLog {
        dir,
        spec,
        header,
        sealed,
        active_bytes: bytes.len(),
        active_records,
        recovery,
        recovery_required: false,
        reader,
    })
}

fn sealed_indexes(dir: &SecureDir, spec: &SegmentSpec) -> Result<Vec<u64>, SegmentError> {
    let segments = match dir.existing_child(SEGMENTS_DIR) {
        Ok(segments) => segments,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(vec![]),
        Err(error) => return Err(error.into()),
    };
    let mut indexes = Vec::new();
    for entry in segments.entries()? {
        let name = entry.name.to_string_lossy();
        if name.starts_with(".axocoatl-") && name.ends_with(".tmp") {
            // An atomic write that never published; it holds nothing acknowledged.
            continue;
        }
        match (spec.sealed_index(&name), entry.file_type) {
            (Some(index), axocoatl_core::SecureEntryType::File) => indexes.push(index),
            _ => {
                return Err(SegmentError::Invalid(
                    "unexpected entry among sealed segments",
                ))
            }
        }
    }
    indexes.sort_unstable();
    if indexes
        .iter()
        .enumerate()
        .any(|(at, index)| *index != at as u64)
    {
        return Err(SegmentError::Invalid("sealed segments are not contiguous"));
    }
    Ok(indexes)
}

fn read_segments_file(
    dir: &SecureDir,
    name: &str,
    spec: &SegmentSpec,
) -> Result<Vec<u8>, SegmentError> {
    Ok(dir
        .existing_child(SEGMENTS_DIR)?
        .read_limited(name, spec.max_segment_file_bytes())?)
}

/// The bytes before a sealed segment's seal line, and the seal.
fn split_seal(bytes: &[u8]) -> Result<(&[u8], SegmentSeal), SegmentError> {
    let without_newline = bytes
        .strip_suffix(b"\n")
        .ok_or(SegmentError::Invalid("sealed segment is unterminated"))?;
    let start = without_newline
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |at| at + 1);
    match serde_json::from_slice::<Line<serde::de::IgnoredAny>>(&without_newline[start..])? {
        Line::Seal(seal) => Ok((&bytes[..start], seal)),
        _ => Err(SegmentError::Invalid("sealed segment has no seal")),
    }
}

fn peek_header(bytes: &[u8]) -> Result<SegmentHeader, SegmentError> {
    let end = bytes
        .iter()
        .position(|byte| *byte == b'\n')
        .ok_or(SegmentError::Invalid("segment has no header"))?;
    match serde_json::from_slice::<Line<serde::de::IgnoredAny>>(&bytes[..end])? {
        Line::Header(header) => Ok(header),
        _ => Err(SegmentError::Invalid(
            "segment does not start with its header",
        )),
    }
}

/// Parse a segment's header and records (no seal), handing each record to
/// `visit`.
fn parse_segment<R, E>(
    bytes: &[u8],
    spec: &SegmentSpec,
    mut visit: impl FnMut(R) -> Result<(), E>,
) -> Result<SegmentHeader, E>
where
    R: DeserializeOwned,
    E: From<SegmentError>,
{
    let mut lines = bytes.split_inclusive(|byte| *byte == b'\n');
    let first = lines
        .next()
        .ok_or(SegmentError::Invalid("segment has no header"))?;
    let header = match serde_json::from_slice::<Line<serde::de::IgnoredAny>>(first)
        .map_err(SegmentError::from)?
    {
        Line::Header(header) => header,
        _ => return Err(SegmentError::Invalid("segment does not start with its header").into()),
    };
    for line in lines {
        if line.len() > spec.record_bytes || line.last() != Some(&b'\n') {
            return Err(SegmentError::Invalid("segment record line is malformed").into());
        }
        match serde_json::from_slice::<Line<R>>(line).map_err(SegmentError::from)? {
            Line::Record(record) => visit(record)?,
            _ => return Err(SegmentError::Invalid("segment frame out of place").into()),
        }
    }
    Ok(header)
}

fn check_header(
    header: &SegmentHeader,
    spec: &SegmentSpec,
    index: u64,
    previous: &Option<String>,
    first_sequence: u64,
    meta: &serde_json::Value,
) -> Result<(), SegmentError> {
    if header.schema != SCHEMA || header.kind != spec.kind || &header.meta != meta {
        return Err(SegmentError::Invalid(
            "segment belongs to another store or schema",
        ));
    }
    if header.index != index
        || &header.previous != previous
        || header.first_sequence != first_sequence
    {
        return Err(SegmentError::Invalid("segment chain is broken"));
    }
    Ok(())
}

fn encode_line<R: Serialize>(line: &LineRef<'_, R>) -> Result<Vec<u8>, SegmentError> {
    let mut bytes = serde_json::to_vec(line)?;
    if bytes.contains(&b'\n') {
        return Err(SegmentError::Invalid("encoded line contains a newline"));
    }
    bytes.write_all(b"\n")?;
    Ok(bytes)
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// A Bloom filter over string keys, built for one sealed segment and kept in
/// memory: a "no" is certain, so a lookup reads only the segments that may
/// hold its key, and confirms the key in the records it reads.
#[derive(Debug, Clone)]
pub struct KeyFilter {
    bits: Vec<u64>,
}

/// Bits per key (about a 1% false-positive rate with [`FILTER_HASHES`]).
const FILTER_BITS_PER_KEY: usize = 10;
const FILTER_HASHES: u64 = 7;

impl KeyFilter {
    pub fn new<'a>(keys: impl IntoIterator<Item = &'a str> + Clone) -> Self {
        let count = keys.clone().into_iter().count().max(1);
        let words = (count * FILTER_BITS_PER_KEY).div_ceil(64).max(1);
        let mut filter = Self {
            bits: vec![0; words],
        };
        for key in keys {
            filter.insert(key);
        }
        filter
    }

    fn positions(&self, key: &str) -> impl Iterator<Item = usize> + '_ {
        let (first, second) = key_hashes(key);
        let len = (self.bits.len() * 64) as u64;
        (0..FILTER_HASHES)
            .map(move |round| (first.wrapping_add(round.wrapping_mul(second)) % len) as usize)
    }

    fn insert(&mut self, key: &str) {
        let positions: Vec<usize> = self.positions(key).collect();
        for at in positions {
            self.bits[at / 64] |= 1 << (at % 64);
        }
    }

    pub fn may_contain(&self, key: &str) -> bool {
        self.positions(key)
            .all(|at| self.bits[at / 64] & (1 << (at % 64)) != 0)
    }

    /// Bytes of memory this filter holds.
    pub fn bytes(&self) -> usize {
        self.bits.len() * 8
    }
}

fn key_hashes(key: &str) -> (u64, u64) {
    let mut first = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut first);
    let mut second = std::collections::hash_map::DefaultHasher::new();
    (key, 0x9e37_79b9_7f4a_7c15_u64).hash(&mut second);
    (first.finish(), second.finish() | 1)
}

/// One sealed segment's decoded records, shared.
pub type SegmentRecords<R> = Arc<Vec<Arc<R>>>;

/// A few recently read sealed segments, decoded, so repeated lookups into
/// the same old segment do not read it again. Its size is fixed.
#[derive(Debug)]
pub struct SegmentCache<R> {
    capacity: usize,
    segments: Mutex<VecDeque<(u64, SegmentRecords<R>)>>,
}

impl<R: DeserializeOwned> SegmentCache<R> {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            segments: Mutex::new(VecDeque::new()),
        }
    }

    /// The records of `segment`, from the cache or read from disk.
    pub fn get(
        &self,
        log: &SegmentLog,
        segment: &SealedSegment,
    ) -> Result<SegmentRecords<R>, SegmentError> {
        {
            let mut segments = self
                .segments
                .lock()
                .map_err(|_| SegmentError::Invalid("segment cache lock poisoned"))?;
            if let Some(at) = segments
                .iter()
                .position(|(index, _)| *index == segment.index)
            {
                let entry = segments.remove(at).unwrap();
                let records = entry.1.clone();
                segments.push_front(entry);
                return Ok(records);
            }
        }
        let records: SegmentRecords<R> = Arc::new(
            log.read_sealed::<R>(segment)?
                .into_iter()
                .map(Arc::new)
                .collect(),
        );
        let mut segments = self
            .segments
            .lock()
            .map_err(|_| SegmentError::Invalid("segment cache lock poisoned"))?;
        segments.push_front((segment.index, records.clone()));
        segments.truncate(self.capacity);
        Ok(records)
    }

    pub fn clear(&self) {
        if let Ok(mut segments) = self.segments.lock() {
            segments.clear();
        }
    }
}

#[cfg(test)]
#[path = "segment_log_tests.rs"]
mod tests;
