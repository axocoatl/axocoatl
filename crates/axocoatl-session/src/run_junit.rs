//! JUnit XML of a run's Outcome, for CI (`axocoatl run --junit <file>` and
//! `GET /api/runs/{run_id}/junit`). The shape is fixed by the spec
//! ("JUnit shape"): one `<testsuites name="axocoatl">` with the suites
//! `checks`, one `check:<name>` per check report, `review`, `adjudications`,
//! `findings`, `coverage` and `run` (the verdict, so a CI view of the file
//! alone never shows a run that needs attention as green).
//!
//! Rules: a check `failed`/`timed_out` is a `<failure>`, `not_run` /
//! `unavailable` an `<error>`, and a check's reason that its status does not
//! carry (a passed check whose report could not be read) is in its
//! `<system-out>`; a review that did not pass is a `<failure>`;
//! a `missing` adjudication is a `<failure type="missing">`, `accept` and
//! `reject` pass with the reason in `<system-out>`; findings `confirmed` and
//! `reproduced` are `<failure>`s when the loadout fails on findings (else
//! `<system-out>`), `fails_on_clean_build` and `not_reproduced` are
//! `<skipped>`, `repro_error` and `missing` are `<error>`s; every not-covered
//! entry is a `<failure type="not_covered">` whose message is
//! [`NotCovered::reason`](crate::run_outcome::NotCovered::reason), never
//! skipped. Text is XML-escaped with control characters other than tab and
//! newline removed, each message is at most 4 KiB, and the document at most
//! 8 MiB: cases past that are summarized in one `truncated` case.
//!
//! Owner: workstream `core`.

use std::fmt::Write as _;

use crate::review_adjudication::round_findings;
use crate::run_outcome::{
    AdjudicationDecision, CheckState, Finding, FindingSource, ReproClassification,
    ReviewVerdictKind, RunOutcome, RunVerdict,
};

/// Largest JUnit document produced, in bytes; test cases past it are
/// summarized in one `truncated` test case.
pub const MAX_JUNIT_BYTES: usize = 8 * 1024 * 1024;
/// Longest message or body text, in bytes.
pub const MAX_JUNIT_MESSAGE_BYTES: usize = 4 * 1024;
/// Room kept for the closing elements and the `truncated` case.
const JUNIT_RESERVE_BYTES: usize = 16 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum JunitError {
    #[error("JUnit: not implemented: {0}")]
    NotImplemented(&'static str),
}

/// What one test case reports besides passing.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Status {
    Passed,
    Failure { kind: String, message: String },
    Error { kind: String, message: String },
    Skipped { message: String },
}

#[derive(Debug, Clone)]
struct Case {
    classname: String,
    name: String,
    time_ms: Option<u64>,
    status: Status,
    body: Option<String>,
    system_out: Option<String>,
}

impl Case {
    fn new(classname: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            classname: classname.into(),
            name: name.into(),
            time_ms: None,
            status: Status::Passed,
            body: None,
            system_out: None,
        }
    }

    fn status(mut self, status: Status) -> Self {
        self.status = status;
        self
    }

    fn out(mut self, text: impl Into<String>) -> Self {
        self.system_out = Some(text.into());
        self
    }

    fn render(&self) -> String {
        let mut xml = String::new();
        let _ = write!(
            xml,
            "    <testcase classname=\"{}\" name=\"{}\"",
            attr(&self.classname),
            attr(&self.name)
        );
        if let Some(ms) = self.time_ms {
            let _ = write!(xml, " time=\"{}\"", seconds(ms));
        }
        let mut children = String::new();
        match &self.status {
            Status::Passed => {}
            Status::Failure { kind, message } => element(
                &mut children,
                "failure",
                kind,
                message,
                self.body.as_deref(),
            ),
            Status::Error { kind, message } => {
                element(&mut children, "error", kind, message, self.body.as_deref())
            }
            Status::Skipped { message } => {
                let _ = writeln!(
                    children,
                    "      <skipped message=\"{}\"/>",
                    attr(&bounded(message))
                );
            }
        }
        if let Some(out) = &self.system_out {
            let _ = writeln!(
                children,
                "      <system-out>{}</system-out>",
                text(&bounded(out))
            );
        }
        if children.is_empty() {
            xml.push_str("/>\n");
        } else {
            xml.push_str(">\n");
            xml.push_str(&children);
            xml.push_str("    </testcase>\n");
        }
        xml
    }
}

