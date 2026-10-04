//! How a Session's network record is kept on disk once it can grow for the
//! Session's whole life.
//!
//! - `network-record.v1.jsonl`, the record's primary file, holds only the
//!   head: [`head`], one marker line written twice. Axocoatl 1.2.0 kept the
//!   whole record in this file; a writer that opens a record in that form
//!   migrates it first (see [`migrate`]).
//! - `network-record.active.jsonl` is the active segment: a header line, then
//!   one network line per event, appended and synced as before.
//! - `segments/network-record.<index>.jsonl` are sealed segments: the exact
//!   bytes the active segment had, then one seal line with the number of
//!   events and the SHA-256 of every byte before it. Sealed files are written
//!   once and never changed.
//!
//! Every header names the digest of the segment before it (`previous`), so a
//! sealed segment cannot be changed, removed or reordered without the chain
//! breaking; a writer verifies every seal and the whole chain when it opens.
//! Each header also carries the totals of the segments before it, so a reader
//! learns the record's counts from the active segment alone.

use std::io::{self, Read};

use axocoatl_core::{SecureDir, SecureEntryType};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{NetworkLine, NetworkRecordError, MAX_LINE_BYTES, NETWORK_RECORD_VERSION};
use crate::segment_log::SegmentsMarker;

/// The active segment, beside the primary file.
pub(super) const ACTIVE_FILE: &str = "network-record.active.jsonl";
/// Directory, beside the primary file, that holds the sealed segments.
pub(super) const SEGMENTS_DIR: &str = "segments";
/// Bound into the head and every header.
const KIND: &str = "network-record";
const SCHEMA: u32 = 1;
/// Longest header or seal line.
const MAX_FRAME_BYTES: usize = 64 * 1024;
/// Default size at which the active segment is sealed.
pub const DEFAULT_SEGMENT_BYTES: usize = 4 * 1024 * 1024;
/// Default number of events at which the active segment is sealed.
pub const DEFAULT_SEGMENT_EVENTS: u64 = 16_384;
/// The most bytes any segment file can hold: the default size, one more line
/// (the active segment is sealed before the append that would pass it) and
/// its header and seal.
pub(super) const SEGMENT_FILE_CEILING: usize =
    DEFAULT_SEGMENT_BYTES + MAX_LINE_BYTES + 2 * MAX_FRAME_BYTES;
/// The largest record Axocoatl 1.2.0 could leave in its single file: its
/// 32 MiB cap, the 64 lines of control headroom past it, and one line.
pub(super) const LEGACY_FILE_CEILING: usize =
    32 * 1024 * 1024 + 64 * MAX_LINE_BYTES + MAX_LINE_BYTES;

/// When the active segment is sealed. These bound each segment, never the
/// record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentLimits {
    /// Seal before an append once the active segment's events hold this many
    /// bytes (at most [`DEFAULT_SEGMENT_BYTES`])...
    pub bytes: usize,
    /// ...or this many events.
    pub events: u64,
}

impl Default for SegmentLimits {
    fn default() -> Self {
        Self {
            bytes: DEFAULT_SEGMENT_BYTES,
            events: DEFAULT_SEGMENT_EVENTS,
        }
    }
}

impl SegmentLimits {
    pub(super) fn validate(self) -> Result<Self, NetworkRecordError> {
        if self.bytes == 0 || self.bytes > DEFAULT_SEGMENT_BYTES || self.events == 0 {
            return Err(NetworkRecordError::InvalidEvent(
                "segment limits must be positive and at most the default size",
            ));
        }
        Ok(self)
    }

    /// Whether an active segment holding `events` events in `bytes` bytes is
    /// sealed before the next append.
    pub(super) fn full(&self, events: u64, bytes: u64) -> bool {
        events > 0 && (events >= self.events || bytes >= self.bytes as u64)
    }
}

pub(super) fn damaged(reason: impl Into<String>) -> NetworkRecordError {
    NetworkRecordError::Damaged {
        line: 0,
        reason: reason.into(),
    }
}

/// Counts over some run of events.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Totals {
    pub events: u64,
    /// Bytes of the events' lines, newlines included.
    pub bytes: u64,
    /// Places where `seq` skipped a value.
    pub gaps: u64,
    /// The highest egress sidecar generation the events name.
    pub max_generation: u32,
}

