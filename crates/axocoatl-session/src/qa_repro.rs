//! The QA explorer's report (findings with reproductions, and coverage) and
//! the classification of each reproduction.
//!
//! The explorer's final answer carries two fenced JSON blocks headed
//! `FINDINGS` and `COVERAGE` (shapes in the spec, "qa"). The host re-runs
//! each reproduction with `browser_check` against the build under test and,
//! when configured, the reference build, and classifies it with
//! [`classify`].
//!
//! Owner: workstream `review-qa`.

use serde::{Deserialize, Serialize};

use crate::review_adjudication::fenced_blocks_after;
use crate::run_outcome::{ReproClassification, ReproRun, Severity};

pub const FINDINGS_HEADING: &str = "FINDINGS";
pub const COVERAGE_HEADING: &str = "COVERAGE";
/// Most findings one explorer report may carry.
pub const MAX_QA_FINDINGS: usize = 200;
/// Most coverage entries one explorer report may carry.
pub const MAX_COVERAGE_ENTRIES: usize = 200;
/// Longest finding or area id kept, in bytes.
pub const MAX_ID_BYTES: usize = 64;
/// Longest title or area name kept, in bytes.
pub const MAX_TITLE_BYTES: usize = 300;
/// Longest `expected`, `actual` or coverage reason kept, in bytes.
pub const MAX_TEXT_BYTES: usize = 4 * 1024;

/// `covered`: the explorer exercised the area.
pub const COVERED: &str = "covered";
/// `not_reached`: the explorer never got to the area.
pub const NOT_REACHED: &str = "not_reached";
/// `blocked`: the explorer tried and could not proceed.
pub const BLOCKED: &str = "blocked";

/// One finding as the explorer reported it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportedFinding {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub area: Option<String>,
    #[serde(default)]
    pub severity: Option<Severity>,
    pub expected: String,
    pub actual: String,
    /// Repository path of the reproduction, under the loadout's `repro_dir`.
    pub repro: Option<String>,
}

/// One area's coverage as the explorer reported it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoverageEntry {
    pub area: String,
    /// `covered`, `not_reached` or `blocked` (normalized: lower case, `_`
    /// for spaces and dashes). Any other value is kept as written and is
    /// never read as covered.
    pub status: String,
    #[serde(default)]
    pub reason: Option<String>,
}