fn element(out: &mut String, tag: &str, kind: &str, message: &str, body: Option<&str>) {
    let _ = write!(
        out,
        "      <{tag} type=\"{}\" message=\"{}\"",
        attr(kind),
        attr(&bounded(message))
    );
    match body.filter(|body| !body.is_empty()) {
        Some(body) => {
            let _ = writeln!(out, ">{}</{tag}>", text(&bounded(body)));
        }
        None => out.push_str("/>\n"),
    }
}

struct Suite {
    name: String,
    cases: Vec<Case>,
}

impl Suite {
    fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            cases: Vec::new(),
        }
    }
}

#[derive(Default, Clone, Copy)]
struct Counts {
    tests: usize,
    failures: usize,
    errors: usize,
    skipped: usize,
}

impl Counts {
    fn add(&mut self, status: &Status) {
        self.tests += 1;
        match status {
            Status::Passed => {}
            Status::Failure { .. } => self.failures += 1,
            Status::Error { .. } => self.errors += 1,
            Status::Skipped { .. } => self.skipped += 1,
        }
    }

    fn attrs(&self) -> String {
        format!(
            "tests=\"{}\" failures=\"{}\" errors=\"{}\" skipped=\"{}\"",
            self.tests, self.failures, self.errors, self.skipped
        )
    }
}

fn seconds(ms: u64) -> String {
    format!("{}.{:03}", ms / 1000, ms % 1000)
}

/// Keep tab and newline; drop other control characters and the two
/// non-characters XML 1.0 refuses.
fn clean(input: &str) -> String {
    input
        .chars()
        .filter(|c| {
            matches!(c, '\t' | '\n') || !(c.is_control() || *c == '\u{FFFE}' || *c == '\u{FFFF}')
        })
        .collect()
}

/// At most [`MAX_JUNIT_MESSAGE_BYTES`], cut at a character boundary.
fn bounded(input: &str) -> String {
    let cleaned = clean(input);
    if cleaned.len() <= MAX_JUNIT_MESSAGE_BYTES {
        return cleaned;
    }
    let mut end = MAX_JUNIT_MESSAGE_BYTES - 3;
    while !cleaned.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &cleaned[..end])
}

fn escape(input: &str, quotes: bool) -> String {
    let mut out = String::with_capacity(input.len());
    for c in clean(input).chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' if quotes => out.push_str("&quot;"),
            '\'' if quotes => out.push_str("&apos;"),
            '\n' if quotes => out.push_str("&#10;"),
            '\t' if quotes => out.push_str("&#9;"),
            other => out.push(other),
        }
    }
    out
}

fn attr(input: &str) -> String {
    escape(input, true)
}

fn text(input: &str) -> String {
    escape(input, false)
}

/// Whether the Outcome's verdict counted findings: `RunOutcome::decide`
/// names them among the attention reasons only when the loadout fails on
/// findings.
fn inferred_fail_on_findings(outcome: &RunOutcome) -> bool {
    outcome.attention.iter().any(|reason| {
        reason.contains(" finding")
            && (reason.ends_with(" need attention") || reason.ends_with(" needs attention"))
    })
}

/// Render `outcome` as JUnit XML. Whether findings fail is read from the
/// Outcome's verdict reasons; [`render_junit_with`] takes it explicitly.
pub fn render_junit(outcome: &RunOutcome) -> Result<String, JunitError> {
    render_junit_with(outcome, inferred_fail_on_findings(outcome))
}