/// Totals and the last sequence number, counted line by line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Tally {
    pub totals: Totals,
    pub last_seq: u64,
}

impl Tally {
    /// Count one line. Sequence numbers must increase; a skipped value is a
    /// gap, except before the record's first line.
    pub fn count(
        &mut self,
        seq: u64,
        bytes: usize,
        generation: Option<u32>,
    ) -> Result<(), &'static str> {
        if seq <= self.last_seq {
            return Err("non-increasing sequence");
        }
        if self.last_seq != 0 && seq != self.last_seq + 1 {
            self.totals.gaps += 1;
        }
        self.last_seq = seq;
        self.totals.events += 1;
        self.totals.bytes += bytes as u64;
        if let Some(generation) = generation {
            self.totals.max_generation = self.totals.max_generation.max(generation);
        }
        Ok(())
    }
}

/// First line of every segment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Header {
    pub schema: u32,
    pub kind: String,
    pub index: u64,
    /// The digest of the segment before this one.
    pub previous: Option<String>,
    /// One more than the last sequence number before this segment; its
    /// events have this sequence number or a higher one.
    pub first_seq: u64,
    /// Totals of every segment before this one.
    pub before: Totals,
    /// The Session journal the record belongs to.
    pub meta: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Seal {
    records: u64,
    digest: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
enum Frame {
    #[serde(rename = "header")]
    Header(Header),
    #[serde(rename = "seal")]
    Seal(Seal),
}

/// Where the next segment starts: its index, the digest it names and the
/// totals of everything before it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Chain {
    pub index: u64,
    pub previous: Option<String>,
    pub tally: Tally,
}

impl Chain {
    pub fn header(&self, meta: &serde_json::Value) -> Header {
        Header {
            schema: SCHEMA,
            kind: KIND.into(),
            index: self.index,
            previous: self.previous.clone(),
            first_seq: self.tally.last_seq + 1,
            before: self.tally.totals,
            meta: meta.clone(),
        }
    }

    /// Check `header` is the next one, and take its generation total, which
    /// only grows: a writer counts generations as events are appended, and
    /// opening reads sealed segments without parsing every event.
    pub fn check(
        &mut self,
        header: &Header,
        meta: &serde_json::Value,
    ) -> Result<(), NetworkRecordError> {
        if header.schema != SCHEMA || header.kind != KIND || &header.meta != meta {
            return Err(damaged("a segment belongs to another record or schema"));
        }
        let expected = self.header(meta);
        if header.index != expected.index
            || header.previous != expected.previous
            || header.first_seq != expected.first_seq
        {
            return Err(damaged(format!(
                "the segment chain is broken at segment {}",
                expected.index
            )));
        }
        let (before, counted) = (header.before, expected.before);
        if before.events != counted.events
            || before.bytes != counted.bytes
            || before.gaps != counted.gaps
            || before.max_generation < counted.max_generation
        {
            return Err(damaged(format!(
                "segment {} disagrees with the totals before it",
                expected.index
            )));
        }
        self.tally.totals.max_generation = before.max_generation;
        Ok(())
    }

    /// Move past a sealed segment with this digest.
    pub fn advance(&mut self, digest: String) {
        self.index += 1;
        self.previous = Some(digest);
    }
}

/// What a writer keeps about one sealed segment: enough to find and verify
/// it again, never its events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Sealed {
    pub index: u64,
    pub last_seq: u64,
    pub digest: String,
}

pub(super) fn sealed_name(index: u64) -> String {
    format!("{KIND}.{index:010}.jsonl")
}

fn sealed_index(name: &str) -> Option<u64> {
    let rest = name
        .strip_prefix(KIND)?
        .strip_prefix('.')?
        .strip_suffix(".jsonl")?;
    (rest.len() == 10 && rest.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| rest.parse().ok())
        .flatten()
}

pub(super) fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn frame_line(frame: &Frame) -> Result<Vec<u8>, NetworkRecordError> {
    let mut line = serde_json::to_vec(frame).map_err(io::Error::other)?;
    if line.len() >= MAX_FRAME_BYTES {
        return Err(damaged("a segment header or seal is too long"));
    }
    line.push(b'\n');
    Ok(line)
}

