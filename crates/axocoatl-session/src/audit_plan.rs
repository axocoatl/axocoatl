//! The audit planner's areas and the workers' and integrator's findings.
//!
//! The planner answers with one fenced JSON block headed `AREAS`:
//! `{"areas": [{"name": "...", "scope": "...", "paths": ["src/**"]}]}` with
//! 2 to 8 areas (the loadout's `min_areas`..`max_areas`). Each area worker
//! and the integrator answer with a `FINDINGS` block of
//! `[{"id", "title", "detail", "severity", "location", "area"}]` and the
//! worker a `NOT_REACHED` list.
//!
//! The parsers read model answers, so they accept the common variations of
//! that shape (Markdown-decorated headings, a block without its fence, an
//! answer that is only the block's JSON, `{"findings": [...]}` objects,
//! `file` + `line` instead of `location`, severity words such as `major`)
//! and refuse what they cannot read with an error the host can quote back.
//! They never drop a finding silently: entries beyond a bound are reported
//! (as a `NOT_REACHED` entry for a worker, as an error for the integrator).
//!
//! Owner: workstream `audit`.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::review_adjudication::{fenced_blocks, headed_block, whole_json, HeadedBlock};
use crate::run_outcome::{Finding, FindingSource, Severity};

pub const AREAS_HEADING: &str = "AREAS";
pub const FINDINGS_HEADING: &str = "FINDINGS";
pub const NOT_REACHED_HEADING: &str = "NOT_REACHED";
/// Fewest and most areas any audit plan may have; a loadout narrows them.
pub const MIN_AREAS: u32 = 2;
pub const MAX_AREAS: u32 = 8;
/// Longest area name: `[a-z][a-z0-9-]{0,31}`.
pub const MAX_AREA_NAME_CHARS: usize = 32;
/// Longest area scope, in bytes.
pub const MAX_SCOPE_BYTES: usize = 4 * 1024;
/// Most paths one area may name, and the longest path, in bytes.
pub const MAX_AREA_PATHS: usize = 32;
pub const MAX_AREA_PATH_BYTES: usize = 256;
/// Most findings one area worker's report may carry; more are reported as
/// not reached.
pub const MAX_AREA_FINDINGS: usize = 200;
/// Most findings the integrator's answer may carry; more is an error.
pub const MAX_INTEGRATED_FINDINGS: usize = 1000;
/// Most `NOT_REACHED` entries one report may carry.
pub const MAX_NOT_REACHED: usize = 64;
const MAX_ID_BYTES: usize = 64;
const MAX_TITLE_BYTES: usize = 512;
const MAX_DETAIL_BYTES: usize = 8 * 1024;
const MAX_LOCATION_BYTES: usize = 512;
const MAX_NOT_REACHED_ITEM_BYTES: usize = 512;
/// Headings of loadout answers; a heading's block ends at the next one.
const KNOWN_HEADINGS: [&str; 5] = [
    AREAS_HEADING,
    FINDINGS_HEADING,
    NOT_REACHED_HEADING,
    "COVERAGE",
    "ADJUDICATIONS",
];

/// One area of an audit plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditArea {
    /// `[a-z][a-z0-9-]{0,31}`, unique; becomes the worker's slot id suffix.
    pub name: String,
    pub scope: String,
    #[serde(default)]
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditPlan {
    pub areas: Vec<AuditArea>,
}

/// One area worker's report.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AreaReport {
    pub findings: Vec<Finding>,
    pub not_reached: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum AuditPlanError {
    #[error("{0}")]
    Invalid(String),
}

fn invalid(message: impl Into<String>) -> AuditPlanError {
    AuditPlanError::Invalid(message.into())
}