fn check_suites(outcome: &RunOutcome) -> (Suite, Vec<Suite>) {
    let mut checks = Suite::new("checks");
    let mut reports = Vec::new();
    for check in &outcome.checks {
        let mut case = Case::new("axocoatl.check", &check.name);
        let reason = check.reason.clone().unwrap_or_default();
        let exit = check
            .exit_code
            .map(|code| format!("exit code {code}"))
            .unwrap_or_else(|| "no exit code".into());
        case.status = match check.state {
            CheckState::Passed => Status::Passed,
            CheckState::Failed => Status::Failure {
                kind: "failed".into(),
                message: if reason.is_empty() {
                    format!("{} failed ({exit})", check.name)
                } else {
                    reason.clone()
                },
            },
            CheckState::TimedOut => Status::Failure {
                kind: "timed_out".into(),
                message: format!(
                    "{} ran past its {} s timeout",
                    check.name,
                    check.timeout_ms / 1000
                ),
            },
            CheckState::NotRun => Status::Error {
                kind: "not_run".into(),
                message: if reason.is_empty() {
                    format!("{} did not run on the final result", check.name)
                } else {
                    reason.clone()
                },
            },
            CheckState::Unavailable => Status::Error {
                kind: "unavailable".into(),
                message: if reason.is_empty() {
                    format!("the record of {} could not be read", check.name)
                } else {
                    reason.clone()
                },
            },
        };
        if !matches!(case.status, Status::Passed) {
            let mut body = String::new();
            if !check.stdout_tail.is_empty() {
                let _ = writeln!(body, "stdout:\n{}", check.stdout_tail);
            }
            if !check.stderr_tail.is_empty() {
                let _ = writeln!(body, "stderr:\n{}", check.stderr_tail);
            }
            case.body = Some(body);
        }
        // A reason the status does not already carry (a passed check whose
        // report could not be read, say) is kept in the test case.
        let shown = match &case.status {
            Status::Failure { message, .. } | Status::Error { message, .. } => message == &reason,
            Status::Passed | Status::Skipped { .. } => false,
        };
        let mut out = format!("argv: {}", check.argv.join(" "));
        if !reason.is_empty() && !shown {
            let _ = write!(out, "\nreason: {reason}");
        }
        case.system_out = Some(out);
        checks.cases.push(case);
        if let Some(report) = &check.report {
            let mut suite = Suite::new(format!("check:{}", check.name));
            for test in &report.tests {
                let classname = if test.suite.is_empty() {
                    check.name.clone()
                } else {
                    format!("{}.{}", check.name, test.suite)
                };
                let message = test.message.clone().unwrap_or_default();
                let mut case =
                    Case::new(classname, &test.name).status(match test.status.as_str() {
                        "passed" => Status::Passed,
                        "failed" => Status::Failure {
                            kind: "failed".into(),
                            message: message.clone(),
                        },
                        "skipped" => Status::Skipped {
                            message: message.clone(),
                        },
                        other => Status::Error {
                            kind: if other == "error" {
                                "error".into()
                            } else {
                                format!("unknown status {other}")
                            },
                            message: message.clone(),
                        },
                    });
                case.time_ms = test.duration_ms;
                suite.cases.push(case);
            }
            if report.truncated > 0 {
                suite.cases.push(
                    Case::new(format!("{}.report", check.name), "truncated").status(
                        Status::Error {
                            kind: "truncated".into(),
                            message: format!(
                            "{} test cases of the report were left out to stay within the bound",
                            report.truncated
                        ),
                        },
                    ),
                );
            }
            reports.push(suite);
        }
    }
    (checks, reports)
}