pub(super) fn header_line(header: &Header) -> Result<Vec<u8>, NetworkRecordError> {
    frame_line(&Frame::Header(header.clone()))
}

/// A sealed segment's file: its bytes and the seal.
pub(super) fn sealed_file(
    body: &[u8],
    records: u64,
) -> Result<(Vec<u8>, String), NetworkRecordError> {
    let digest = sha256(body);
    let mut bytes = body.to_vec();
    bytes.extend(frame_line(&Frame::Seal(Seal {
        records,
        digest: digest.clone(),
    }))?);
    Ok((bytes, digest))
}

/// The head line: the segments marker, and text that Axocoatl 1.2.0's
/// search for `web` events matches.
#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct HeadLine {
    segments: SegmentsMarker,
    older: OlderReaders,
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct OlderReaders {
    kind: String,
}

/// What the primary file holds once the record is segmented: the head line,
/// twice. Axocoatl 1.2.0 reads the primary file as the whole record. Its
/// first line is not a network line and is not the last line, so 1.2.0
/// reports the record damaged instead of reading part of it, and cannot cut
/// the line away as an interrupted append. Its search for `web` events finds
/// `"kind":"web"` in that first line and reports the same damage.
pub(super) fn head() -> Vec<u8> {
    let line = serde_json::to_vec(&HeadLine {
        segments: SegmentsMarker {
            schema: SCHEMA,
            kind: KIND.into(),
        },
        older: OlderReaders { kind: "web".into() },
    })
    .unwrap_or_default();
    let mut bytes = Vec::with_capacity(2 * line.len() + 2);
    for _ in 0..2 {
        bytes.extend_from_slice(&line);
        bytes.push(b'\n');
    }
    bytes
}

/// Whether the primary file is the head of a segmented record (`true`) or a
/// record in Axocoatl 1.2.0's single file (`false`).
pub(super) fn is_head(primary: &[u8]) -> Result<bool, NetworkRecordError> {
    if !primary.starts_with(b"{\"segments\"") {
        return Ok(false);
    }
    if primary == head().as_slice() {
        Ok(true)
    } else {
        Err(damaged(
            "the record's head is not one this version of Axocoatl reads",
        ))
    }
}

/// The sequence number at the start of a line this writer wrote, without
/// parsing the rest of it.
pub(super) fn line_seq(line: &[u8]) -> Option<u64> {
    let rest = line.strip_prefix(b"{\"v\":1,\"seq\":")?;
    let digits = rest.iter().take_while(|byte| byte.is_ascii_digit()).count();
    if digits == 0 || digits > 19 || rest.get(digits) != Some(&b',') {
        return None;
    }
    std::str::from_utf8(&rest[..digits]).ok()?.parse().ok()
}

/// One line of any version, or `None` when it does not parse.
pub(super) fn parse_any(line: &[u8]) -> Option<NetworkLine> {
    (line.len() < MAX_LINE_BYTES)
        .then(|| serde_json::from_slice::<NetworkLine>(line).ok())
        .flatten()
}

/// One line of this version.
pub(super) fn parse_line(line: &[u8]) -> Option<NetworkLine> {
    parse_any(line).filter(|parsed| parsed.v == NETWORK_RECORD_VERSION)
}

/// Complete lines of `region`, each with its start offset, newline excluded.
pub(super) fn lines(region: &[u8]) -> impl Iterator<Item = (usize, &[u8])> {
    let mut start = 0;
    std::iter::from_fn(move || {
        let length = memchr::memchr(b'\n', &region[start..])?;
        let at = start;
        start += length + 1;
        Some((at, &region[at..at + length]))
    })
}

/// The header at the start of a segment and the length of its line.
pub(super) fn parse_header(bytes: &[u8]) -> Result<(Header, usize), NetworkRecordError> {
    let end = memchr::memchr(b'\n', &bytes[..bytes.len().min(MAX_FRAME_BYTES)])
        .ok_or_else(|| damaged("a segment has no header"))?;
    match serde_json::from_slice::<Frame>(&bytes[..end]) {
        Ok(Frame::Header(header)) => Ok((header, end + 1)),
        _ => Err(damaged("a segment does not start with its header")),
    }
}