/// Read and validate the planner's `AREAS` block: `min`..=`max` areas
/// (narrowed to 2..=8), each with a unique name (normalized to
/// `[a-z][a-z0-9-]{0,31}`), a non-empty scope and optional paths.
pub fn parse_plan(answer: &str, min: u32, max: u32) -> Result<AuditPlan, AuditPlanError> {
    let (min, max) = (min.max(MIN_AREAS), max.min(MAX_AREAS));
    if min > max {
        return Err(invalid(format!(
            "the loadout's area bounds are empty ({min} to {max})"
        )));
    }
    let text = block(answer, AREAS_HEADING, Some("areas"))?.ok_or_else(|| {
        invalid(
            "the answer has no AREAS block: write a line AREAS, then a fenced JSON block \
             {\"areas\": [{\"name\", \"scope\", \"paths\"}]}",
        )
    })?;
    let value: Value = serde_json::from_str(text.trim())
        .map_err(|error| invalid(format!("the AREAS block is not valid JSON: {error}")))?;
    let entries = match &value {
        Value::Object(object) => match object.get("areas") {
            Some(Value::Array(entries)) => entries,
            _ => {
                return Err(invalid(
                    "the AREAS block is not an object {\"areas\": [...]}",
                ))
            }
        },
        Value::Array(entries) => entries,
        _ => {
            return Err(invalid(
                "the AREAS block is not an object {\"areas\": [...]}",
            ))
        }
    };
    if entries.len() < min as usize || entries.len() > max as usize {
        return Err(invalid(format!(
            "the plan has {} area{}; it needs {min} to {max}",
            entries.len(),
            if entries.len() == 1 { "" } else { "s" }
        )));
    }
    let mut names = BTreeSet::new();
    let mut areas = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let number = index + 1;
        let Value::Object(object) = entry else {
            return Err(invalid(format!(
                "area {number} is not an object with name, scope and paths"
            )));
        };
        let raw_name = object
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid(format!("area {number} has no name")))?;
        let name = normalize_area_name(raw_name).ok_or_else(|| {
            invalid(format!(
                "area {number}'s name {raw_name:?} has no letters or digits"
            ))
        })?;
        if !names.insert(name.clone()) {
            return Err(invalid(format!(
                "two areas are named {name}; give every area its own name"
            )));
        }
        let scope = object
            .get("scope")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        if scope.is_empty() {
            return Err(invalid(format!("area {name} has no scope")));
        }
        if scope.len() > MAX_SCOPE_BYTES {
            return Err(invalid(format!(
                "area {name}'s scope is longer than {MAX_SCOPE_BYTES} bytes"
            )));
        }
        let paths = area_paths(object.get("paths"))
            .map_err(|reason| invalid(format!("area {name}'s paths: {reason}")))?;
        areas.push(AuditArea {
            name,
            scope: scope.to_owned(),
            paths,
        });
    }
    Ok(AuditPlan { areas })
}

/// Normalize a planner's area name to `[a-z][a-z0-9-]{0,31}`: lowercase,
/// runs of other characters become one `-`, a leading digit gets `area-`,
/// and the result is cut to 32 characters. `None` when nothing is left.
pub fn normalize_area_name(raw: &str) -> Option<String> {
    let mut name = String::new();
    let mut separator = false;
    for character in raw.trim().chars().flat_map(char::to_lowercase) {
        if character.is_ascii_lowercase() || character.is_ascii_digit() {
            if separator && !name.is_empty() {
                name.push('-');
            }
            separator = false;
            name.push(character);
        } else {
            separator = true;
        }
    }
    if name.is_empty() {
        return None;
    }
    if name.starts_with(|c: char| c.is_ascii_digit()) {
        name.insert_str(0, "area-");
    }
    name.truncate(MAX_AREA_NAME_CHARS);
    while name.ends_with('-') {
        name.pop();
    }
    Some(name)
}