impl CoverageEntry {
    /// Whether the explorer reported this area exercised.
    pub fn is_covered(&self) -> bool {
        self.status == COVERED
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExplorerReport {
    pub findings: Vec<ReportedFinding>,
    pub coverage: Vec<CoverageEntry>,
    /// Whether the answer had a `FINDINGS` block.
    pub findings_block: bool,
    /// Whether the answer had a `COVERAGE` block.
    pub coverage_block: bool,
    /// Entries the host could not read or had to leave out, in words. Each
    /// is reported as not covered; none is dropped silently.
    pub problems: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QaReportError {
    /// A `FINDINGS` or `COVERAGE` block that is not a JSON array.
    #[error("qa report: {0}")]
    Invalid(String),
}

/// At most `max` bytes of `text`, trimmed and cut on a character boundary.
fn clip(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// The array in the last `heading` block, `None` without one.
fn block_array(
    answer: &str,
    heading: &str,
) -> Result<Option<Vec<serde_json::Value>>, QaReportError> {
    let Some(block) = fenced_blocks_after(answer, heading).pop() else {
        return Ok(None);
    };
    let value: serde_json::Value = serde_json::from_str(block.trim()).map_err(|error| {
        QaReportError::Invalid(format!(
            "the {heading} block is not JSON: {}",
            clip(&error.to_string(), 200)
        ))
    })?;
    match value {
        serde_json::Value::Array(entries) => Ok(Some(entries)),
        _ => Err(QaReportError::Invalid(format!(
            "the {heading} block is not a JSON array"
        ))),
    }
}

fn text_field(entry: &serde_json::Value, key: &str) -> Option<String> {
    match entry.get(key)? {
        serde_json::Value::String(text) if !text.trim().is_empty() => Some(text.clone()),
        serde_json::Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

fn severity(entry: &serde_json::Value) -> Option<Severity> {
    match entry
        .get("severity")?
        .as_str()?
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "low" | "minor" => Some(Severity::Low),
        "medium" | "moderate" => Some(Severity::Medium),
        "high" | "major" => Some(Severity::High),
        "critical" | "blocker" => Some(Severity::Critical),
        _ => None,
    }
}

/// `not reached`, `Not-Reached` and `not_reached` are one status.
fn normalize_status(status: &str) -> String {
    status.trim().to_ascii_lowercase().replace([' ', '-'], "_")
}

/// Read the explorer's `FINDINGS` and `COVERAGE` blocks: the last fenced
/// block after each heading, each a JSON array. A block that is not a JSON
/// array is an error (the report cannot be read). An entry the host cannot
/// read (a finding without an id, title, expected or actual; a coverage
/// entry without an area or status), entries past the bounds, and repeated
/// finding ids are listed in [`ExplorerReport::problems`]; a repeated id gets
/// a `-2`, `-3`, ... suffix so both findings stay. Unknown fields are
/// ignored; long text is bounded.
pub fn parse_explorer_report(answer: &str) -> Result<ExplorerReport, QaReportError> {
    let mut report = ExplorerReport::default();
    if let Some(entries) = block_array(answer, FINDINGS_HEADING)? {
        report.findings_block = true;
        let total = entries.len();
        for (index, entry) in entries.into_iter().enumerate() {
            if report.findings.len() >= MAX_QA_FINDINGS {
                report.problems.push(format!(
                    "{} more findings were left out: a report carries at most {MAX_QA_FINDINGS}",
                    total - index
                ));
                break;
            }
            let fields = (
                text_field(&entry, "id"),
                text_field(&entry, "title"),
                text_field(&entry, "expected"),
                text_field(&entry, "actual"),
            );
            let (Some(id), Some(title), Some(expected), Some(actual)) = fields else {
                report.problems.push(format!(
                    "finding {} of the FINDINGS block has no id, title, expected or actual",
                    index + 1
                ));
                continue;
            };
            let mut id = clip(&id, MAX_ID_BYTES);
            if report.findings.iter().any(|finding| finding.id == id) {
                let base = id.clone();
                let mut suffix = 2;
                while report
                    .findings
                    .iter()
                    .any(|finding| finding.id == format!("{base}-{suffix}"))
                {
                    suffix += 1;
                }
                id = format!("{base}-{suffix}");
                report.problems.push(format!(
                    "the FINDINGS block uses the id {base} more than once; the later finding is {id}"
                ));
            }
            report.findings.push(ReportedFinding {
                id,
                title: clip(&title, MAX_TITLE_BYTES),
                area: text_field(&entry, "area").map(|area| clip(&area, MAX_TITLE_BYTES)),
                severity: severity(&entry),
                expected: clip(&expected, MAX_TEXT_BYTES),
                actual: clip(&actual, MAX_TEXT_BYTES),
                repro: text_field(&entry, "repro").map(|path| clip(&path, 1024)),
            });
        }
    }
    if let Some(entries) = block_array(answer, COVERAGE_HEADING)? {
        report.coverage_block = true;
        let total = entries.len();
        for (index, entry) in entries.into_iter().enumerate() {
            if report.coverage.len() >= MAX_COVERAGE_ENTRIES {
                report.problems.push(format!(
                    "{} more coverage entries were left out: a report carries at most {MAX_COVERAGE_ENTRIES}",
                    total - index
                ));
                break;
            }
            let (Some(area), Some(status)) =
                (text_field(&entry, "area"), text_field(&entry, "status"))
            else {
                report.problems.push(format!(
                    "coverage entry {} of the COVERAGE block has no area or status",
                    index + 1
                ));
                continue;
            };
            report.coverage.push(CoverageEntry {
                area: clip(&area, MAX_TITLE_BYTES),
                status: clip(&normalize_status(&status), 64),
                reason: text_field(&entry, "reason").map(|reason| clip(&reason, MAX_TEXT_BYTES)),
            });
        }
    }
    Ok(report)
}

/// Classify a reproduction from its run on the build under test and, when
/// configured, on the reference build. This is the contract the spec fixes.
pub fn classify(target: &ReproRun, reference: Option<&ReproRun>) -> ReproClassification {
    match (target.status.as_str(), reference.map(|r| r.status.as_str())) {
        ("error", _) | ("failed", Some("error")) => ReproClassification::ReproError,
        ("passed", _) => ReproClassification::NotReproduced,
        ("failed", None) => ReproClassification::Reproduced,
        ("failed", Some("passed")) => ReproClassification::Confirmed,
        ("failed", Some("failed")) => ReproClassification::FailsOnCleanBuild,
        _ => ReproClassification::ReproError,
    }
}

/// How a classification reads in the run's output and record.
pub fn classification_label(classification: ReproClassification) -> &'static str {
    match classification {
        ReproClassification::Confirmed => "confirmed",
        ReproClassification::FailsOnCleanBuild => "fails on clean build",
        ReproClassification::Reproduced => "reproduced (no reference build)",
        ReproClassification::NotReproduced => "not reproduced",
        ReproClassification::ReproError => "reproduction error",
        ReproClassification::Missing => "no reproduction",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(status: &str) -> ReproRun {
        ReproRun {
            base_url: "http://localhost:3000".into(),
            status: status.into(),
            first_error: None,
        }
    }

    #[test]
    fn classification_follows_the_reference_rule() {
        use ReproClassification::*;
        assert_eq!(classify(&run("failed"), Some(&run("passed"))), Confirmed);
        assert_eq!(
            classify(&run("failed"), Some(&run("failed"))),
            FailsOnCleanBuild
        );
        assert_eq!(classify(&run("failed"), None), Reproduced);
        assert_eq!(
            classify(&run("passed"), Some(&run("passed"))),
            NotReproduced
        );
        assert_eq!(classify(&run("error"), Some(&run("passed"))), ReproError);
        assert_eq!(classify(&run("failed"), Some(&run("error"))), ReproError);
        assert_eq!(
            classification_label(FailsOnCleanBuild),
            "fails on clean build"
        );
    }

    const ANSWER: &str = r#"I explored checkout, search and gift cards.

## FINDINGS
```json
[
  {"id": "B1", "title": "Total ignores the coupon", "area": "checkout", "severity": "High",
   "expected": "10% off", "actual": "full price", "repro": "axocoatl-qa/b1.spec.ts",
   "steps": ["ignored extra field"]},
  {"id": "B2", "title": "Search is case sensitive", "expected": "matches", "actual": "no results",
   "repro": null},
  {"id": "B1", "title": "Duplicate id", "expected": "a", "actual": "b", "repro": "axocoatl-qa/b1b.spec.ts"},
  {"title": "No id", "expected": "a", "actual": "b"}
]
```

**COVERAGE:**
```json
[
  {"area": "checkout", "status": "covered"},
  {"area": "search", "status": "Not reached", "reason": "ran out of steps"},
  {"area": "gift cards", "status": "blocked", "reason": "classifier stop"},
  {"area": "admin", "status": "partially"},
  {"status": "covered"}
]
```
"#;

    #[test]
    fn a_report_is_read_from_its_blocks() {
        let report = parse_explorer_report(ANSWER).unwrap();
        assert!(report.findings_block && report.coverage_block);
        let ids: Vec<_> = report.findings.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(ids, ["B1", "B2", "B1-2"]);
        let first = &report.findings[0];
        assert_eq!(first.title, "Total ignores the coupon");
        assert_eq!(first.area.as_deref(), Some("checkout"));
        assert_eq!(first.severity, Some(Severity::High));
        assert_eq!(first.repro.as_deref(), Some("axocoatl-qa/b1.spec.ts"));
        assert_eq!(report.findings[1].repro, None);
        let statuses: Vec<_> = report
            .coverage
            .iter()
            .map(|c| (c.area.as_str(), c.status.as_str(), c.is_covered()))
            .collect();
        assert_eq!(
            statuses,
            [
                ("checkout", "covered", true),
                ("search", "not_reached", false),
                ("gift cards", "blocked", false),
                ("admin", "partially", false),
            ]
        );
        assert_eq!(
            report.coverage[1].reason.as_deref(),
            Some("ran out of steps")
        );
        assert_eq!(report.problems.len(), 3, "{:?}", report.problems);
        assert!(report.problems[0].contains("B1 more than once"));
        assert!(report.problems[1].contains("finding 4"));
        assert!(report.problems[2].contains("coverage entry 5"));
    }

    #[test]
    fn absent_blocks_are_reported_as_absent() {
        let report = parse_explorer_report("I looked around and found nothing.").unwrap();
        assert_eq!(report, ExplorerReport::default());
        let report =
            parse_explorer_report("FINDINGS\n```json\n[]\n```\nNo coverage block.").unwrap();
        assert!(report.findings_block && !report.coverage_block);
        assert!(report.findings.is_empty());
    }

    #[test]
    fn an_unreadable_block_is_an_error() {
        assert!(matches!(
            parse_explorer_report("FINDINGS\n```json\n[{\"id\": \"B1\",]\n```"),
            Err(QaReportError::Invalid(message)) if message.contains("FINDINGS")
        ));
        assert!(matches!(
            parse_explorer_report("COVERAGE\n```json\n{\"area\": \"x\"}\n```"),
            Err(QaReportError::Invalid(message)) if message.contains("COVERAGE")
        ));
    }

    #[test]
    fn reports_are_bounded() {
        let findings: Vec<_> = (0..MAX_QA_FINDINGS + 5)
            .map(|n| {
                serde_json::json!({"id": format!("B{n}"), "title": "t".repeat(400),
                    "expected": "e", "actual": "a"})
            })
            .collect();
        let answer = format!(
            "FINDINGS\n```json\n{}\n```",
            serde_json::to_string(&findings).unwrap()
        );
        let report = parse_explorer_report(&answer).unwrap();
        assert_eq!(report.findings.len(), MAX_QA_FINDINGS);
        assert!(report.findings[0].title.len() <= MAX_TITLE_BYTES + 3);
        assert_eq!(report.problems.len(), 1);
        assert!(report.problems[0].starts_with("5 more findings"));
    }
}