/// A sealed segment read back and checked against its own seal.
pub(super) struct SealedRead {
    pub bytes: Vec<u8>,
    pub header: Header,
    /// Where the events start and end in `bytes`.
    pub events: std::ops::Range<usize>,
    pub records: u64,
    pub digest: String,
}

impl SealedRead {
    pub fn region(&self) -> &[u8] {
        &self.bytes[self.events.clone()]
    }
}

/// Read sealed segment `index` and check its digest, and that it is the
/// segment it says.
pub(super) fn read_sealed(
    segments: &SecureDir,
    index: u64,
) -> Result<SealedRead, NetworkRecordError> {
    let bytes = segments.read_limited(sealed_name(index), SEGMENT_FILE_CEILING)?;
    let without_newline = bytes
        .strip_suffix(b"\n")
        .ok_or_else(|| damaged(format!("sealed segment {index} is unterminated")))?;
    let seal_start = memchr::memrchr(b'\n', without_newline).map_or(0, |at| at + 1);
    let seal = match serde_json::from_slice::<Frame>(&without_newline[seal_start..]) {
        Ok(Frame::Seal(seal)) => seal,
        _ => return Err(damaged(format!("sealed segment {index} has no seal"))),
    };
    let digest = sha256(&bytes[..seal_start]);
    if seal.digest != digest {
        return Err(damaged(format!(
            "sealed segment {index} does not match its digest"
        )));
    }
    let (header, header_len) = parse_header(&bytes[..seal_start])?;
    if header.index != index {
        return Err(damaged(format!(
            "sealed segment {index} names another index"
        )));
    }
    Ok(SealedRead {
        bytes,
        header,
        events: header_len..seal_start,
        records: seal.records,
        digest,
    })
}

/// Only the header of sealed segment `index`, for finding a sequence number
/// without reading whole segments.
pub(super) fn read_sealed_header(
    segments: &SecureDir,
    index: u64,
) -> Result<Header, NetworkRecordError> {
    let file = segments.open_file_limited(sealed_name(index), SEGMENT_FILE_CEILING)?;
    let mut start = Vec::with_capacity(4096);
    file.take(MAX_FRAME_BYTES as u64).read_to_end(&mut start)?;
    let (header, _) = parse_header(&start)?;
    if header.index != index {
        return Err(damaged(format!(
            "sealed segment {index} names another index"
        )));
    }
    Ok(header)
}

/// The sealed segments' indexes, which must run from 0 without a gap. An
/// atomic write that never published leaves a temporary file, which holds
/// nothing and is skipped.
pub(super) fn sealed_count(segments: Option<&SecureDir>) -> Result<u64, NetworkRecordError> {
    let Some(segments) = segments else {
        return Ok(0);
    };
    let mut indexes = Vec::new();
    for entry in segments.entries()? {
        let name = entry.name.to_string_lossy();
        if name.starts_with(".axocoatl-") && name.ends_with(".tmp") {
            continue;
        }
        match (sealed_index(&name), entry.file_type) {
            (Some(index), SecureEntryType::File) => indexes.push(index),
            _ => return Err(damaged("an unexpected entry is among the sealed segments")),
        }
    }
    indexes.sort_unstable();
    if indexes
        .iter()
        .enumerate()
        .any(|(at, index)| *index != at as u64)
    {
        return Err(damaged("the sealed segments are not contiguous"));
    }
    Ok(indexes.len() as u64)
}

