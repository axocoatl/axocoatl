//! Reports a required check writes (JUnit XML, or tester-army/e2e's
//! `report.json`), parsed into the Outcome's check results.
//!
//! A report counts only when it is bound to the exact check run: the check
//! prints `AXOCOATL-CHECK-REPORT sha256=<hex>` as its last stdout line, and
//! the host reads the file from the Session container and accepts it only
//! when its SHA-256 matches the line in the run's recorded stdout. The
//! binding says which file the run produced; it is not a signature. A report
//! is evidence for the Outcome's test cases, never the check's verdict: pass
//! or fail always comes from the check's exit status.
//!
//! Both parsers read untrusted bytes: at most [`MAX_REPORT_BYTES`], at most
//! [`MAX_REPORT_TESTS`] test cases kept (the rest counted in `truncated`),
//! every text field bounded and stripped of control characters. The XML
//! reader resolves only the five predefined entities and character
//! references; it reads no DTD and expands nothing else.
//!
//! Owner: workstream `e2e`.

use std::collections::{BTreeSet, HashMap};

use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::run_outcome::{CheckReport, CheckTestCase};

/// Prefix of the stdout line that binds a report to its check run.
pub const REPORT_MARKER_PREFIX: &str = "AXOCOATL-CHECK-REPORT sha256=";
/// Largest report read, in bytes.
pub const MAX_REPORT_BYTES: usize = 4 * 1024 * 1024;
/// Most test cases kept per report.
pub const MAX_REPORT_TESTS: usize = 2_000;
/// Longest message kept per test case, in bytes.
pub const MAX_REPORT_MESSAGE_BYTES: usize = 4 * 1024;
/// Longest suite or test name kept, in bytes.
pub const MAX_REPORT_NAME_BYTES: usize = 1024;
/// `CheckReportSpec::format` of a JUnit XML report.
pub const REPORT_FORMAT_JUNIT: &str = "junit";
/// `CheckReportSpec::format` of tester-army/e2e's `report.json`.
pub const REPORT_FORMAT_E2E: &str = "e2e_report_json";
/// `schemaVersion` of the e2e report this parser reads.
pub const E2E_REPORT_SCHEMA: &str = "report-1";

/// A passing test case ([`CheckTestCase::status`]).
pub const STATUS_PASSED: &str = "passed";
/// A test case that failed its assertion or timed out.
pub const STATUS_FAILED: &str = "failed";
/// A test case that was skipped, or stopped before its verdict.
pub const STATUS_SKIPPED: &str = "skipped";
/// A test case that could not reach a verdict for another reason.
pub const STATUS_ERROR: &str = "error";

/// Deepest element nesting a JUnit document may have.
const MAX_XML_DEPTH: usize = 128;

/// Why a report could not be read.
#[derive(Debug, thiserror::Error)]
pub enum CheckReportError {
    #[error("check report: {0}")]
    Invalid(String),
    #[error("check report: larger than {MAX_REPORT_BYTES} bytes")]
    TooLarge,
    #[error("check report: unknown format {0:?}")]
    UnknownFormat(String),
}

fn invalid(reason: impl Into<String>) -> CheckReportError {
    CheckReportError::Invalid(reason.into())
}

/// The digest named by the last marker line of `stdout`, when there is one.
///
/// The check's wrapper prints the marker as its last stdout line. Lines a
/// host appends after it (a truncation note) do not hide it, and an earlier
/// marker never stands in for a malformed last one.
pub fn report_marker(stdout: &str) -> Option<String> {
    let line = stdout
        .lines()
        .rev()
        .map(|line| line.trim_end_matches('\r'))
        .find(|line| line.starts_with(REPORT_MARKER_PREFIX))?;
    let digest = &line[REPORT_MARKER_PREFIX.len()..];
    (digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)))
    .then(|| digest.to_owned())
}

/// SHA-256 of `bytes`, lowercase hex: what a marker names.
pub fn report_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Parse `bytes` as a report in `format` (`junit` or `e2e_report_json`).
pub fn parse_report(format: &str, bytes: &[u8]) -> Result<CheckReport, CheckReportError> {
    match format {
        REPORT_FORMAT_JUNIT => parse_junit(bytes),
        REPORT_FORMAT_E2E => parse_e2e_report_json(bytes),
        other => Err(CheckReportError::UnknownFormat(bounded(
            other,
            MAX_REPORT_NAME_BYTES,
        ))),
    }
}

/// Text without control characters other than tab and newline, at most
/// `max` bytes, cut on a character boundary.
fn bounded(text: &str, max: usize) -> String {
    let mut out = String::with_capacity(text.len().min(max));
    for ch in text
        .chars()
        .filter(|ch| !ch.is_control() || *ch == '\t' || *ch == '\n')
    {
        if out.len() + ch.len_utf8() > max {
            break;
        }
        out.push(ch);
    }
    out
}

/// A bounded, trimmed message; `None` when nothing is left.
fn message(text: &str) -> Option<String> {
    let text = bounded(text.trim(), MAX_REPORT_MESSAGE_BYTES);
    let text = text.trim_end();
    (!text.is_empty()).then(|| text.to_owned())
}

/// Counts every case and keeps the first [`MAX_REPORT_TESTS`].
struct Tally {
    format: &'static str,
    sha256: String,
    tests: Vec<CheckTestCase>,
    passed: u32,
    failed: u32,
    skipped: u32,
    errors: u32,
    truncated: u32,
}

impl Tally {
    fn new(format: &'static str, bytes: &[u8]) -> Self {
        Self {
            format,
            sha256: report_digest(bytes),
            tests: Vec::new(),
            passed: 0,
            failed: 0,
            skipped: 0,
            errors: 0,
            truncated: 0,
        }
    }

    fn push(
        &mut self,
        suite: &str,
        name: &str,
        status: &'static str,
        message: Option<String>,
        duration_ms: Option<u64>,
    ) {
        let counter = match status {
            STATUS_PASSED => &mut self.passed,
            STATUS_FAILED => &mut self.failed,
            STATUS_SKIPPED => &mut self.skipped,
            _ => &mut self.errors,
        };
        *counter = counter.saturating_add(1);
        if self.tests.len() >= MAX_REPORT_TESTS {
            self.truncated = self.truncated.saturating_add(1);
            return;
        }
        self.tests.push(CheckTestCase {
            suite: bounded(suite, MAX_REPORT_NAME_BYTES),
            name: bounded(name, MAX_REPORT_NAME_BYTES),
            status: status.to_owned(),
            message,
            duration_ms,
        });
    }

