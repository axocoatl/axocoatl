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

use std::io::{BufRead, Read};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

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

/// Longest line `verify_bundle` reads; the Session export is the largest
/// section.
pub const MAX_BUNDLE_LINE_BYTES: usize = 256 * 1024 * 1024;
/// Section name of the last line.
pub const BUNDLE_END_SECTION: &str = "end";

#[derive(Debug, thiserror::Error)]
pub enum BundleError {
    #[error("record bundle: not implemented: {0}")]
    NotImplemented(&'static str),
    #[error("record bundle: {0}")]
    Invalid(String),
    #[error("record bundle: {0}")]
    Io(#[from] std::io::Error),
}

fn section_index(section: &str) -> Option<usize> {
    BUNDLE_SECTIONS.iter().position(|name| *name == section)
}

/// Writes a bundle line by line, hashing as it goes.
pub struct BundleWriter<W: std::io::Write> {
    out: W,
    hasher: Sha256,
    lines: u64,
    /// Index in [`BUNDLE_SECTIONS`] of the last section written.
    position: usize,
}

impl<W: std::io::Write> BundleWriter<W> {
    pub fn new(out: W, header: &BundleHeader) -> Result<Self, BundleError> {
        if header.schema != RECORD_BUNDLE_SCHEMA {
            return Err(BundleError::Invalid(format!(
                "the header's schema is {RECORD_BUNDLE_SCHEMA}"
            )));
        }
        let mut writer = Self {
            out,
            hasher: Sha256::new(),
            lines: 0,
            position: 0,
        };
        let line =
            serde_json::to_vec(header).map_err(|error| BundleError::Invalid(error.to_string()))?;
        writer.write_line(&line)?;
        Ok(writer)
    }

    fn write_line(&mut self, line: &[u8]) -> Result<(), BundleError> {
        if line.len() + 1 > MAX_BUNDLE_LINE_BYTES {
            return Err(BundleError::Invalid(format!(
                "a section line is larger than {MAX_BUNDLE_LINE_BYTES} bytes"
            )));
        }
        self.out.write_all(line)?;
        self.out.write_all(b"\n")?;
        self.hasher.update(line);
        self.hasher.update(b"\n");
        Ok(())
    }

    /// Write one section line. Sections must come in [`BUNDLE_SECTIONS`]
    /// order; a section may repeat (`turn`, `network`, `run_event`).
    pub fn section(&mut self, section: &str, data: &serde_json::Value) -> Result<(), BundleError> {
        let index = section_index(section)
            .ok_or_else(|| BundleError::Invalid(format!("unknown section {section:?}")))?;
        if index < self.position {
            return Err(BundleError::Invalid(format!(
                "section {section:?} comes before {:?}",
                BUNDLE_SECTIONS[self.position]
            )));
        }
        #[derive(Serialize)]
        struct Line<'a> {
            section: &'a str,
            data: &'a serde_json::Value,
        }
        let line = serde_json::to_vec(&Line { section, data })
            .map_err(|error| BundleError::Invalid(error.to_string()))?;
        self.write_line(&line)?;
        self.position = index;
        self.lines += 1;
        Ok(())
    }

    /// The output written so far, for a caller that drains it between
    /// sections (a streamed response). The digest is unaffected.
    pub fn get_mut(&mut self) -> &mut W {
        &mut self.out
    }

    /// Section lines written so far.
    pub fn lines(&self) -> u64 {
        self.lines
    }

    /// Write the end line and return the writer.
    pub fn finish(mut self) -> Result<W, BundleError> {
        let end = BundleEnd {
            section: BUNDLE_END_SECTION.into(),
            lines: self.lines,
            sha256: format!("{:x}", self.hasher.clone().finalize()),
        };
        let line =
            serde_json::to_vec(&end).map_err(|error| BundleError::Invalid(error.to_string()))?;
        self.out.write_all(&line)?;
        self.out.write_all(b"\n")?;
        self.out.flush()?;
        Ok(self.out)
    }
}

/// Read one line, at most [`MAX_BUNDLE_LINE_BYTES`]; `None` at the end.
fn read_line(input: &mut impl BufRead, buffer: &mut Vec<u8>) -> Result<bool, BundleError> {
    buffer.clear();
    let read = input
        .by_ref()
        .take(MAX_BUNDLE_LINE_BYTES as u64 + 1)
        .read_until(b'\n', buffer)?;
    if read == 0 {
        return Ok(false);
    }
    if buffer.len() > MAX_BUNDLE_LINE_BYTES {
        return Err(BundleError::Invalid(format!(
            "a line is longer than {MAX_BUNDLE_LINE_BYTES} bytes"
        )));
    }
    Ok(true)
}

/// Verify a bundle's order, count and digest.
pub fn verify_bundle(mut input: impl std::io::BufRead) -> Result<BundleSummary, BundleError> {
    let mut hasher = Sha256::new();
    let mut buffer = Vec::new();
    if !read_line(&mut input, &mut buffer)? {
        return Err(BundleError::Invalid("the file is empty".into()));
    }
    if buffer.last() != Some(&b'\n') {
        return Err(BundleError::Invalid("the header line is cut off".into()));
    }
    let header: BundleHeader = serde_json::from_slice(&buffer[..buffer.len() - 1])
        .map_err(|error| BundleError::Invalid(format!("line 1 is not a bundle header: {error}")))?;
    if header.schema != RECORD_BUNDLE_SCHEMA {
        return Err(BundleError::Invalid(format!(
            "schema {:?} is not {RECORD_BUNDLE_SCHEMA}",
            header.schema
        )));
    }
    hasher.update(&buffer);
    let mut sections: Vec<(String, u64)> = Vec::new();
    let mut position = 0usize;
    let mut lines = 0u64;
    let mut line_number = 1u64;
    loop {
        if !read_line(&mut input, &mut buffer)? {
            return Err(BundleError::Invalid(
                "the end line is missing: the file is cut off".into(),
            ));
        }
        line_number += 1;
        let complete = buffer.last() == Some(&b'\n');
        let body = if complete {
            &buffer[..buffer.len() - 1]
        } else {
            &buffer[..]
        };
        let value: serde_json::Value = serde_json::from_slice(body).map_err(|error| {
            BundleError::Invalid(format!("line {line_number} is not JSON: {error}"))
        })?;
        let section = value
            .get("section")
            .and_then(|section| section.as_str())
            .ok_or_else(|| BundleError::Invalid(format!("line {line_number} names no section")))?
            .to_string();
        if section == BUNDLE_END_SECTION {
            let end: BundleEnd = serde_json::from_value(value).map_err(|error| {
                BundleError::Invalid(format!("the end line is malformed: {error}"))
            })?;
            let digest = format!("{:x}", hasher.finalize());
            if end.lines != lines {
                return Err(BundleError::Invalid(format!(
                    "the end line counts {} section lines; the file has {lines}",
                    end.lines
                )));
            }
            if end.sha256 != digest {
                return Err(BundleError::Invalid(format!(
                    "the digest does not match: the end line says {}, the content hashes to {digest}",
                    end.sha256
                )));
            }
            if !complete {
                return Err(BundleError::Invalid("the end line is cut off".into()));
            }
            if read_line(&mut input, &mut buffer)? {
                return Err(BundleError::Invalid(
                    "there is content after the end line".into(),
                ));
            }
            return Ok(BundleSummary {
                header,
                lines,
                sections,
            });
        }
        if !complete {
            return Err(BundleError::Invalid(format!(
                "line {line_number} is cut off"
            )));
        }
        let index = section_index(&section).ok_or_else(|| {
            BundleError::Invalid(format!("line {line_number}: unknown section {section:?}"))
        })?;
        if index < position {
            return Err(BundleError::Invalid(format!(
                "line {line_number}: section {section:?} comes after {:?}",
                BUNDLE_SECTIONS[position]
            )));
        }
        if value.get("data").is_none() {
            return Err(BundleError::Invalid(format!(
                "line {line_number}: the section has no data"
            )));
        }
        position = index;
        lines += 1;
        hasher.update(&buffer);
        match sections.last_mut() {
            Some((name, count)) if *name == section => *count += 1,
            _ => sections.push((section, 1)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> BundleHeader {
        BundleHeader {
            schema: RECORD_BUNDLE_SCHEMA.into(),
            run_id: "run-1".into(),
            session_id: "ses-1".into(),
            created_at_ms: 5,
            axocoatl_version: "1.3.0".into(),
        }
    }

    fn bundle() -> Vec<u8> {
        let mut writer = BundleWriter::new(Vec::new(), &header()).unwrap();
        writer
            .section("manifest", &serde_json::json!({"run_id": "run-1"}))
            .unwrap();
        writer
            .section(
                "loadout",
                &serde_json::json!({"text": "id: x\n", "digest": "d"}),
            )
            .unwrap();
        writer
            .section("outcome", &serde_json::json!({"verdict": "pass"}))
            .unwrap();
        for seq in 1..=3 {
            writer
                .section("network", &serde_json::json!({"seq": seq}))
                .unwrap();
        }
        writer
            .section("run_event", &serde_json::json!({"seq": 1, "event": {}}))
            .unwrap();
        assert_eq!(writer.lines(), 7);
        writer.finish().unwrap()
    }

    #[test]
    fn write_then_verify() {
        let bytes = bundle();
        let summary = verify_bundle(bytes.as_slice()).unwrap();
        assert_eq!(summary.header, header());
        assert_eq!(summary.lines, 7);
        assert_eq!(
            summary.sections,
            vec![
                ("manifest".to_string(), 1),
                ("loadout".to_string(), 1),
                ("outcome".to_string(), 1),
                ("network".to_string(), 3),
                ("run_event".to_string(), 1),
            ]
        );
        let text = String::from_utf8(bytes).unwrap();
        let last = text.lines().last().unwrap();
        assert!(last.starts_with("{\"section\":\"end\",\"lines\":7,\"sha256\":\""));
    }

    #[test]
    fn a_one_byte_tamper_is_detected() {
        let bytes = bundle();
        let target = bytes
            .windows(4)
            .position(|window| window == b"pass")
            .unwrap();
        let mut tampered = bytes.clone();
        tampered[target] = b'P';
        let error = verify_bundle(tampered.as_slice()).unwrap_err();
        assert!(error.to_string().contains("digest"), "{error}");
    }

    #[test]
    fn order_count_and_truncation_are_checked() {
        let mut writer = BundleWriter::new(Vec::new(), &header()).unwrap();
        writer.section("outcome", &serde_json::json!({})).unwrap();
        assert!(writer.section("manifest", &serde_json::json!({})).is_err());
        assert!(writer.section("mystery", &serde_json::json!({})).is_err());

        let bytes = bundle();
        let text = String::from_utf8(bytes.clone()).unwrap();
        let mut lines: Vec<&str> = text.lines().collect();
        // Cut off: no end line.
        let cut = lines[..lines.len() - 1].join("\n") + "\n";
        assert!(verify_bundle(cut.as_bytes()).is_err());
        // Swapped sections.
        lines.swap(1, 2);
        let swapped = lines.join("\n") + "\n";
        assert!(verify_bundle(swapped.as_bytes()).is_err());
        // A wrong count.
        let wrong = text.replace("\"lines\":7", "\"lines\":6");
        assert!(verify_bundle(wrong.as_bytes())
            .unwrap_err()
            .to_string()
            .contains("counts"));
        // Content after the end line.
        let mut extra = bytes.clone();
        extra.extend_from_slice(b"{}\n");
        assert!(verify_bundle(extra.as_slice()).is_err());
        // Another schema.
        let other = text.replace(RECORD_BUNDLE_SCHEMA, "axocoatl.record-bundle/9");
        assert!(verify_bundle(other.as_bytes()).is_err());
        assert!(verify_bundle(&b""[..]).is_err());
    }
}