/// The segments directory, if any segment was ever sealed.
pub(super) fn segments_dir(root: &SecureDir) -> Result<Option<SecureDir>, NetworkRecordError> {
    match root.existing_child(SEGMENTS_DIR) {
        Ok(segments) => Ok(Some(segments)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Count a sealed segment's events against `chain`, which it must follow,
/// without parsing each event: the digest says the bytes are the ones a
/// writer validated and sealed. Returns what a writer keeps about it.
pub(super) fn verify_sealed(
    sealed: &SealedRead,
    chain: &mut Chain,
    meta: &serde_json::Value,
) -> Result<Sealed, NetworkRecordError> {
    chain.check(&sealed.header, meta)?;
    let index = sealed.header.index;
    let mut records = 0;
    for (_, line) in lines(sealed.region()) {
        let (seq, generation) = match line_seq(line) {
            Some(seq) => (seq, None),
            None => {
                let parsed = parse_line(line).ok_or_else(|| {
                    damaged(format!("sealed segment {index} holds a damaged line"))
                })?;
                (parsed.seq, parsed.event.generation())
            }
        };
        chain
            .tally
            .count(seq, line.len() + 1, generation)
            .map_err(|reason| damaged(format!("sealed segment {index}: {reason}")))?;
        records += 1;
    }
    if records != sealed.records || records == 0 {
        return Err(damaged(format!(
            "sealed segment {index} does not hold the events its seal counts"
        )));
    }
    chain.advance(sealed.digest.clone());
    Ok(Sealed {
        index,
        last_seq: chain.tally.last_seq,
        digest: sealed.digest.clone(),
    })
}

/// Count every event of a sealed segment by parsing it, for a reader that
/// must learn a segment's totals itself.
pub(super) fn tally_sealed(sealed: &SealedRead) -> Result<Tally, NetworkRecordError> {
    let mut tally = Tally {
        totals: sealed.header.before,
        last_seq: sealed.header.first_seq - 1,
    };
    for (_, line) in lines(sealed.region()) {
        let parsed = parse_line(line).ok_or_else(|| {
            damaged(format!(
                "sealed segment {} holds a damaged line",
                sealed.header.index
            ))
        })?;
        tally
            .count(parsed.seq, line.len() + 1, parsed.event.generation())
            .map_err(damaged)?;
    }
    Ok(tally)
}

/// The active segment as read: its header, its events and anything after the
/// last complete event.
#[derive(Debug)]
pub(super) struct ActiveRead {
    pub header: Header,
    pub header_len: usize,
    /// `(seq, offset)` of each event, offsets from the start of the file.
    pub index: Vec<(u64, u32)>,
    /// The record's totals through the last event.
    pub tally: Tally,
    /// Bytes through the last complete event.
    pub good_len: usize,
    /// Bytes after it: an interrupted append.
    pub torn_bytes: usize,
}

/// Parse the active segment. A last line that is unfinished or does not
/// parse is an interrupted append; any other damage refuses the read.
pub(super) fn read_active(bytes: &[u8]) -> Result<ActiveRead, NetworkRecordError> {
    let (header, header_len) = parse_header(bytes)?;
    let mut tally = Tally {
        totals: header.before,
        last_seq: header.first_seq.saturating_sub(1),
    };
    let mut index = Vec::new();
    let mut position = header_len;
    let mut line_number = 1u64;
    while position < bytes.len() {
        line_number += 1;
        let Some(length) = memchr::memchr(b'\n', &bytes[position..]) else {
            break;
        };
        let end = position + length;
        let Some(line) = parse_any(&bytes[position..end]) else {
            if end + 1 == bytes.len() {
                break;
            }
            return Err(NetworkRecordError::Damaged {
                line: line_number,
                reason: "unparseable line before the end of the active segment".into(),
            });
        };
        if line.v != NETWORK_RECORD_VERSION {
            return Err(NetworkRecordError::Damaged {
                line: line_number,
                reason: "unknown version".into(),
            });
        }
        tally
            .count(line.seq, length + 1, line.event.generation())
            .map_err(|reason| NetworkRecordError::Damaged {
                line: line_number,
                reason: reason.into(),
            })?;
        index.push((line.seq, position as u32));
        position = end + 1;
    }
    Ok(ActiveRead {
        header,
        header_len,
        index,
        tally,
        good_len: position,
        torn_bytes: bytes.len() - position,
    })
}

/// Lines of `region` with `seq > after`, at most `limit`.
pub(super) fn lines_after(
    region: &[u8],
    after: u64,
    limit: usize,
    out: &mut Vec<NetworkLine>,
) -> Result<(), NetworkRecordError> {
    for (_, line) in lines(region) {
        if out.len() >= limit {
            break;
        }
        if line_seq(line).is_some_and(|seq| seq <= after) {
            continue;
        }
        let parsed = parse_line(line).ok_or_else(|| damaged("a recorded line does not parse"))?;
        if parsed.seq > after {
            out.push(parsed);
        }
    }
    Ok(())
}

/// Lines of `region` with `seq > after` whose event is one of `kinds`, at
/// most `max`. Only lines holding `"kind":"<kind>"`, which the writer's
/// compact JSON puts in every event of that kind, are parsed. Returns the
/// sequence number of the last line kept when `max` stopped the search.
pub(super) fn kind_lines_after(
    region: &[u8],
    kinds: &[&str],
    after: u64,
    max: usize,
    out: &mut Vec<NetworkLine>,
) -> Result<Option<u64>, NetworkRecordError> {
    let mut starts = Vec::new();
    for kind in kinds {
        let needle = format!("\"kind\":\"{kind}\"");
        for found in memchr::memmem::find_iter(region, needle.as_bytes()) {
            starts.push(memchr::memrchr(b'\n', &region[..found]).map_or(0, |at| at + 1));
        }
    }
    starts.sort_unstable();
    starts.dedup();
    for start in starts {
        let end = start
            + memchr::memchr(b'\n', &region[start..])
                .ok_or_else(|| damaged("a recorded line is unterminated"))?;
        let line = &region[start..end];
        if line_seq(line).is_some_and(|seq| seq <= after) {
            continue;
        }
        let parsed = parse_line(line).ok_or_else(|| damaged("a recorded line does not parse"))?;
        // The text can also match a nested `kind` field of another event.
        if parsed.seq <= after || !kinds.contains(&parsed.event.kind()) {
            continue;
        }
        let seq = parsed.seq;
        out.push(parsed);
        if out.len() >= max {
            return Ok(Some(seq));
        }
    }
    Ok(None)
}

/// Move a record from Axocoatl 1.2.0's single file into segments. `legacy`
/// is that file's complete lines, already validated. Leftovers of an
/// interrupted migration are removed first: until the head replaces the
/// primary file, the single file is the record. The primary file is replaced
/// last, so a crash at any point leaves either the single file or the whole
/// segmented record.
pub(super) fn migrate(
    namespace: &crate::execution_namespace::OwnedExecutionNamespace,
    primary: &str,
    legacy: &[u8],
    meta: &serde_json::Value,
    limits: SegmentLimits,
) -> Result<(), NetworkRecordError> {
    let root = namespace.secure_dir()?;
    if root.has_exact_directory(SEGMENTS_DIR)? {
        root.remove_dir_all(SEGMENTS_DIR)?;
    }
    if root.has_exact_file(ACTIVE_FILE)? {
        root.remove_file(ACTIVE_FILE)?;
    }
    root.sync_all()?;
    let mut chain = Chain::default();
    let mut body = header_line(&chain.header(meta))?;
    let mut events = 0u64;
    let mut event_bytes = 0u64;
    let mut segments = None;
    for (_, line) in lines(legacy) {
        if limits.full(events, event_bytes) {
            seal_chunk(namespace, &mut segments, &mut chain, &body, events)?;
            body = header_line(&chain.header(meta))?;
            events = 0;
            event_bytes = 0;
        }
        let parsed = parse_line(line).ok_or_else(|| damaged("a migrated line does not parse"))?;
        chain
            .tally
            .count(parsed.seq, line.len() + 1, parsed.event.generation())
            .map_err(damaged)?;
        body.extend_from_slice(line);
        body.push(b'\n');
        events += 1;
        event_bytes += line.len() as u64 + 1;
    }
    if events > 0 {
        seal_chunk(namespace, &mut segments, &mut chain, &body, events)?;
    }
    namespace.atomic_write(ACTIVE_FILE, &header_line(&chain.header(meta))?)?;
    namespace.atomic_write(primary, &head())?;
    Ok(())
}

fn seal_chunk(
    namespace: &crate::execution_namespace::OwnedExecutionNamespace,
    segments: &mut Option<crate::execution_namespace::OwnedExecutionNamespace>,
    chain: &mut Chain,
    body: &[u8],
    records: u64,
) -> Result<(), NetworkRecordError> {
    if segments.is_none() {
        *segments = Some(namespace.child(SEGMENTS_DIR)?);
    }
    let (bytes, digest) = sealed_file(body, records)?;
    if let Some(segments) = segments {
        segments.atomic_write(sealed_name(chain.index), &bytes)?;
    }
    chain.advance(digest);
    Ok(())
}