fn finding_case(finding: &Finding, fail_on_findings: bool) -> Case {
    let area = finding
        .area
        .clone()
        .filter(|area| !area.is_empty())
        .unwrap_or_else(|| "general".into());
    let mut case = Case::new(
        format!("axocoatl.finding.{area}"),
        format!("{} {}", finding.id, finding.title),
    );
    let mut detail = finding.detail.clone();
    if let Some(location) = &finding.location {
        detail = format!("{location}: {detail}");
    }
    let counted = |kind: &str| {
        if fail_on_findings {
            Status::Failure {
                kind: kind.into(),
                message: detail.clone(),
            }
        } else {
            Status::Passed
        }
    };
    case.status = match &finding.repro {
        Some(repro) => match repro.classification {
            ReproClassification::Confirmed => counted("confirmed"),
            ReproClassification::Reproduced => counted("reproduced"),
            ReproClassification::FailsOnCleanBuild => Status::Skipped {
                message: "fails on clean build".into(),
            },
            ReproClassification::NotReproduced => Status::Skipped {
                message: "not reproduced".into(),
            },
            ReproClassification::ReproError => Status::Error {
                kind: "repro_error".into(),
                message: repro
                    .target
                    .as_ref()
                    .and_then(|run| run.first_error.clone())
                    .unwrap_or_else(|| "the reproduction could not run".into()),
            },
            ReproClassification::Missing => Status::Error {
                kind: "missing".into(),
                message: format!("no reproduction at {}", repro.path),
            },
        },
        None if finding.source != FindingSource::Explorer => counted("finding"),
        None => Status::Error {
            kind: "missing".into(),
            message: "the finding names no reproduction".into(),
        },
    };
    let mut out = String::new();
    if matches!(case.status, Status::Passed) || matches!(case.status, Status::Skipped { .. }) {
        out.push_str(&detail);
    }
    if let Some(repro) = &finding.repro {
        if !out.is_empty() {
            out.push('\n');
        }
        let _ = write!(out, "repro: {}", repro.path);
    }
    if !out.is_empty() {
        case.system_out = Some(out);
    }
    case
}

fn verdict_case(outcome: &RunOutcome) -> Case {
    let case = Case::new("axocoatl.run", "verdict");
    let reasons = outcome.attention.join("; ");
    match outcome.verdict {
        RunVerdict::Pass => case,
        RunVerdict::ChecksFailed => case.status(Status::Failure {
            kind: "checks_failed".into(),
            message: if reasons.is_empty() {
                "a required check failed".into()
            } else {
                reasons
            },
        }),
        RunVerdict::NeedsAttention => case.status(Status::Failure {
            kind: "needs_attention".into(),
            message: reasons,
        }),
        RunVerdict::Interrupted => case.status(Status::Error {
            kind: "interrupted".into(),
            message: "the run was interrupted".into(),
        }),
        RunVerdict::Error => case.status(Status::Error {
            kind: "error".into(),
            message: outcome
                .error
                .clone()
                .unwrap_or_else(|| "the run could not execute".into()),
        }),
    }
}

/// Render `outcome` as JUnit XML, with findings failing when
/// `fail_on_findings` (the loadout's `qa.fail_on_findings` /
/// `audit.fail_on_findings`).
pub fn render_junit_with(
    outcome: &RunOutcome,
    fail_on_findings: bool,
) -> Result<String, JunitError> {
    let (checks, reports) = check_suites(outcome);
    let mut suites = vec![checks];
    suites.extend(reports);
    if let Some(review) = &outcome.review {
        let mut suite = Suite::new("review");
        let mut case = Case::new("axocoatl.review", "required-review");
        if !review.passed {
            case.status = Status::Failure {
                kind: review.state.clone(),
                message: review.reason.clone(),
            };
        }
        let mut out = format!(
            "reviewer {}:{} ({}), {} of {} rounds",
            review.reviewer.provider,
            review.reviewer.model,
            review.reviewer.runtime,
            review.rounds.len(),
            review.max_rounds
        );
        for round in &review.rounds {
            let findings = round_findings(round).len();
            let _ = write!(
                out,
                "\nround {}: {}, {findings} finding{}",
                round.round,
                match round.verdict {
                    ReviewVerdictKind::Approve => "approve",
                    ReviewVerdictKind::Changes => "changes",
                    ReviewVerdictKind::Unreadable => "unreadable",
                },
                if findings == 1 { "" } else { "s" }
            );
        }
        case.system_out = Some(out);
        suite.cases.push(case);
        suites.push(suite);
    }
    let mut adjudications = Suite::new("adjudications");
    for adjudication in &outcome.adjudications {
        let case = Case::new(
            "axocoatl.adjudication",
            format!("round {} {}", adjudication.round, adjudication.finding_id),
        );
        adjudications.cases.push(match adjudication.decision {
            AdjudicationDecision::Accept => case.out(format!("accept: {}", adjudication.reason)),
            AdjudicationDecision::Reject => case.out(format!("reject: {}", adjudication.reason)),
            AdjudicationDecision::Missing => case
                .status(Status::Failure {
                    kind: "missing".into(),
                    message: if adjudication.reason.is_empty() {
                        format!("the writer did not answer {}", adjudication.finding_id)
                    } else {
                        adjudication.reason.clone()
                    },
                })
                .out(adjudication.finding.clone()),
        });
    }
    suites.push(adjudications);
    let mut findings = Suite::new("findings");
    for finding in &outcome.findings {
        findings.cases.push(finding_case(finding, fail_on_findings));
    }
    suites.push(findings);
    let mut coverage = Suite::new("coverage");
    for entry in &outcome.not_covered {
        coverage.cases.push(
            Case::new("axocoatl.coverage", &entry.area).status(Status::Failure {
                kind: "not_covered".into(),
                message: entry.reason(),
            }),
        );
    }
    suites.push(coverage);
    let mut run = Suite::new("run");
    run.cases.push(verdict_case(outcome));
    suites.push(run);
    Ok(assemble(outcome, suites))
}