    fn finish(self) -> CheckReport {
        CheckReport {
            format: self.format.to_owned(),
            sha256: self.sha256,
            tests: self.tests,
            passed: self.passed,
            failed: self.failed,
            skipped: self.skipped,
            errors: self.errors,
            truncated: self.truncated,
        }
    }
}

fn check_size(bytes: &[u8]) -> Result<(), CheckReportError> {
    if bytes.len() > MAX_REPORT_BYTES {
        return Err(CheckReportError::TooLarge);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// JUnit XML
// ---------------------------------------------------------------------------

/// JUnit `time` (seconds, sometimes with a decimal comma) in milliseconds.
fn junit_duration_ms(value: &str) -> Option<u64> {
    let seconds: f64 = value.trim().replace(',', ".").parse().ok()?;
    (seconds.is_finite() && (0.0..=1.0e9).contains(&seconds))
        .then(|| (seconds * 1000.0).round() as u64)
}

/// How bad a JUnit result element is: `error` > `failure` > `skipped`.
fn junit_rank(status: &str) -> u8 {
    match status {
        STATUS_ERROR => 3,
        STATUS_FAILED => 2,
        STATUS_SKIPPED => 1,
        _ => 0,
    }
}

/// One `<testcase>` being read.
struct PendingCase {
    suite: String,
    name: String,
    duration_ms: Option<u64>,
    status: &'static str,
    message: Option<String>,
    /// Text of the result element that decided `status` while it is open
    /// and carried no `message` attribute.
    text: Option<String>,
    /// Depth of that result element.
    text_depth: usize,
    /// Depth of the `<testcase>` element itself.
    depth: usize,
}

/// What [`parse_junit`] has read so far.
struct JunitReader {
    tally: Tally,
    /// Open suites: their depth and the name their cases inherit.
    suites: Vec<(usize, String)>,
    saw_suite: bool,
    current: Option<PendingCase>,
}

type Attributes = HashMap<Vec<u8>, String>;

fn attributes(
    reader: &Reader<&[u8]>,
    element: &BytesStart<'_>,
) -> Result<Attributes, CheckReportError> {
    let mut values = HashMap::new();
    for attribute in element.attributes() {
        let attribute =
            attribute.map_err(|error| invalid(format!("unreadable JUnit attribute: {error}")))?;
        let value = attribute
            .decoded_and_normalized_value(quick_xml::XmlVersion::Implicit1_0, reader.decoder())
            .map_err(|error| invalid(format!("unreadable JUnit attribute: {error}")))?;
        values.insert(
            attribute.key.local_name().as_ref().to_vec(),
            value.into_owned(),
        );
    }
    Ok(values)
}

fn take(attributes: &mut Attributes, name: &str) -> Option<String> {
    attributes.remove(name.as_bytes())
}

impl JunitReader {
    fn open(
        &mut self,
        element: &[u8],
        mut attributes: Attributes,
        depth: usize,
        empty: bool,
    ) -> Result<(), CheckReportError> {
        match element {
            b"testsuite" | b"testsuites" => {
                if self.current.is_some() {
                    return Err(invalid("a test suite inside a test case"));
                }
                self.saw_suite = true;
                if !empty {
                    let inherited = self
                        .suites
                        .last()
                        .map(|(_, name)| name.clone())
                        .unwrap_or_default();
                    let name = take(&mut attributes, "name")
                        .filter(|name| element == b"testsuite" && !name.trim().is_empty())
                        .unwrap_or(inherited);
                    self.suites.push((depth, name));
                }
            }
            b"testcase" => {
                if self.current.is_some() {
                    return Err(invalid("a test case inside a test case"));
                }
                let suite = take(&mut attributes, "classname")
                    .filter(|name| !name.trim().is_empty())
                    .or_else(|| self.suites.last().map(|(_, name)| name.clone()))
                    .unwrap_or_default();
                let case = PendingCase {
                    suite,
                    name: take(&mut attributes, "name").unwrap_or_default(),
                    duration_ms: take(&mut attributes, "time")
                        .as_deref()
                        .and_then(junit_duration_ms),
                    status: STATUS_PASSED,
                    message: None,
                    text: None,
                    text_depth: 0,
                    depth,
                };
                if empty {
                    self.finish_case(case);
                } else {
                    self.current = Some(case);
                }
            }
            b"failure" | b"error" | b"skipped" => {
                let Some(case) = self.current.as_mut() else {
                    return Ok(());
                };
                if depth != case.depth + 1 {
                    return Ok(());
                }
                let status = match element {
                    b"failure" => STATUS_FAILED,
                    b"error" => STATUS_ERROR,
                    _ => STATUS_SKIPPED,
                };
                if junit_rank(status) > junit_rank(case.status) {
                    case.status = status;
                    case.message = take(&mut attributes, "message")
                        .as_deref()
                        .and_then(message)
                        .or_else(|| take(&mut attributes, "type").as_deref().and_then(message));
                    case.text = (case.message.is_none() && !empty).then(String::new);
                    case.text_depth = depth;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn close(&mut self, element: &[u8], depth: usize) {
        let closes_case = self
            .current
            .as_ref()
            .is_some_and(|case| case.depth == depth);
        let closes_suite = self.suites.last().is_some_and(|(open, _)| *open == depth);
        match element {
            b"testsuite" | b"testsuites" if closes_suite => {
                self.suites.pop();
            }
            b"testcase" if closes_case => {
                if let Some(case) = self.current.take() {
                    self.finish_case(case);
                }
            }
            b"failure" | b"error" | b"skipped" => {
                if let Some(case) = self
                    .current
                    .as_mut()
                    .filter(|case| case.text.is_some() && case.text_depth == depth)
                {
                    case.message = case.text.take().as_deref().and_then(message);
                }
            }
            _ => {}
        }
    }

    fn text(&mut self, text: &str) {
        if let Some(buffer) = self.current.as_mut().and_then(|case| case.text.as_mut()) {
            if buffer.len() < MAX_REPORT_MESSAGE_BYTES * 2 {
                buffer.push_str(text);
            }
        }
    }

    fn finish_case(&mut self, mut case: PendingCase) {
        if let Some(text) = case.text.take() {
            case.message = message(&text);
        }
        self.tally.push(
            &case.suite,
            &case.name,
            case.status,
            case.message,
            case.duration_ms,
        );
    }
}

/// Parse a JUnit XML document: `<testsuites>` or one `<testsuite>`, with
/// `<testcase>` elements whose `<failure>`, `<error>` or `<skipped>` child
/// gives their status (none: passed; several: the worst). A case's suite is
/// its `classname`, else the innermost enclosing suite's name.
pub fn parse_junit(bytes: &[u8]) -> Result<CheckReport, CheckReportError> {
    check_size(bytes)?;
    let mut reader = Reader::from_reader(bytes);
    let mut state = JunitReader {
        tally: Tally::new(REPORT_FORMAT_JUNIT, bytes),
        suites: Vec::new(),
        saw_suite: false,
        current: None,
    };
    let mut depth = 0_usize;
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        let event = match reader.read_event_into(&mut buffer) {
            Ok(event) => event,
            Err(error) => {
                return Err(invalid(format!(
                    "not well-formed JUnit XML at byte {}: {error}",
                    reader.error_position()
                )))
            }
        };
        match event {
            Event::Start(element) => {
                depth += 1;
                if depth > MAX_XML_DEPTH {
                    return Err(invalid("JUnit elements nest too deeply"));
                }
                let found = attributes(&reader, &element)?;
                state.open(element.local_name().as_ref(), found, depth, false)?;
            }
            Event::Empty(element) => {
                let found = attributes(&reader, &element)?;
                state.open(element.local_name().as_ref(), found, depth + 1, true)?;
            }
            Event::End(element) => {
                state.close(element.local_name().as_ref(), depth);
                depth = depth.saturating_sub(1);
            }
            Event::Text(text) => {
                let text = text
                    .decode()
                    .map_err(|error| invalid(format!("unreadable JUnit text: {error}")))?;
                state.text(&text);
            }
            Event::CData(data) => {
                let text = data
                    .decode()
                    .map_err(|error| invalid(format!("unreadable JUnit text: {error}")))?;
                state.text(&text);
            }
            Event::GeneralRef(reference) => {
                if reference.is_char_ref() {
                    if let Ok(Some(ch)) = reference.resolve_char_ref() {
                        state.text(ch.encode_utf8(&mut [0; 4]));
                    }
                } else if let Ok(name) = reference.decode() {
                    match quick_xml::escape::resolve_predefined_entity(&name) {
                        Some(value) => state.text(value),
                        None => state.text(&format!("&{name};")),
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if depth != 0 || state.current.is_some() {
        return Err(invalid("the JUnit document ends inside an element"));
    }
    if !state.saw_suite {
        return Err(invalid(
            "not a JUnit document: no <testsuite> or <testsuites> element",
        ));
    }
    Ok(state.tally.finish())
}

// ---------------------------------------------------------------------------
// tester-army/e2e report.json (`report-1`)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct E2eReport {
    #[serde(rename = "schemaVersion")]
    schema_version: String,
    run: E2eRun,
}

#[derive(Debug, Deserialize)]
struct E2eRun {
    #[serde(default)]
    results: Vec<E2eResult>,
    #[serde(default)]
    errors: Vec<E2eError>,
    #[serde(default, rename = "serialGroups")]
    serial_groups: Vec<E2eSerialGroup>,
}

#[derive(Debug, Deserialize)]
struct E2eResult {
    #[serde(default, rename = "testId")]
    test_id: String,
    #[serde(default, rename = "titlePath")]
    title_path: Vec<String>,
    #[serde(default)]
    file: String,
    #[serde(default, rename = "targetId")]
    target_id: String,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    repeat: Option<u64>,
    #[serde(default)]
    selected: Option<bool>,
    #[serde(default, rename = "serialGroupId")]
    serial_group_id: Option<String>,
    status: String,
    #[serde(default)]
    skip: Option<E2eSkip>,
    #[serde(default)]
    attempts: Vec<E2eAttempt>,
}

#[derive(Debug, Deserialize)]
struct E2eAttempt {
    #[serde(default)]
    status: String,
    #[serde(default, rename = "durationMs")]
    duration_ms: Option<u64>,
    #[serde(default)]
    error: Option<E2eError>,
    #[serde(default)]
    steps: Vec<E2eStep>,
}

#[derive(Debug, Deserialize)]
struct E2eSerialGroup {
    #[serde(default)]
    id: String,
    #[serde(default)]
    attempts: Vec<E2eSerialAttempt>,
}

#[derive(Debug, Deserialize)]
struct E2eSerialAttempt {
    #[serde(default)]
    status: String,
    #[serde(default)]
    error: Option<E2eError>,
    #[serde(default)]
    members: Vec<E2eSerialMember>,
}

#[derive(Debug, Deserialize)]
struct E2eSerialMember {
    #[serde(default, rename = "testId")]
    test_id: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    error: Option<E2eError>,
    #[serde(default, rename = "durationMs")]
    duration_ms: Option<u64>,
    #[serde(default)]
    steps: Vec<E2eStep>,
}

#[derive(Debug, Clone, Deserialize)]
struct E2eError {
    #[serde(default)]
    category: String,
    #[serde(default)]
    code: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    phase: Option<String>,
}

#[derive(Debug, Deserialize)]
struct E2eSkip {
    #[serde(default)]
    reason: String,
}

#[derive(Debug, Deserialize)]
struct E2eStep {
    #[serde(default)]
    model: Option<E2eModel>,
}

#[derive(Debug, Deserialize)]
struct E2eModel {
    #[serde(default)]
    model: String,
}

/// One attempt of a result as the reporters read it: its own, or, for a
/// member of a serial group, its share of the group's attempt.
struct AttemptView<'a> {
    status: &'a str,
    error: Option<&'a E2eError>,
    duration_ms: Option<u64>,
}

fn attempt_views<'a>(
    result: &'a E2eResult,
    groups: &HashMap<&str, &'a E2eSerialGroup>,
) -> Vec<AttemptView<'a>> {
    match result.serial_group_id.as_deref() {
        None => result
            .attempts
            .iter()
            .map(|attempt| AttemptView {
                status: &attempt.status,
                error: attempt.error.as_ref(),
                duration_ms: attempt.duration_ms,
            })
            .collect(),
        Some(group) => groups
            .get(group)
            .map(|group| {
                group
                    .attempts
                    .iter()
                    .map(|attempt| {
                        let member = attempt
                            .members
                            .iter()
                            .find(|member| member.test_id == result.test_id);
                        AttemptView {
                            status: member
                                .and_then(|member| member.status.as_deref())
                                .unwrap_or(&attempt.status),
                            error: member
                                .and_then(|member| member.error.as_ref())
                                .or(attempt.error.as_ref()),
                            duration_ms: member.and_then(|member| member.duration_ms),
                        }
                    })
                    .collect()
            })
            .unwrap_or_default(),
    }
}

fn error_text(error: &E2eError) -> String {
    let phase = error
        .phase
        .as_deref()
        .map(|phase| format!(" ({phase})"))
        .unwrap_or_default();
    format!("{}{phase}: {}", error.code, error.message)
}

/// One result as a test case: `(status, message)`, read the way e2e's own
/// JUnit reporter reads it. A test-class error is a failure, any other an
/// error; an interrupted test reached no verdict and reads as skipped.
fn e2e_case(result: &E2eResult, attempts: &[AttemptView<'_>]) -> (&'static str, Option<String>) {
    let failed = |attempt: &&AttemptView<'_>| matches!(attempt.status, "failed" | "timed-out");
    match result.status.as_str() {
        "passed" => (STATUS_PASSED, None),
        "flaky" => {
            let count = attempts
                .iter()
                .take(attempts.len().saturating_sub(1))
                .filter(|attempt| attempt.status != "passed")
                .count();
            let plural = if count == 1 { "" } else { "s" };
            (
                STATUS_PASSED,
                Some(format!(
                    "flaky: {count} failed attempt{plural} before passing"
                )),
            )
        }
        "failed" | "timed-out" => {
            let error = attempts
                .iter()
                .rev()
                .find(failed)
                .or(attempts.last())
                .and_then(|attempt| attempt.error);
            let status = match error {
                Some(error) if error.category != "test" => STATUS_ERROR,
                _ => STATUS_FAILED,
            };
            let text = error
                .map(error_text)
                .unwrap_or_else(|| result.status.clone());
            (status, message(&text))
        }
        "interrupted" => {
            let reason = attempts
                .last()
                .and_then(|attempt| attempt.error)
                .map(|error| error.message.clone())
                .unwrap_or_else(|| "the run was stopped".into());
            (STATUS_SKIPPED, message(&format!("interrupted: {reason}")))
        }
        "skipped" => (
            STATUS_SKIPPED,
            message(
                result
                    .skip
                    .as_ref()
                    .map(|skip| skip.reason.as_str())
                    .filter(|reason| !reason.trim().is_empty())
                    .unwrap_or("skipped"),
            ),
        ),
        other => (
            STATUS_ERROR,
            message(&format!("e2e reported the unknown status {other:?}")),
        ),
    }
}

fn read_e2e_report(bytes: &[u8]) -> Result<E2eReport, CheckReportError> {
    check_size(bytes)?;
    let report: E2eReport = serde_json::from_slice(bytes)
        .map_err(|error| invalid(format!("not an e2e report.json: {error}")))?;
    if report.schema_version != E2E_REPORT_SCHEMA {
        return Err(invalid(format!(
            "e2e report schemaVersion {:?}; this Axocoatl reads {E2E_REPORT_SCHEMA:?}",
            bounded(&report.schema_version, 64)
        )));
    }
    Ok(report)
}

/// Parse tester-army/e2e's `report.json` (`schemaVersion: report-1`): one
/// case per selected test-target pair, named as e2e's JUnit reporter names
/// it (`title > path [target]`, the file as the suite), plus one `error`
/// case per run-level error in suite `run` (a startup failure such as
/// `APP_UNREACHABLE` is never silently absent).
pub fn parse_e2e_report_json(bytes: &[u8]) -> Result<CheckReport, CheckReportError> {
    let report = read_e2e_report(bytes)?;
    let groups: HashMap<&str, &E2eSerialGroup> = report
        .run
        .serial_groups
        .iter()
        .map(|group| (group.id.as_str(), group))
        .collect();
    let mut tally = Tally::new(REPORT_FORMAT_E2E, bytes);
    for result in &report.run.results {
        // Unselected results are report-only: the run did not choose them.
        if result.selected == Some(false) {
            continue;
        }
        let attempts = attempt_views(result, &groups);
        let (status, message) = e2e_case(result, &attempts);
        let repeat = match result.repeat {
            Some(repeat) if repeat > 0 => format!(" (repeat #{repeat})"),
            _ => String::new(),
        };
        let agent = match result.agent.as_deref() {
            Some(agent) if agent != "default" => format!(" [{agent}]"),
            _ => String::new(),
        };
        let name = format!(
            "{}{repeat} [{}]{agent}",
            result.title_path.join(" > "),
            result.target_id
        );
        let duration = attempts.last().and_then(|attempt| attempt.duration_ms);
        tally.push(&result.file, &name, status, message, duration);
    }
    for error in &report.run.errors {
        let phase = error
            .phase
            .as_deref()
            .map(|phase| format!(" ({phase})"))
            .unwrap_or_default();
        tally.push(
            "run",
            &format!("{}{phase}", error.code),
            STATUS_ERROR,
            message(&format!("{} error {}", error.category, error_text(error))),
            Some(0),
        );
    }
    Ok(tally.finish())
}

/// Every model e2e's agent steps called, as the report names them (the
/// provider's model id). Empty when no step called a model (every action
/// replayed from the cache, or a test without agent steps).
pub fn e2e_report_models(bytes: &[u8]) -> Result<BTreeSet<String>, CheckReportError> {
    let report = read_e2e_report(bytes)?;
    let mut models = BTreeSet::new();
    let steps = report
        .run
        .results
        .iter()
        .flat_map(|result| result.attempts.iter())
        .flat_map(|attempt| attempt.steps.iter())
        .chain(
            report
                .run
                .serial_groups
                .iter()
                .flat_map(|group| group.attempts.iter())
                .flat_map(|attempt| attempt.members.iter())
                .flat_map(|member| member.steps.iter()),
        );
    for step in steps {
        if let Some(model) = step.model.as_ref().filter(|model| !model.model.is_empty()) {
            if models.len() >= 64 {
                break;
            }
            models.insert(bounded(&model.model, 256));
        }
    }
    Ok(models)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `e2e run --reporter junit` of e2e@0.18.0 in the recipe image: one
    /// passing, one failing (two attempts under CI) and one skipped test.
    const E2E_JUNIT: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<testsuites name="e2e" tests="3" failures="1" errors="0" skipped="1" time="2.304">
  <testsuite name="tests/smoke.e2e.ts" tests="3" failures="1" errors="0" skipped="1" time="2.304">
    <testcase name="home &gt; shows the greeting [web]" classname="tests/smoke.e2e.ts" time="0.107"/>
    <testcase name="home &gt; fails on purpose [web]" classname="tests/smoke.e2e.ts" time="2.197">
      <failure message="expect.toContainText failed&#10;locator: getByRole(&quot;heading&quot;)&#10;expected: text containing &quot;Goodbye&quot;&#10;observed: text &quot;Hello &amp; welcome&quot; (match count 1)" type="ASSERTION_FAILED">ASSERTION_FAILED (body): expect.toContainText failed
locator: getByRole(&quot;heading&quot;)
expected: text containing &quot;Goodbye&quot;
observed: text &quot;Hello &amp; welcome&quot; (match count 1)</failure>
    </testcase>
    <testcase name="home &gt; not ready yet [web]" classname="tests/smoke.e2e.ts" time="0.000">
      <skipped message="skipped"/>
    </testcase>
  </testsuite>
</testsuites>
"#;

    /// The `report.json` of the same run (`e2e run --reporter json`), with
    /// artifacts, step events and environment fields trimmed.
    const E2E_REPORT: &str = r#"{"schemaVersion":"report-1","run":{"id":"01a1149a-871c-73af-93bd-14f3a63a73a5","specVersion":"0.1","runner":{"name":"e2e","version":"0.18.0"},"status":"failed","exitCode":1,"startedAt":"2026-10-07T04:23:54.109Z","finishedAt":"2026-10-07T04:23:58.821Z","serialGroups":[],"results":[{"testId":"tests/smoke.e2e.ts::home::shows%20the%20greeting","kind":"test","titlePath":["home","shows the greeting"],"file":"tests/smoke.e2e.ts","targetId":"web","platform":"web","agent":"default","repeat":0,"tags":[],"selected":true,"status":"passed","attempts":[{"index":0,"status":"passed","durationMs":121,"artifacts":[],"secondaryErrors":[],"cleanup":"complete","steps":[{"id":"01a1149a-8923-7078-95f3-a2af9f25a525:0","index":0,"kind":"app","api":"app.open","status":"passed","durationMs":24},{"id":"01a1149a-8923-7078-95f3-a2af9f25a525:1","index":1,"kind":"assertion","api":"expect.toContainText","status":"passed","durationMs":23}]}]},{"testId":"tests/smoke.e2e.ts::home::fails%20on%20purpose","kind":"test","titlePath":["home","fails on purpose"],"file":"tests/smoke.e2e.ts","targetId":"web","platform":"web","agent":"default","repeat":0,"tags":[],"selected":true,"status":"failed","attempts":[{"index":0,"status":"failed","durationMs":2110,"artifacts":[],"error":{"category":"test","code":"ASSERTION_FAILED","message":"expect.toContainText failed\nlocator: getByRole(\"heading\")\nexpected: text containing \"Goodbye\"\nobserved: text \"Hello & welcome\" (match count 1)","retryable":false,"phase":"body","details":{"locator":"getByRole(\"heading\")","expected":"text containing \"Goodbye\"","observed":"text \"Hello & welcome\"","matches":1}},"secondaryErrors":[],"cleanup":"complete","steps":[{"id":"01a1149a-899c-7e5d-960b-f807e0025e82:0","index":0,"kind":"app","api":"app.open","status":"passed","durationMs":17},{"id":"01a1149a-899c-7e5d-960b-f807e0025e82:1","index":1,"kind":"assertion","api":"expect.toContainText","status":"failed","durationMs":2021}]},{"index":1,"status":"failed","durationMs":2156,"artifacts":[],"error":{"category":"test","code":"ASSERTION_FAILED","message":"expect.toContainText failed\nlocator: getByRole(\"heading\")\nexpected: text containing \"Goodbye\"\nobserved: text \"Hello & welcome\" (match count 1)","retryable":false,"phase":"body","details":{"locator":"getByRole(\"heading\")","expected":"text containing \"Goodbye\"","observed":"text \"Hello & welcome\"","matches":1}},"secondaryErrors":[],"cleanup":"complete","steps":[{"id":"01a1149a-91df-7348-84e0-ce19c9614845:0","index":0,"kind":"app","api":"app.open","status":"passed","durationMs":27},{"id":"01a1149a-91df-7348-84e0-ce19c9614845:1","index":1,"kind":"assertion","api":"expect.toContainText","status":"failed","durationMs":2045}]}]},{"testId":"tests/smoke.e2e.ts::home::not%20ready%20yet","kind":"test","titlePath":["home","not ready yet"],"file":"tests/smoke.e2e.ts","targetId":"web","platform":"web","agent":"default","repeat":0,"tags":[],"selected":true,"status":"skipped","skip":{"cause":"explicit","reason":"skipped"},"attempts":[]}],"errors":[],"summary":{"discovered":3,"selected":3,"executed":2,"passed":1,"failed":1,"interrupted":0,"flaky":0,"skipped":1}}}"#;

    fn digest(ch: char) -> String {
        ch.to_string().repeat(64)
    }

    #[test]
    fn the_marker_is_the_last_marker_line() {
        let a = digest('a');
        let b = digest('b');
        let stdout = format!(
            "e2e output\n{REPORT_MARKER_PREFIX}{a}\nmore output\n{REPORT_MARKER_PREFIX}{b}\n"
        );
        assert_eq!(report_marker(&stdout), Some(b.clone()));
        // A note a host appends after the marker does not hide it.
        let noted = format!("{REPORT_MARKER_PREFIX}{b}\r\n[output truncated]\n");
        assert_eq!(report_marker(&noted), Some(b.clone()));
        // A malformed last marker never falls back to an earlier one.
        for bad in [
            format!(
                "{REPORT_MARKER_PREFIX}{a}\n{REPORT_MARKER_PREFIX}{}\n",
                "B".repeat(64)
            ),
            format!(
                "{REPORT_MARKER_PREFIX}{a}\n{REPORT_MARKER_PREFIX}{}\n",
                "b".repeat(63)
            ),
            format!("{REPORT_MARKER_PREFIX}{a}\n{REPORT_MARKER_PREFIX}{b} trailing\n"),
            format!("{REPORT_MARKER_PREFIX}{a}\n{REPORT_MARKER_PREFIX}\n"),
        ] {
            assert_eq!(report_marker(&bad), None, "{bad}");
        }
        assert_eq!(report_marker("no marker here\n"), None);
        assert_eq!(report_marker(""), None);
        // Only a line that starts with the prefix counts.
        assert_eq!(
            report_marker(&format!("  {REPORT_MARKER_PREFIX}{a}\n")),
            None
        );
        assert_eq!(report_digest(b"").len(), 64);
        assert_eq!(
            report_digest(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn junit_from_e2e_passes_fails_and_skips() {
        let report = parse_junit(E2E_JUNIT.as_bytes()).unwrap();
        assert_eq!(report.format, REPORT_FORMAT_JUNIT);
        assert_eq!(report.sha256, report_digest(E2E_JUNIT.as_bytes()));
        assert_eq!(
            (report.passed, report.failed, report.skipped, report.errors),
            (1, 1, 1, 0)
        );
        assert_eq!(report.truncated, 0);
        let names: Vec<_> = report.tests.iter().map(|case| case.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "home > shows the greeting [web]",
                "home > fails on purpose [web]",
                "home > not ready yet [web]"
            ]
        );
        assert!(report
            .tests
            .iter()
            .all(|case| case.suite == "tests/smoke.e2e.ts"));
        let failed = &report.tests[1];
        assert_eq!(failed.status, STATUS_FAILED);
        assert_eq!(failed.duration_ms, Some(2197));
        let message = failed.message.as_deref().unwrap();
        assert!(message.starts_with("expect.toContainText failed\nlocator: getByRole(\"heading\")"));
        assert!(message.contains("\"Hello & welcome\""));
        assert_eq!(report.tests[0].status, STATUS_PASSED);
        assert_eq!(report.tests[0].message, None);
        assert_eq!(report.tests[0].duration_ms, Some(107));
        assert_eq!(report.tests[2].status, STATUS_SKIPPED);
        assert_eq!(report.tests[2].message.as_deref(), Some("skipped"));
    }

    #[test]
    fn junit_errors_text_messages_and_suite_names() {
        let xml = r#"<?xml version="1.0"?>
<!-- pytest -->
<testsuite name="pytest" tests="5">
  <testcase classname="tests.test_api" name="test_ok" time="0,5"/>
  <testcase name="no classname"><error type="TimeoutError"/></testcase>
  <testcase classname="tests.test_api" name="text only"><failure><![CDATA[assert 1 == 2]]> &amp; more &#x41;</failure><system-out>noise</system-out></testcase>
  <testcase classname="tests.test_api" name="worst wins"><skipped message="later"/><error message="boom"/><failure message="assert"/></testcase>
  <testcase classname="" name="flaky rerun"><flakyFailure message="first try"/></testcase>
  <testsuite name="nested"><testcase name="inner"/></testsuite>
</testsuite>"#;
        let report = parse_junit(xml.as_bytes()).unwrap();
        assert_eq!(
            (report.passed, report.failed, report.skipped, report.errors),
            (3, 1, 0, 2)
        );
        let by_name = |name: &str| {
            report
                .tests
                .iter()
                .find(|case| case.name == name)
                .unwrap()
                .clone()
        };
        assert_eq!(by_name("test_ok").duration_ms, Some(500));
        assert_eq!(by_name("test_ok").suite, "tests.test_api");
        let error = by_name("no classname");
        assert_eq!(
            (error.suite.as_str(), error.status.as_str()),
            ("pytest", STATUS_ERROR)
        );
        assert_eq!(error.message.as_deref(), Some("TimeoutError"));
        assert_eq!(
            by_name("text only").message.as_deref(),
            Some("assert 1 == 2 & more A")
        );
        let worst = by_name("worst wins");
        assert_eq!(
            (worst.status.as_str(), worst.message.as_deref()),
            (STATUS_ERROR, Some("boom"))
        );
        assert_eq!(by_name("flaky rerun").status, STATUS_PASSED);
        assert_eq!(by_name("flaky rerun").suite, "pytest");
        assert_eq!(by_name("inner").suite, "nested");
    }

    #[test]
    fn junit_entities_are_never_expanded_beyond_the_predefined_ones() {
        let xml = r#"<?xml version="1.0"?>
<!DOCTYPE lolz [<!ENTITY lol "lol"><!ENTITY lol2 "&lol;&lol;&lol;&lol;&lol;">]>
<testsuites><testsuite name="s"><testcase name="t"><failure>&lol2; &lt;ok&gt;</failure></testcase></testsuite></testsuites>"#;
        let report = parse_junit(xml.as_bytes()).unwrap();
        assert_eq!(report.tests[0].message.as_deref(), Some("&lol2; <ok>"));
    }

    #[test]
    fn junit_refuses_what_it_cannot_read() {
        for bad in [
            "",
            "not xml at all",
            "<results><case/></results>",
            "<testsuites><testsuite name=\"s\"><testcase name=\"t\">",
            "<testsuites><testsuite></testcase></testsuites>",
            "<testsuites><testcase name=\"a\"><testcase name=\"b\"/></testcase></testsuites>",
            "<testsuite><testcase a=\"1\" a=\"2\"/></testsuite>",
        ] {
            assert!(
                matches!(
                    parse_junit(bad.as_bytes()),
                    Err(CheckReportError::Invalid(_))
                ),
                "{bad:?}"
            );
        }
        let deep = format!(
            "{}{}",
            "<testsuite>".repeat(MAX_XML_DEPTH + 1),
            "</testsuite>".repeat(MAX_XML_DEPTH + 1)
        );
        assert!(matches!(
            parse_junit(deep.as_bytes()),
            Err(CheckReportError::Invalid(_))
        ));
        let large = vec![b' '; MAX_REPORT_BYTES + 1];
        assert!(matches!(
            parse_junit(&large),
            Err(CheckReportError::TooLarge)
        ));
        assert!(matches!(
            parse_e2e_report_json(&large),
            Err(CheckReportError::TooLarge)
        ));
    }

    #[test]
    fn large_junit_keeps_the_first_cases_and_counts_the_rest() {
        let mut xml = String::from("<testsuites><testsuite name=\"big\">");
        for index in 0..2_500 {
            if index % 2 == 0 {
                xml.push_str(&format!("<testcase name=\"case {index}\"/>"));
            } else {
                xml.push_str(&format!(
                    "<testcase name=\"case {index}\"><failure message=\"m\"/></testcase>"
                ));
            }
        }
        xml.push_str("</testsuite></testsuites>");
        let report = parse_junit(xml.as_bytes()).unwrap();
        assert_eq!(report.tests.len(), MAX_REPORT_TESTS);
        assert_eq!(report.truncated, 500);
        assert_eq!((report.passed, report.failed), (1_250, 1_250));
        assert_eq!(report.tests[MAX_REPORT_TESTS - 1].name, "case 1999");
    }

    #[test]
    fn text_is_bounded_and_stripped_of_control_characters() {
        let long = "x".repeat(MAX_REPORT_MESSAGE_BYTES * 3);
        let xml = format!(
            "<testsuite name=\"s\x01\"><testcase name=\"a\tb\"><failure message=\"{long}\"/></testcase>\
             <testcase name=\"esc&#27;[31mred\"><failure>\u{e9}{long}</failure></testcase></testsuite>"
        );
        let report = parse_junit(xml.as_bytes()).unwrap();
        assert_eq!(
            report.tests[0].message.as_ref().unwrap().len(),
            MAX_REPORT_MESSAGE_BYTES
        );
        assert_eq!(report.tests[0].name, "a b");
        assert_eq!(report.tests[0].suite, "s");
        assert_eq!(report.tests[1].name, "esc[31mred");
        let second = report.tests[1].message.as_ref().unwrap();
        assert!(second.len() <= MAX_REPORT_MESSAGE_BYTES && second.starts_with('\u{e9}'));
        assert_eq!(bounded("a\u{7}b\nc\u{1b}", 16), "ab\nc");
        assert_eq!(bounded("\u{e9}\u{e9}", 3), "\u{e9}");
    }

    #[test]
    fn e2e_report_from_the_recipe_image_reads_like_its_junit() {
        let report = parse_e2e_report_json(E2E_REPORT.as_bytes()).unwrap();
        let junit = parse_junit(E2E_JUNIT.as_bytes()).unwrap();
        assert_eq!(report.format, REPORT_FORMAT_E2E);
        assert_eq!(report.sha256, report_digest(E2E_REPORT.as_bytes()));
        assert_eq!(
            (report.passed, report.failed, report.skipped, report.errors),
            (junit.passed, junit.failed, junit.skipped, junit.errors)
        );
        for (from_json, from_junit) in report.tests.iter().zip(&junit.tests) {
            assert_eq!(from_json.name, from_junit.name);
            assert_eq!(from_json.suite, from_junit.suite);
            assert_eq!(from_json.status, from_junit.status);
        }
        let failed = &report.tests[1];
        assert_eq!(failed.duration_ms, Some(2156));
        assert_eq!(
            failed.message.as_deref().unwrap(),
            "ASSERTION_FAILED (body): expect.toContainText failed\nlocator: getByRole(\"heading\")\n\
             expected: text containing \"Goodbye\"\nobserved: text \"Hello & welcome\" (match count 1)"
        );
        assert_eq!(report.tests[2].message.as_deref(), Some("skipped"));
        assert!(e2e_report_models(E2E_REPORT.as_bytes()).unwrap().is_empty());
    }

    fn report(results: serde_json::Value, extra: serde_json::Value) -> Vec<u8> {
        let mut run = json!({"status": "failed", "exitCode": 1, "results": results, "errors": []});
        if let (Some(run), Some(extra)) = (run.as_object_mut(), extra.as_object()) {
            for (key, value) in extra {
                run.insert(key.clone(), value.clone());
            }
        }
        serde_json::to_vec(&json!({"schemaVersion": "report-1", "run": run})).unwrap()
    }

    fn result(title: &str, status: &str, attempts: serde_json::Value) -> serde_json::Value {
        json!({"testId": title, "titlePath": ["suite", title], "file": "tests/a.e2e.ts",
               "targetId": "web", "agent": "default", "status": status, "attempts": attempts})
    }

    #[test]
    fn e2e_statuses_errors_and_selection() {
        let infrastructure = json!({"category": "infrastructure", "code": "MODEL_UNAVAILABLE",
                                    "message": "the provider refused", "phase": "body"});
        let failure = json!({"category": "test", "code": "ASSERTION_FAILED", "message": "nope"});
        let mut other_agent = result(
            "careful",
            "passed",
            json!([{"status": "passed", "durationMs": 5}]),
        );
        other_agent["agent"] = json!("careful");
        other_agent["repeat"] = json!(2);
        let mut unselected = result("left out", "skipped", json!([]));
        unselected["selected"] = json!(false);
        let bytes = report(
            json!([
                result(
                    "flaky",
                    "flaky",
                    json!([
                        {"status": "failed", "durationMs": 10, "error": failure},
                        {"status": "passed", "durationMs": 7}
                    ])
                ),
                result(
                    "timed",
                    "timed-out",
                    json!([{"status": "timed-out", "durationMs": 30000,
                    "error": {"category": "test", "code": "TEST_TIMEOUT", "message": "30s"}}])
                ),
                result(
                    "model",
                    "failed",
                    json!([{"status": "failed", "durationMs": 3, "error": infrastructure}])
                ),
                result(
                    "stopped",
                    "interrupted",
                    json!([{"status": "interrupted", "durationMs": 1,
                    "error": {"category": "interrupted", "code": "INTERRUPTED", "message": "SIGTERM"}}])
                ),
                result("odd", "wobbly", json!([])),
                other_agent,
                unselected,
            ]),
            json!({"errors": [{"category": "infrastructure", "code": "APP_UNREACHABLE",
                               "message": "no answer on :3000", "phase": "launch"}]}),
        );
        let parsed = parse_e2e_report_json(&bytes).unwrap();
        assert_eq!(parsed.tests.len(), 7);
        assert_eq!(
            (parsed.passed, parsed.failed, parsed.skipped, parsed.errors),
            (2, 1, 1, 3)
        );
        let case = |index: usize| parsed.tests[index].clone();
        assert_eq!(case(0).status, STATUS_PASSED);
        assert_eq!(
            case(0).message.as_deref(),
            Some("flaky: 1 failed attempt before passing")
        );
        assert_eq!(case(0).duration_ms, Some(7));
        assert_eq!(case(1).status, STATUS_FAILED);
        assert_eq!(case(1).message.as_deref(), Some("TEST_TIMEOUT: 30s"));
        assert_eq!(case(2).status, STATUS_ERROR);
        assert_eq!(
            case(2).message.as_deref(),
            Some("MODEL_UNAVAILABLE (body): the provider refused")
        );
        assert_eq!(case(3).status, STATUS_SKIPPED);
        assert_eq!(case(3).message.as_deref(), Some("interrupted: SIGTERM"));
        assert_eq!(case(4).status, STATUS_ERROR);
        assert_eq!(case(5).name, "suite > careful (repeat #2) [web] [careful]");
        let run_error = case(6);
        assert_eq!(
            (
                run_error.suite.as_str(),
                run_error.name.as_str(),
                run_error.status.as_str()
            ),
            ("run", "APP_UNREACHABLE (launch)", STATUS_ERROR)
        );
        assert_eq!(
            run_error.message.as_deref(),
            Some("infrastructure error APP_UNREACHABLE (launch): no answer on :3000")
        );
    }

    #[test]
    fn e2e_serial_members_read_their_share_of_the_group() {
        let bytes = report(
            json!([
                {"testId": "t1", "titlePath": ["flow", "one"], "file": "f.e2e.ts", "targetId": "web",
                 "serialGroupId": "g1", "status": "passed", "attempts": []},
                {"testId": "t2", "titlePath": ["flow", "two"], "file": "f.e2e.ts", "targetId": "web",
                 "serialGroupId": "g1", "status": "failed", "attempts": []}
            ]),
            json!({"serialGroups": [{"id": "g1", "attempts": [{"status": "failed", "members": [
                {"testId": "t1", "status": "passed", "durationMs": 4},
                {"testId": "t2", "status": "failed", "durationMs": 9,
                 "error": {"category": "test", "code": "ASSERTION_FAILED", "message": "two broke"},
                 "steps": [{"model": {"provider": "openrouter", "model": "vendor/vision-1"}}]}
            ]}]}]}),
        );
        let parsed = parse_e2e_report_json(&bytes).unwrap();
        assert_eq!(parsed.tests[0].duration_ms, Some(4));
        assert_eq!(parsed.tests[1].status, STATUS_FAILED);
        assert_eq!(
            parsed.tests[1].message.as_deref(),
            Some("ASSERTION_FAILED: two broke")
        );
        assert_eq!(
            e2e_report_models(&bytes)
                .unwrap()
                .into_iter()
                .collect::<Vec<_>>(),
            ["vendor/vision-1"]
        );
    }

    #[test]
    fn e2e_models_come_from_agent_steps() {
        let bytes = report(
            json!([result(
                "agent",
                "passed",
                json!([{"status": "passed", "steps": [
                    {"kind": "agent", "model": {"provider": "openrouter.chat", "model": "a/b"}},
                    {"kind": "agent", "model": {"provider": "openrouter.chat", "model": "a/b"}},
                    {"kind": "agent", "model": {"provider": "openai.chat", "model": "c"}},
                    {"kind": "locator"}
                ]}])
            )]),
            json!({}),
        );
        let models: Vec<_> = e2e_report_models(&bytes).unwrap().into_iter().collect();
        assert_eq!(models, ["a/b", "c"]);
    }

    #[test]
    fn e2e_refuses_other_documents() {
        for bad in [
            &b"not json"[..],
            br#"{"schemaVersion":"report-2","run":{"results":[]}}"#,
            br#"{"run":{"results":[]}}"#,
            br#"{"schemaVersion":"report-1"}"#,
            br#"{"schemaVersion":"report-1","run":{"results":[{"titlePath":["x"]}]}}"#,
        ] {
            assert!(
                matches!(
                    parse_e2e_report_json(bad),
                    Err(CheckReportError::Invalid(_))
                ),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    #[test]
    fn large_e2e_reports_are_truncated() {
        let results: Vec<_> = (0..2_100)
            .map(|index| {
                result(
                    &format!("t{index}"),
                    "passed",
                    json!([{"status": "passed"}]),
                )
            })
            .collect();
        let parsed = parse_e2e_report_json(&report(json!(results), json!({}))).unwrap();
        assert_eq!(parsed.tests.len(), MAX_REPORT_TESTS);
        assert_eq!((parsed.passed, parsed.truncated), (2_100, 100));
    }

    #[test]
    fn reports_dispatch_on_their_format() {
        assert_eq!(
            parse_report(REPORT_FORMAT_JUNIT, E2E_JUNIT.as_bytes())
                .unwrap()
                .format,
            REPORT_FORMAT_JUNIT
        );
        assert_eq!(
            parse_report(REPORT_FORMAT_E2E, E2E_REPORT.as_bytes())
                .unwrap()
                .format,
            REPORT_FORMAT_E2E
        );
        assert!(matches!(
            parse_report("tap", b""),
            Err(CheckReportError::UnknownFormat(_))
        ));
    }
}