fn area_paths(value: Option<&Value>) -> Result<Vec<String>, String> {
    let items: Vec<&Value> = match value {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(single @ Value::String(_)) => vec![single],
        Some(Value::Array(items)) => items.iter().collect(),
        Some(_) => return Err("write a list of path patterns".into()),
    };
    if items.len() > MAX_AREA_PATHS {
        return Err(format!("name at most {MAX_AREA_PATHS} paths"));
    }
    let mut paths = Vec::with_capacity(items.len());
    for item in items {
        let path = item
            .as_str()
            .ok_or("every path is a string")?
            .trim()
            .to_owned();
        if path.is_empty() {
            continue;
        }
        if path.len() > MAX_AREA_PATH_BYTES || path.chars().any(char::is_control) {
            return Err(format!(
                "a path is longer than {MAX_AREA_PATH_BYTES} bytes or has control characters"
            ));
        }
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    Ok(paths)
}

/// Read one area worker's report: its `FINDINGS` block (required; `[]` when
/// it found nothing) and its `NOT_REACHED` list (absent means none). Each
/// finding is attributed to `area` and its id is prefixed with it.
pub fn parse_area_report(answer: &str, area: &str) -> Result<AreaReport, AuditPlanError> {
    let text = block(answer, FINDINGS_HEADING, Some("findings"))?.ok_or_else(|| {
        invalid(
            "the answer has no FINDINGS block: write a line FINDINGS, then a fenced JSON \
             array of findings ([] when there are none)",
        )
    })?;
    let (entries, embedded_not_reached) = findings_entries(&text)?;
    let mut report = AreaReport::default();
    for (index, entry) in entries.iter().enumerate() {
        if report.findings.len() == MAX_AREA_FINDINGS {
            report.not_reached.push(format!(
                "{} more finding{} beyond the first {MAX_AREA_FINDINGS} (left out of this report)",
                entries.len() - index,
                if entries.len() - index == 1 { "" } else { "s" }
            ));
            break;
        }
        if let Some(mut finding) = finding_from(entry, FindingSource::AuditWorker)? {
            let id = if finding.id.is_empty() {
                (index + 1).to_string()
            } else {
                finding.id
            };
            finding.id = bounded(&format!("{area}-{id}"), MAX_ID_BYTES);
            finding.area = Some(area.to_owned());
            report.findings.push(finding);
        }
    }
    let not_reached = match block(answer, NOT_REACHED_HEADING, None)? {
        Some(text) => parse_not_reached(&text)?,
        None => embedded_not_reached,
    };
    for item in not_reached {
        if report.not_reached.len() >= MAX_NOT_REACHED {
            break;
        }
        report.not_reached.push(item);
    }
    Ok(report)
}

/// Read the integrator's merged findings from its `FINDINGS` block. Ids that
/// are missing or repeated get `A<n>`.
pub fn parse_integrated(answer: &str) -> Result<Vec<Finding>, AuditPlanError> {
    let text = block(answer, FINDINGS_HEADING, Some("findings"))?.ok_or_else(|| {
        invalid(
            "the answer has no FINDINGS block: write a line FINDINGS, then a fenced JSON \
             array of the merged findings ([] when there are none)",
        )
    })?;
    let (entries, _) = findings_entries(&text)?;
    if entries.len() > MAX_INTEGRATED_FINDINGS {
        return Err(invalid(format!(
            "the FINDINGS block has {} findings; at most {MAX_INTEGRATED_FINDINGS} are read",
            entries.len()
        )));
    }
    let mut findings = Vec::with_capacity(entries.len());
    for entry in &entries {
        if let Some(finding) = finding_from(entry, FindingSource::Integrator)? {
            findings.push(finding);
        }
    }
    let mut used = BTreeSet::new();
    let mut unnamed = Vec::new();
    for (index, finding) in findings.iter().enumerate() {
        if finding.id.is_empty() || !used.insert(finding.id.clone()) {
            unnamed.push(index);
        }
    }
    let mut next = 1usize;
    for index in unnamed {
        while used.contains(&format!("A{next}")) {
            next += 1;
        }
        findings[index].id = format!("A{next}");
        used.insert(findings[index].id.clone());
    }
    Ok(findings)
}

/// A block's entries, and the `not_reached` list of a combined object.
fn findings_entries(text: &str) -> Result<(Vec<Value>, Vec<String>), AuditPlanError> {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("none") {
        return Ok((Vec::new(), Vec::new()));
    }
    let value: Value = serde_json::from_str(trimmed)
        .map_err(|error| invalid(format!("the FINDINGS block is not valid JSON: {error}")))?;
    match value {
        Value::Array(entries) => Ok((entries, Vec::new())),
        Value::Object(mut object) => {
            let Some(Value::Array(entries)) = object.remove("findings") else {
                return Err(invalid(
                    "the FINDINGS block is not a JSON array of findings",
                ));
            };
            let not_reached = match object.remove("not_reached") {
                Some(value) => not_reached_items(value)?,
                None => Vec::new(),
            };
            Ok((entries, not_reached))
        }
        _ => Err(invalid(
            "the FINDINGS block is not a JSON array of findings",
        )),
    }
}

/// One finding entry; `None` for an entry with neither title nor detail.
fn finding_from(entry: &Value, source: FindingSource) -> Result<Option<Finding>, AuditPlanError> {
    let object = match entry {
        Value::Object(object) => object,
        Value::String(text) if !text.trim().is_empty() => {
            return Ok(Some(Finding {
                id: String::new(),
                source,
                title: bounded(first_line(text), MAX_TITLE_BYTES),
                detail: bounded(text.trim(), MAX_DETAIL_BYTES),
                severity: None,
                area: None,
                location: None,
                repro: None,
            }))
        }
        Value::String(_) | Value::Null => return Ok(None),
        _ => return Err(invalid("a finding is not a JSON object")),
    };
    let text = |keys: &[&str]| {
        keys.iter()
            .find_map(|key| object.get(*key).and_then(scalar_text))
            .filter(|text| !text.trim().is_empty())
    };
    let mut detail = text(&["detail", "details", "description"]).unwrap_or_default();
    if let Some(evidence) = text(&["evidence"]) {
        if !detail.is_empty() {
            detail.push('\n');
        }
        detail.push_str("Evidence: ");
        detail.push_str(evidence.trim());
    }
    let title = match text(&["title", "summary", "name"]) {
        Some(title) => title,
        None if !detail.trim().is_empty() => first_line(&detail).to_owned(),
        None => return Ok(None),
    };
    let location = text(&["location"]).or_else(|| {
        let file = text(&["file", "path"])?;
        Some(match object.get("line").and_then(scalar_text) {
            Some(line) => format!("{}:{}", file.trim(), line.trim()),
            None => file,
        })
    });
    let area = text(&["area"]).or_else(|| {
        let names: Vec<String> = object
            .get("areas")?
            .as_array()?
            .iter()
            .filter_map(Value::as_str)
            .map(|name| name.trim().to_owned())
            .filter(|name| !name.is_empty())
            .collect();
        (!names.is_empty()).then(|| names.join(", "))
    });
    Ok(Some(Finding {
        id: text(&["id"])
            .map(|id| bounded(id.trim(), MAX_ID_BYTES))
            .unwrap_or_default(),
        source,
        title: bounded(title.trim(), MAX_TITLE_BYTES),
        detail: bounded(detail.trim(), MAX_DETAIL_BYTES),
        severity: text(&["severity", "priority"]).and_then(|word| severity_of(&word)),
        area: area.map(|area| bounded(area.trim(), MAX_TITLE_BYTES)),
        location: location.map(|location| bounded(location.trim(), MAX_LOCATION_BYTES)),
        repro: None,
    }))
}

/// A string or number as text.
fn scalar_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// The severity a model's word means, when it means one.
pub fn severity_of(word: &str) -> Option<Severity> {
    match word.trim().to_ascii_lowercase().as_str() {
        "low" | "minor" | "info" | "informational" | "trivial" | "nit" => Some(Severity::Low),
        "medium" | "moderate" | "med" | "normal" => Some(Severity::Medium),
        "high" | "major" | "severe" | "important" => Some(Severity::High),
        "critical" | "blocker" | "urgent" => Some(Severity::Critical),
        _ => None,
    }
}

fn parse_not_reached(text: &str) -> Result<Vec<String>, AuditPlanError> {
    let trimmed = text.trim();
    if trimmed.is_empty() || is_none_word(trimmed) {
        return Ok(Vec::new());
    }
    if trimmed.starts_with('[') || trimmed.starts_with('{') {
        let value: Value = serde_json::from_str(trimmed).map_err(|error| {
            invalid(format!("the NOT_REACHED block is not valid JSON: {error}"))
        })?;
        return not_reached_items(value);
    }
    Ok(trimmed
        .lines()
        .map(strip_list_marker)
        .filter(|line| !line.is_empty() && !is_none_word(line))
        .take(MAX_NOT_REACHED)
        .map(|line| bounded(line, MAX_NOT_REACHED_ITEM_BYTES))
        .collect())
}

fn not_reached_items(value: Value) -> Result<Vec<String>, AuditPlanError> {
    let entries = match value {
        Value::Array(entries) => entries,
        Value::Object(mut object) => match object.remove("not_reached") {
            Some(Value::Array(entries)) => entries,
            _ => return Err(invalid("the NOT_REACHED block is not a JSON array")),
        },
        Value::Null => return Ok(Vec::new()),
        _ => return Err(invalid("the NOT_REACHED block is not a JSON array")),
    };
    let mut items = Vec::new();
    for entry in entries {
        let item = match &entry {
            Value::String(text) => text.trim().to_owned(),
            Value::Object(object) => {
                let what = ["item", "area", "path", "name", "what", "scope"]
                    .iter()
                    .find_map(|key| object.get(*key).and_then(Value::as_str))
                    .map(str::trim)
                    .unwrap_or_default();
                let why = ["reason", "why", "detail"]
                    .iter()
                    .find_map(|key| object.get(*key).and_then(Value::as_str))
                    .map(str::trim)
                    .unwrap_or_default();
                match (what.is_empty(), why.is_empty()) {
                    (false, false) => format!("{what} ({why})"),
                    (false, true) => what.to_owned(),
                    (true, false) => why.to_owned(),
                    (true, true) => continue,
                }
            }
            Value::Null => continue,
            _ => return Err(invalid("a NOT_REACHED entry is not text")),
        };
        if item.is_empty() || is_none_word(&item) {
            continue;
        }
        if items.len() == MAX_NOT_REACHED {
            break;
        }
        items.push(bounded(&item, MAX_NOT_REACHED_ITEM_BYTES));
    }
    Ok(items)
}

/// A list line without its `-`, `*`, `+` or `1.` marker.
fn strip_list_marker(line: &str) -> &str {
    let line = line.trim();
    let line = line
        .strip_prefix(['-', '*', '+'])
        .map(str::trim_start)
        .unwrap_or(line);
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 {
        if let Some(rest) = line[digits..].strip_prefix(['.', ')']) {
            return rest.trim();
        }
    }
    line
}

fn is_none_word(text: &str) -> bool {
    matches!(
        text.trim()
            .trim_end_matches('.')
            .to_ascii_lowercase()
            .as_str(),
        "none" | "nothing" | "n/a" | "[]"
    )
}

fn first_line(text: &str) -> &str {
    text.trim().lines().next().unwrap_or_default().trim()
}

/// `text` cut to at most `max` bytes on a character boundary, marked when
/// cut.
fn bounded(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    const MARK: &str = " [truncated]";
    let mut end = max.saturating_sub(MARK.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{MARK}", &text[..end])
}

/// The text of `heading`'s block: the block after its last heading, fenced
/// or not ([`headed_block`]). Without the heading, and when `key` names the
/// object the block is (`{"areas": [...]}`), the last fenced block that is
/// such an object, else the whole answer when it is exactly the block's JSON
/// (that object, or a bare array). `Ok(None)` when there is none; an error
/// when the heading has nothing after it.
fn block(answer: &str, heading: &str, key: Option<&str>) -> Result<Option<String>, AuditPlanError> {
    match headed_block(answer, heading, &KNOWN_HEADINGS) {
        HeadedBlock::Found(text) => Ok(Some(text)),
        HeadedBlock::Empty => Err(invalid(format!(
            "the {heading} block is not valid JSON: nothing follows the {heading} heading"
        ))),
        HeadedBlock::Absent => {
            let Some(key) = key else {
                return Ok(None);
            };
            let keyed = |value: &Value| value.get(key).is_some_and(Value::is_array);
            let fenced = fenced_blocks(answer).into_iter().rev().find(|body| {
                serde_json::from_str::<Value>(body.trim()).is_ok_and(|value| keyed(&value))
            });
            if fenced.is_some() {
                return Ok(fenced);
            }
            Ok(whole_json(answer)
                .filter(|value| keyed(value) || value.is_array())
                .map(|_| answer.trim().to_owned()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan_answer(areas: &[(&str, &str)]) -> String {
        let areas: Vec<Value> = areas
            .iter()
            .map(|(name, scope)| serde_json::json!({"name": name, "scope": scope, "paths": ["src/**"]}))
            .collect();
        format!(
            "I read the tree first.\n\n## AREAS\n```json\n{}\n```\n",
            serde_json::to_string_pretty(&serde_json::json!({ "areas": areas })).unwrap()
        )
    }

    #[test]
    fn a_plan_within_bounds_parses_with_normalized_names() {
        let answer = plan_answer(&[
            ("Auth & Sessions", "login, tokens"),
            ("storage_layer", "the database code"),
            ("2FA", "second factor"),
        ]);
        let plan = parse_plan(&answer, 2, 8).unwrap();
        let names: Vec<_> = plan.areas.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["auth-sessions", "storage-layer", "area-2fa"]);
        assert_eq!(plan.areas[0].scope, "login, tokens");
        assert_eq!(plan.areas[0].paths, ["src/**"]);
    }

    #[test]
    fn one_area_and_nine_areas_are_refused() {
        let one = plan_answer(&[("auth", "login")]);
        let error = parse_plan(&one, 2, 8).unwrap_err().to_string();
        assert!(
            error.contains("1 area;") && error.contains("2 to 8"),
            "{error}"
        );
        let nine: Vec<(String, &str)> = (0..9).map(|n| (format!("area{n}"), "x")).collect();
        let nine: Vec<(&str, &str)> = nine.iter().map(|(n, s)| (n.as_str(), *s)).collect();
        let error = parse_plan(&plan_answer(&nine), 2, 8)
            .unwrap_err()
            .to_string();
        assert!(error.contains("9 areas"), "{error}");
        // The loadout's own bounds narrow 2..8, and never widen it.
        let three = plan_answer(&[("a", "x"), ("b", "y"), ("c", "z")]);
        assert!(parse_plan(&three, 2, 2).is_err());
        assert!(parse_plan(&three, 4, 8).is_err());
        assert!(parse_plan(&three, 0, 20).is_ok());
        assert!(parse_plan(&plan_answer(&[("a", "x")]), 0, 20).is_err());
    }

    #[test]
    fn duplicate_names_are_refused_even_after_normalization() {
        let answer = plan_answer(&[("auth", "login"), ("Auth", "tokens")]);
        let error = parse_plan(&answer, 2, 8).unwrap_err().to_string();
        assert!(error.contains("two areas are named auth"), "{error}");
    }

    #[test]
    fn names_without_letters_scopes_and_shapes_are_checked() {
        let error = parse_plan(&plan_answer(&[("auth", "x"), ("!!!", "y")]), 2, 8)
            .unwrap_err()
            .to_string();
        assert!(error.contains("no letters"), "{error}");
        let error = parse_plan(&plan_answer(&[("auth", "x"), ("db", "  ")]), 2, 8)
            .unwrap_err()
            .to_string();
        assert!(error.contains("area db has no scope"), "{error}");
        let error = parse_plan("AREAS\n```json\n{\"areas\": [1, 2]}\n```", 2, 8)
            .unwrap_err()
            .to_string();
        assert!(error.contains("not an object"), "{error}");
        let error = parse_plan("AREAS\n```json\n{\"areas\": [\n```", 2, 8)
            .unwrap_err()
            .to_string();
        assert!(error.contains("not valid JSON"), "{error}");
        let error = parse_plan("I could not decide.", 2, 8)
            .unwrap_err()
            .to_string();
        assert!(error.contains("no AREAS block"), "{error}");
        let error = parse_plan("AREAS: see below", 2, 8)
            .unwrap_err()
            .to_string();
        assert!(error.contains("not valid JSON"), "{error}");
        let error = parse_plan("**AREAS:**\nsee below", 2, 8)
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with("the AREAS block is not valid JSON: "),
            "{error}"
        );
        let error = parse_plan("I split it up.\n\n## AREAS\n", 2, 8)
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "the AREAS block is not valid JSON: nothing follows the AREAS heading"
        );
    }

    /// Answers of the 1.3.0 smoke test's audit runs, exactly as recorded:
    /// the planner and the integrator answered with only the JSON they were
    /// asked for, without a heading or a fence, and both were refused ("no
    /// AREAS block", "no FINDINGS block"); run 2 audited nothing.
    const AUDIT_RUN_1_PLAN: &str =
        include_str!("../tests/fixtures/answers/audit-run1-plan-first.txt");
    const AUDIT_RUN_2_PLAN: &str = include_str!("../tests/fixtures/answers/audit-run2-plan.txt");
    const AUDIT_RUN_1_INTEGRATE: &str =
        include_str!("../tests/fixtures/answers/audit-run1-integrate.txt");

    #[test]
    fn the_recorded_json_only_plans_are_read() {
        for answer in [AUDIT_RUN_1_PLAN, AUDIT_RUN_2_PLAN] {
            assert!(answer.starts_with("{\"areas\": [{\"name\": \"auth\""));
            let plan = parse_plan(answer, 2, 8).unwrap();
            let names: Vec<_> = plan.areas.iter().map(|a| a.name.as_str()).collect();
            assert_eq!(names, ["auth", "billing", "ingest", "notify"]);
        }
        let plan = parse_plan(AUDIT_RUN_2_PLAN, 2, 8).unwrap();
        assert_eq!(plan.areas[3].paths, ["notify/**/*"]);
        // The plan's bounds still apply to a JSON-only answer.
        let error = parse_plan(AUDIT_RUN_1_PLAN, 2, 3).unwrap_err().to_string();
        assert!(error.contains("4 areas"), "{error}");
    }

    #[test]
    fn the_recorded_json_only_integration_is_read() {
        assert!(AUDIT_RUN_1_INTEGRATE.starts_with("{\n  \"findings\": [\n"));
        let findings = parse_integrated(AUDIT_RUN_1_INTEGRATE).unwrap();
        let ids: Vec<_> = findings.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "auth-F1",
                "billing-F1",
                "ingest-F1",
                "notify-F1",
                "notify-F2",
                "notify-F3"
            ]
        );
        assert!(findings
            .iter()
            .all(|f| f.source == FindingSource::Integrator));
        assert_eq!(findings[2].location.as_deref(), Some("ingest/feed.go:29"));
        assert_eq!(findings[3].area.as_deref(), Some("notify"));
        assert_eq!(findings[3].severity, Some(Severity::High));
    }

    #[test]
    fn unfenced_blocks_after_headings_are_read() {
        // A worker's report with both blocks unfenced.
        let report = parse_area_report(
            "I read src/db.\n\nFINDINGS\n[{\"title\": \"Unchecked unwrap\", \"location\": \
             \"src/db.rs:3\"}]\n\nNOT_REACHED\n[\"migrations\"]\n",
            "db",
        )
        .unwrap();
        assert_eq!(report.findings[0].id, "db-1");
        assert_eq!(report.not_reached, ["migrations"]);
        // A JSON value on the heading's line that continues on the next ones.
        let report = parse_area_report(
            "FINDINGS: [\n  {\"title\": \"a\"},\n  {\"title\": \"b\"}\n]\nNOT_REACHED: none",
            "db",
        )
        .unwrap();
        assert_eq!(report.findings.len(), 2);
        assert!(report.not_reached.is_empty());
        // An unfenced list after NOT_REACHED ends at its paragraph.
        let report = parse_area_report(
            "FINDINGS: []\nNOT_REACHED\n- the cache layer\n- vendored code\n\nThat is all.",
            "db",
        )
        .unwrap();
        assert_eq!(report.not_reached, ["the cache layer", "vendored code"]);
        // The integrator's object under a heading, without a fence.
        let findings =
            parse_integrated("Merged.\n\n## FINDINGS\n{\"findings\": [{\"title\": \"t\"}]}\n")
                .unwrap();
        assert_eq!(findings[0].id, "A1");
        // A worker answer that is only a bare array of findings.
        let report = parse_area_report("[{\"title\": \"t\"}]", "db").unwrap();
        assert_eq!(report.findings.len(), 1);
        assert!(report.not_reached.is_empty());
        // Unfenced JSON that does not parse is reported as such.
        let error = parse_area_report("FINDINGS\n[{\"title\": }]", "db")
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with("the FINDINGS block is not valid JSON: "),
            "{error}"
        );
    }

    #[test]
    fn long_names_are_cut_and_paths_bounded() {
        let long = "a".repeat(40);
        let answer = plan_answer(&[(&long, "x"), ("b", "y")]);
        let plan = parse_plan(&answer, 2, 8).unwrap();
        assert_eq!(plan.areas[0].name.len(), MAX_AREA_NAME_CHARS);
        let paths: Vec<String> = (0..40).map(|n| format!("src/{n}")).collect();
        let answer = format!(
            "AREAS\n```\n{}\n```",
            serde_json::json!({"areas": [
                {"name": "a", "scope": "x", "paths": paths},
                {"name": "b", "scope": "y", "paths": "lib/**"}
            ]})
        );
        let error = parse_plan(&answer, 2, 8).unwrap_err().to_string();
        assert!(error.contains("at most 32 paths"), "{error}");
        let answer = answer.replace(
            &serde_json::to_string(&paths).unwrap(),
            "[\"src/**\", \"src/**\", \"\"]",
        );
        let plan = parse_plan(&answer, 2, 8).unwrap();
        assert_eq!(plan.areas[0].paths, ["src/**"]);
        assert_eq!(plan.areas[1].paths, ["lib/**"]);
    }

    #[test]
    fn the_last_areas_block_wins_and_a_bare_block_is_accepted() {
        let draft = plan_answer(&[("a", "x")]);
        let fixed = plan_answer(&[("a", "x"), ("b", "y")]);
        let plan = parse_plan(&format!("{draft}\nOn reflection:\n{fixed}"), 2, 8).unwrap();
        assert_eq!(plan.areas.len(), 2);
        let bare = "```json\n{\"areas\":[{\"name\":\"a\",\"scope\":\"x\"},{\"name\":\"b\",\"scope\":\"y\"}]}\n```";
        let plan = parse_plan(bare, 2, 8).unwrap();
        assert_eq!(plan.areas[1].paths, Vec::<String>::new());
        // A heading-like line inside a fenced block is not a heading.
        let quoted = format!("```text\nAREAS\n```\n{fixed}");
        assert_eq!(parse_plan(&quoted, 2, 8).unwrap().areas.len(), 2);
    }

    #[test]
    fn an_area_report_reads_findings_and_not_reached() {
        let answer = r#"I audited src/auth.

**FINDINGS**
```json
[
  {"id": "F1", "title": "Token compared with ==", "detail": "timing leak", "severity": "High", "location": "src/auth.rs:42"},
  {"title": "Session never expires", "severity": "major", "file": "src/session.rs", "line": 7},
  {"detail": "Password hash uses MD5\nsee hash()", "severity": "whatever"},
  {}
]
```

### Not reached
```json
["src/auth/oauth.rs", {"item": "migrations", "reason": "budget"}]
```
"#;
        let report = parse_area_report(answer, "auth").unwrap();
        assert_eq!(report.findings.len(), 3);
        let first = &report.findings[0];
        assert_eq!(first.id, "auth-F1");
        assert_eq!(first.source, FindingSource::AuditWorker);
        assert_eq!(first.area.as_deref(), Some("auth"));
        assert_eq!(first.severity, Some(Severity::High));
        assert_eq!(first.location.as_deref(), Some("src/auth.rs:42"));
        assert_eq!(report.findings[1].id, "auth-2");
        assert_eq!(report.findings[1].severity, Some(Severity::High));
        assert_eq!(
            report.findings[1].location.as_deref(),
            Some("src/session.rs:7")
        );
        assert_eq!(report.findings[2].title, "Password hash uses MD5");
        assert_eq!(report.findings[2].severity, None);
        assert_eq!(
            report.not_reached,
            ["src/auth/oauth.rs", "migrations (budget)"]
        );
    }

    #[test]
    fn an_empty_report_and_list_forms_are_read() {
        let report = parse_area_report("FINDINGS: []\nNOT_REACHED: none", "db").unwrap();
        assert!(report.findings.is_empty() && report.not_reached.is_empty());
        let report = parse_area_report(
            "FINDINGS\n```json\n[]\n```\nNOT_REACHED\n```\n- the cache layer\n- 2. vendored code\n```",
            "db",
        )
        .unwrap();
        assert_eq!(report.not_reached, ["the cache layer", "vendored code"]);
        let report = parse_area_report(
            "FINDINGS\n```json\n{\"findings\": [\"Unchecked unwrap in load()\"], \"not_reached\": [\"tests\"]}\n```",
            "db",
        )
        .unwrap();
        assert_eq!(report.findings[0].title, "Unchecked unwrap in load()");
        assert_eq!(report.not_reached, ["tests"]);
    }

    #[test]
    fn an_unreadable_report_is_an_error() {
        let error = parse_area_report("All good, nothing found.", "db")
            .unwrap_err()
            .to_string();
        assert!(error.contains("no FINDINGS block"), "{error}");
        let error = parse_area_report("FINDINGS\n```json\n[{\"title\": \n```", "db")
            .unwrap_err()
            .to_string();
        assert!(error.contains("not valid JSON"), "{error}");
        let error = parse_area_report("FINDINGS\n```json\n[1]\n```", "db")
            .unwrap_err()
            .to_string();
        assert!(error.contains("not a JSON object"), "{error}");
        let error = parse_area_report(
            "FINDINGS\n```json\n[]\n```\nNOT_REACHED\n```json\n[oops\n```",
            "db",
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("NOT_REACHED block is not valid JSON"),
            "{error}"
        );
    }

    #[test]
    fn findings_beyond_the_bound_are_reported_as_not_reached() {
        let entries: Vec<Value> = (0..MAX_AREA_FINDINGS + 3)
            .map(|n| serde_json::json!({"title": format!("defect {n}")}))
            .collect();
        let answer = format!(
            "FINDINGS\n```json\n{}\n```",
            serde_json::to_string(&entries).unwrap()
        );
        let report = parse_area_report(&answer, "db").unwrap();
        assert_eq!(report.findings.len(), MAX_AREA_FINDINGS);
        assert_eq!(report.not_reached.len(), 1);
        assert!(
            report.not_reached[0].starts_with("3 more findings"),
            "{:?}",
            report.not_reached
        );
        let long = "x".repeat(MAX_DETAIL_BYTES * 2);
        let answer = format!(
            "FINDINGS\n```json\n[{}]\n```",
            serde_json::json!({"title": "t", "detail": long})
        );
        let report = parse_area_report(&answer, "db").unwrap();
        assert!(report.findings[0].detail.len() <= MAX_DETAIL_BYTES);
        assert!(report.findings[0].detail.ends_with("[truncated]"));
    }

    #[test]
    fn integrated_findings_keep_areas_and_get_unique_ids() {
        let answer = r#"Merged 4 reports.
FINDINGS
```json
[
  {"id": "A1", "title": "Token compared with ==", "severity": "high", "location": "src/auth.rs:42", "area": "auth"},
  {"id": "A1", "title": "Duplicate id", "areas": ["db", "cache"]},
  {"title": "No id", "severity": "critical"},
  {"title": "", "detail": ""}
]
```"#;
        let findings = parse_integrated(answer).unwrap();
        let ids: Vec<_> = findings.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(ids, ["A1", "A2", "A3"]);
        assert!(findings
            .iter()
            .all(|f| f.source == FindingSource::Integrator));
        assert_eq!(findings[0].area.as_deref(), Some("auth"));
        assert_eq!(findings[1].area.as_deref(), Some("db, cache"));
        assert_eq!(findings[2].severity, Some(Severity::Critical));
        assert!(parse_integrated("FINDINGS: none").unwrap().is_empty());
        assert!(parse_integrated("I merged them all.").is_err());
        let many: Vec<Value> = (0..MAX_INTEGRATED_FINDINGS + 1)
            .map(|n| serde_json::json!({"title": format!("d{n}")}))
            .collect();
        let error = parse_integrated(&format!(
            "FINDINGS\n```json\n{}\n```",
            serde_json::to_string(&many).unwrap()
        ))
        .unwrap_err()
        .to_string();
        assert!(error.contains("at most 1000"), "{error}");
    }

    #[test]
    fn severity_words_map_to_severities() {
        assert_eq!(severity_of("Minor"), Some(Severity::Low));
        assert_eq!(severity_of("moderate"), Some(Severity::Medium));
        assert_eq!(severity_of("SEVERE"), Some(Severity::High));
        assert_eq!(severity_of("blocker"), Some(Severity::Critical));
        assert_eq!(severity_of("spicy"), None);
    }

    #[test]
    fn normalization_rules() {
        assert_eq!(
            normalize_area_name(" API / Routes ").as_deref(),
            Some("api-routes")
        );
        assert_eq!(normalize_area_name("--x--").as_deref(), Some("x"));
        assert_eq!(normalize_area_name("Ünïcode").as_deref(), Some("n-code"));
        assert_eq!(normalize_area_name("42").as_deref(), Some("area-42"));
        assert_eq!(normalize_area_name("___"), None);
        let cut = normalize_area_name(&format!("{}-{}", "a".repeat(30), "bbb")).unwrap();
        assert_eq!(cut, format!("{}-b", "a".repeat(30)));
        let cut = normalize_area_name(&format!("{} {}", "a".repeat(31), "b")).unwrap();
        assert_eq!(cut, "a".repeat(31));
    }
}