fn assemble(outcome: &RunOutcome, suites: Vec<Suite>) -> String {
    let budget = MAX_JUNIT_BYTES - JUNIT_RESERVE_BYTES;
    let mut used = 1024;
    let mut left_out = 0usize;
    let mut rendered: Vec<(String, Counts, String)> = Vec::new();
    for suite in suites {
        let mut counts = Counts::default();
        let mut body = String::new();
        for case in &suite.cases {
            let xml = case.render();
            // The verdict case is always kept.
            if used + xml.len() > budget && suite.name != "run" {
                left_out += 1;
                continue;
            }
            used += xml.len();
            counts.add(&case.status);
            body.push_str(&xml);
        }
        used += 128 + suite.name.len();
        rendered.push((suite.name, counts, body));
    }
    if left_out > 0 {
        let case = Case::new("axocoatl.truncated", "truncated").status(Status::Error {
            kind: "truncated".into(),
            message: format!(
                "{left_out} test cases were left out to keep this file within {} MiB",
                MAX_JUNIT_BYTES / (1024 * 1024)
            ),
        });
        let mut counts = Counts::default();
        counts.add(&case.status);
        rendered.push(("truncated".into(), counts, case.render()));
    }
    let mut total = Counts::default();
    for (_, counts, _) in &rendered {
        total.tests += counts.tests;
        total.failures += counts.failures;
        total.errors += counts.errors;
        total.skipped += counts.skipped;
    }
    let elapsed = outcome.finished_at_ms.saturating_sub(outcome.started_at_ms);
    let mut xml = String::with_capacity(used + 1024);
    xml.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    let _ = writeln!(
        xml,
        "<testsuites name=\"axocoatl\" {} time=\"{}\">",
        total.attrs(),
        seconds(elapsed)
    );
    xml.push_str("  <properties>\n");
    let verdict = serde_json::to_value(outcome.verdict)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_default();
    for (name, value) in [
        ("axocoatl.run_id", outcome.run_id.clone()),
        ("axocoatl.session_id", outcome.session_id.clone()),
        (
            "axocoatl.loadout",
            format!(
                "{}@{} sha256:{}",
                outcome.loadout.id, outcome.loadout.version, outcome.loadout.digest
            ),
        ),
        ("axocoatl.verdict", verdict),
        ("axocoatl.exit_code", outcome.exit_code.to_string()),
    ] {
        let _ = writeln!(
            xml,
            "    <property name=\"{}\" value=\"{}\"/>",
            attr(name),
            attr(&bounded(&value))
        );
    }
    for warning in &outcome.warnings {
        let _ = writeln!(
            xml,
            "    <property name=\"axocoatl.warning.{}\" value=\"{}\"/>",
            attr(&warning.code),
            attr(&bounded(&warning.message))
        );
    }
    xml.push_str("  </properties>\n");
    for (name, counts, body) in rendered {
        if body.is_empty() {
            let _ = writeln!(
                xml,
                "  <testsuite name=\"{}\" {}/>",
                attr(&name),
                counts.attrs()
            );
        } else {
            let _ = writeln!(
                xml,
                "  <testsuite name=\"{}\" {}>",
                attr(&name),
                counts.attrs()
            );
            xml.push_str(&body);
            xml.push_str("  </testsuite>\n");
        }
    }
    xml.push_str("</testsuites>\n");
    xml
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run_outcome::*;

    fn identity(model: &str) -> ModelIdentity {
        ModelIdentity {
            provider: "openrouter".into(),
            model: model.into(),
            runtime: "native".into(),
        }
    }

    fn check(name: &str, state: CheckState) -> CheckResult {
        CheckResult {
            name: name.into(),
            argv: vec!["npm".into(), "test".into()],
            state,
            timeout_ms: 180_000,
            exit_code: match state {
                CheckState::Passed => Some(0),
                CheckState::Failed => Some(1),
                _ => None,
            },
            stdout_tail: String::new(),
            stderr_tail: if state == CheckState::Failed {
                "assertion <failed> & more\u{1b}[31m".into()
            } else {
                String::new()
            },
            candidate_sha256: None,
            report: None,
            reason: None,
        }
    }

    fn finding(id: &str, area: &str, classification: ReproClassification) -> Finding {
        Finding {
            id: id.into(),
            source: FindingSource::Explorer,
            title: format!("{id} title"),
            detail: "total \"ignores\" coupon".into(),
            severity: Some(Severity::High),
            area: Some(area.into()),
            location: None,
            repro: Some(ReproResult {
                path: format!(".axocoatl/qa/{id}.spec.ts"),
                sha256: None,
                classification,
                target: None,
                reference: None,
            }),
        }
    }

    /// One Outcome that exercises every rule of the JUnit shape.
    fn fixture() -> RunOutcome {
        let mut e2e = check("e2e", CheckState::Failed);
        e2e.report = Some(CheckReport {
            format: "junit".into(),
            sha256: "b".repeat(64),
            tests: vec![
                CheckTestCase {
                    suite: "checkout".into(),
                    name: "pays with a saved card".into(),
                    status: "failed".into(),
                    message: Some("expected 10 < 9".into()),
                    duration_ms: Some(1500),
                },
                CheckTestCase {
                    suite: "checkout".into(),
                    name: "shows the cart".into(),
                    status: "passed".into(),
                    message: None,
                    duration_ms: None,
                },
            ],
            passed: 1,
            failed: 1,
            skipped: 0,
            errors: 0,
            truncated: 0,
        });
        let mut outcome = RunOutcome {
            schema: RUN_OUTCOME_SCHEMA.into(),
            run_id: "run-00000000-0000-4000-8000-000000000001".into(),
            session_id: "ses-1".into(),
            workspace_id: "wsp-1".into(),
            loadout: LoadoutRef {
                id: "fix".into(),
                version: 1,
                kind: "fix".into(),
                digest: "c".repeat(64),
                builtin: true,
            },
            task: "t".into(),
            started_at_ms: 1_000,
            finished_at_ms: 513_300,
            verdict: RunVerdict::Pass,
            exit_code: 0,
            attention: Vec::new(),
            turns: Vec::new(),
            checks: vec![
                check("tests", CheckState::Passed),
                e2e,
                check("lint", CheckState::TimedOut),
                check("types", CheckState::NotRun),
                CheckResult {
                    reason: Some(
                        "the report /tmp/axocoatl-check-reports/smoke/report.json could not be \
                         read: IO error"
                            .into(),
                    ),
                    ..check("smoke", CheckState::Passed)
                },
            ],
            review: Some(ReviewOutcome {
                reviewer: identity("openai/gpt-oss-120b"),
                max_rounds: 3,
                rounds: vec![ReviewRound {
                    round: 1,
                    verdict: ReviewVerdictKind::Changes,
                    passed: false,
                    findings_text: "F1 x\nF2 y".into(),
                    findings: vec![],
                    continued: true,
                    candidate_sha256: None,
                }],
                passed: false,
                state: "changes".into(),
                reason: "the reviewer asked for changes".into(),
            }),
            adjudications: vec![
                Adjudication {
                    round: 1,
                    finding_id: "F1".into(),
                    finding: "x".into(),
                    decision: AdjudicationDecision::Reject,
                    reason: "not a bug".into(),
                    writer_generation: Some(2),
                },
                Adjudication {
                    round: 1,
                    finding_id: "F2".into(),
                    finding: "y".into(),
                    decision: AdjudicationDecision::Missing,
                    reason: String::new(),
                    writer_generation: None,
                },
            ],
            findings: vec![
                finding("B1", "checkout", ReproClassification::Confirmed),
                finding("B2", "search", ReproClassification::FailsOnCleanBuild),
                finding("B3", "search", ReproClassification::ReproError),
            ],
            not_covered: vec![
                NotCovered {
                    area: "gift cards".into(),
                    class: FailureClass::ProviderRefusal,
                    detail: "classifier stop".into(),
                    node_id: None,
                    turn_id: None,
                },
                NotCovered {
                    area: "checkout".into(),
                    class: FailureClass::NotReached,
                    detail: "not_reached: ran out of steps".into(),
                    node_id: None,
                    turn_id: None,
                },
            ],
            warnings: vec![RunWarning {
                code: SAME_MODEL_REVIEWER.into(),
                message: "same model".into(),
            }],
            usage: RunUsage::default(),
            network: NetworkSummary::default(),
            keep: None,
            error: None,
        };
        outcome.decide(VerdictInputs {
            fail_on_findings: true,
            ..VerdictInputs::default()
        });
        outcome
    }

    #[test]
    fn junit_golden() {
        let xml = render_junit(&fixture()).unwrap();
        let golden = include_str!("../tests/fixtures/run_junit_golden.xml");
        if xml != golden && std::env::var_os("AXOCOATL_BLESS").is_some() {
            std::fs::write(
                concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/fixtures/run_junit_golden.xml"
                ),
                &xml,
            )
            .unwrap();
            return;
        }
        assert_eq!(xml, golden, "{xml}");
    }

    #[test]
    fn junit_rules() {
        let xml = render_junit(&fixture()).unwrap();
        // Escaping and control characters.
        assert!(xml.contains("assertion &lt;failed&gt; &amp; more[31m"));
        assert!(!xml.contains('\u{1b}'));
        assert!(xml.contains("total &quot;ignores&quot; coupon"));
        // Checks: failure for failed and timed out, error for not run.
        assert!(xml.contains("<failure type=\"timed_out\""));
        assert!(xml.contains("<error type=\"not_run\""));
        // A report becomes its own suite.
        assert!(xml.contains("<testsuite name=\"check:e2e\" tests=\"2\" failures=\"1\""));
        assert!(xml
            .contains("classname=\"e2e.checkout\" name=\"pays with a saved card\" time=\"1.500\""));
        // Review not passed, missing adjudication, reject passes with its reason.
        assert!(xml.contains("<failure type=\"changes\""));
        assert!(xml.contains("<failure type=\"missing\" message=\"the writer did not answer F2\""));
        assert!(xml.contains("<system-out>reject: not a bug</system-out>"));
        // Findings: confirmed fails, clean-build failures are skipped, repro errors are errors.
        assert!(xml.contains("<failure type=\"confirmed\""));
        assert!(xml.contains("<skipped message=\"fails on clean build\"/>"));
        assert!(xml.contains("<error type=\"repro_error\""));
        // Not covered is a failure, never skipped, naming its class once.
        assert!(xml.contains(
            "<failure type=\"not_covered\" message=\"provider_refusal: classifier stop\"/>"
        ));
        assert!(xml
            .contains("<failure type=\"not_covered\" message=\"not_reached: ran out of steps\"/>"));
        // A passed check keeps the reason its report could not be read.
        assert!(xml.contains(
            "<system-out>argv: npm test\nreason: the report \
             /tmp/axocoatl-check-reports/smoke/report.json could not be read: IO error</system-out>"
        ));
        // The review's rounds: their verdicts and finding counts, in words.
        assert!(xml.contains("\nround 1: changes, 2 findings</system-out>"));
        // Properties.
        assert!(xml.contains(&format!("value=\"fix@1 sha256:{}\"", "c".repeat(64))));
        assert!(xml.contains("<property name=\"axocoatl.exit_code\" value=\"1\"/>"));
        assert!(xml.contains("time=\"512.300\""));
    }

    #[test]
    fn findings_pass_when_the_loadout_does_not_fail_on_them() {
        let mut outcome = fixture();
        outcome.findings = vec![finding("B1", "checkout", ReproClassification::Confirmed)];
        let xml = render_junit_with(&outcome, false).unwrap();
        assert!(!xml.contains("<failure type=\"confirmed\""));
        assert!(xml.contains("<testsuite name=\"findings\" tests=\"1\" failures=\"0\""));
        let xml = render_junit_with(&outcome, true).unwrap();
        assert!(xml.contains("<failure type=\"confirmed\""));
    }

    #[test]
    fn messages_and_documents_are_bounded() {
        let mut outcome = fixture();
        outcome.not_covered = (0..20_000)
            .map(|index| NotCovered {
                area: format!("area {index}"),
                class: FailureClass::NotReached,
                detail: "x".repeat(8 * 1024),
                node_id: None,
                turn_id: None,
            })
            .collect();
        let xml = render_junit(&outcome).unwrap();
        assert!(xml.len() <= MAX_JUNIT_BYTES, "{}", xml.len());
        assert!(xml.contains("name=\"truncated\""));
        assert!(xml.contains("<testcase classname=\"axocoatl.run\" name=\"verdict\""));
        assert!(xml.ends_with("</testsuites>\n"));
        assert!(!xml.contains(&"x".repeat(MAX_JUNIT_MESSAGE_BYTES + 1)));
    }

    #[test]
    fn review_rounds_count_findings_in_words() {
        let mut outcome = fixture();
        let review = outcome.review.as_mut().unwrap();
        review.rounds[0].findings_text = "F1: src/paginate.js:7: off by one".into();
        review.rounds.push(ReviewRound {
            round: 2,
            verdict: ReviewVerdictKind::Approve,
            passed: true,
            findings_text: "Nothing must change.".into(),
            findings: Vec::new(),
            continued: false,
            candidate_sha256: None,
        });
        let xml = render_junit(&outcome).unwrap();
        assert!(
            xml.contains(
                "\nround 1: changes, 1 finding\nround 2: approve, 0 findings</system-out>"
            ),
            "{xml}"
        );
        assert!(!xml.contains("1 findings"));
    }

    #[test]
    fn a_failed_check_reason_is_its_message_and_not_repeated() {
        let mut outcome = fixture();
        let mut failed = check("e2e", CheckState::Failed);
        failed.reason = Some("e2e failed; the report could not be read".into());
        let mut timed_out = check("slow", CheckState::TimedOut);
        timed_out.reason = Some("the report could not be read".into());
        outcome.checks = vec![failed, timed_out];
        let xml = render_junit(&outcome).unwrap();
        assert!(xml.contains(
            "<failure type=\"failed\" message=\"e2e failed; the report could not be read\">"
        ));
        assert_eq!(
            xml.matches("e2e failed; the report could not be read")
                .count(),
            1
        );
        // The timeout is the message; the reason goes to system-out.
        assert!(xml.contains("reason: the report could not be read</system-out>"));
    }

    #[test]
    fn a_passing_run_has_no_failures() {
        let mut outcome = fixture();
        outcome.checks = vec![check("tests", CheckState::Passed)];
        outcome.review = None;
        outcome.adjudications.clear();
        outcome.findings.clear();
        outcome.not_covered.clear();
        assert_eq!(outcome.decide(VerdictInputs::default()), exit_code::PASS);
        let xml = render_junit(&outcome).unwrap();
        assert!(xml.contains(
            "<testsuites name=\"axocoatl\" tests=\"2\" failures=\"0\" errors=\"0\" skipped=\"0\""
        ));
    }
}
