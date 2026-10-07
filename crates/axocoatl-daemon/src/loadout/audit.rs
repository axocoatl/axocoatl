//! The audit loadout: three turns in the run's one Session, each after its
//! own Team and budget Apply.
//!
//! 1. **Plan**: the `planner` slot alone answers with an `AREAS` block of
//!    `min_areas`..`max_areas` areas. An invalid plan gets one retry turn
//!    that quotes the parse error; a second invalid plan (or a planner with
//!    no answer) ends the run with the whole scope not covered.
//! 2. **Areas**: one `worker-<area>` slot per area, instantiated from the
//!    `worker` Agent: read-only (`writes: []`; its commands run under the
//!    supervisor's write restriction), a fresh context (`reset_history`), no
//!    dependencies, required, no checks and no review, so the controller
//!    starts every one at once. A worker without a result, an unreadable
//!    report and every `NOT_REACHED` entry of its own area are listed as not
//!    covered. An entry that names another planned area is dropped (that
//!    area's own worker audits it), and one that names a repository path
//!    that does not exist is recorded as a note, not a gap
//!    ([`classify_not_reached`]).
//! 3. **Integrate**: the `integrator` slot alone receives every worker's
//!    report (each bounded to 24 KiB, truncation noted) and the not-covered
//!    list, and answers with the merged `FINDINGS`. When integration has no
//!    result, the workers' findings are reported unmerged and integration is
//!    listed as not covered.
//!
//! The run's wall clock bounds all three turns: at the deadline the turn is
//! stopped, what did not finish is not covered (budget), and no further turn
//! starts. Findings change the exit code only with `fail_on_findings`
//! (default false); anything not covered always needs attention.
//!
//! Owner: audit.

use std::fmt::Write as _;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use axocoatl_config::loadout::{AuditSettings, LoadoutAgent, LoadoutRole, ResolvedLoadout};
use axocoatl_session::audit_plan::{
    normalize_area_name, parse_area_report, parse_integrated, parse_plan, AreaReport, AuditArea,
    AuditPlan,
};
use axocoatl_session::failure_class::{classify_failure, FailureFacts};
use axocoatl_session::path_scope::{in_git_directory, pattern_matches};
use axocoatl_session::run_outcome::{
    FailureClass, Finding, NodeObservation, NodeState, NotCovered, RunTurnRef, TurnObservation,
    TurnState,
};
use axocoatl_session::run_record::RunEvent;

use super::team_plan::{self, SlotPlan};
use super::{KindDriver, KindReport, RunContext, RunError, RunHost};
use crate::SessionTeamEdit;

/// Slot of the plan turn.
pub const PLANNER_SLOT: &str = "planner";
/// Slot of the integrate turn.
pub const INTEGRATOR_SLOT: &str = "integrator";
/// Prefix of each area worker's slot: `worker-<area>`.
pub const WORKER_SLOT_PREFIX: &str = "worker-";
/// Largest worker report the integrator receives, in bytes.
pub const MAX_REPORT_BYTES: usize = 24 * 1024;
/// The not-covered entry of a run whose plan never became usable.
pub const WHOLE_SCOPE: &str = "whole scope";
/// `RunTurnRef::purpose` of each turn.
pub const PLAN_PURPOSE: &str = "audit_plan";
pub const AREAS_PURPOSE: &str = "audit_areas";
pub const INTEGRATE_PURPOSE: &str = "audit_integrate";
/// How long a stopped turn may take to settle before it is observed.
const STOP_GRACE: Duration = Duration::from_secs(30);
/// Not-covered entries listed in the integrate request; the rest are counted.
const MAX_LISTED_NOT_COVERED: usize = 64;
const MAX_LISTED_DETAIL_BYTES: usize = 300;
/// Most repository entries looked at to tell whether a `NOT_REACHED` path
/// pattern names anything; past it the entry stays a gap.
const MAX_PATH_WALK_ENTRIES: usize = 20_000;

/// Builds the Team and budget edit of one turn; `team_plan::team_edit` in
/// the daemon, a recording fake in tests.
pub type EditBuilder = dyn Fn(&ResolvedLoadout, &[SlotPlan], bool, u64) -> Result<SessionTeamEdit, RunError>
    + Send
    + Sync;

pub struct AuditDriver;

#[async_trait]
impl KindDriver for AuditDriver {
    async fn drive(&self, host: &dyn RunHost, run: &RunContext) -> Result<KindReport, RunError> {
        drive_audit(host, run, &team_plan::team_edit).await
    }
}

/// Run the three audit turns with `build` making each turn's edit.
pub async fn drive_audit(
    host: &dyn RunHost,
    run: &RunContext,
    build: &EditBuilder,
) -> Result<KindReport, RunError> {
    let settings = audit_settings(&run.resolved)?;
    let mut audit = Audit {
        host,
        run,
        build,
        applies: 0,
        open_turn: None,
        report: KindReport {
            fail_on_findings: settings.fail_on_findings,
            ..KindReport::default()
        },
    };
    if let Some(plan) = audit.plan(&settings).await? {
        let results = audit.areas(&plan).await?;
        audit.integrate(&plan, &results).await?;
    }
    Ok(audit.report)
}

fn audit_settings(resolved: &ResolvedLoadout) -> Result<AuditSettings, RunError> {
    resolved
        .loadout
        .file
        .audit
        .clone()
        .ok_or_else(|| RunError::Usage("an audit loadout needs an audit section".into()))
}

/// The loadout Agent with `role`.
fn agent(resolved: &ResolvedLoadout, role: LoadoutRole) -> Result<&LoadoutAgent, RunError> {
    resolved
        .loadout
        .file
        .agents
        .iter()
        .find(|agent| agent.role == role)
        .ok_or_else(|| RunError::Usage(format!("the audit loadout has no {role:?} Agent")))
}

/// One read-only, required slot of `agent` with no dependencies.
fn read_only_slot(
    resolved: &ResolvedLoadout,
    slot_id: String,
    agent: &LoadoutAgent,
    instructions: Option<String>,
) -> Result<SlotPlan, RunError> {
    let model = resolved
        .agent_models
        .get(&agent.id)
        .cloned()
        .ok_or_else(|| RunError::Usage(format!("Agent {} has no resolved model", agent.id)))?;
    let mut agent = agent.clone();
    agent.writes = Some(Vec::new());
    agent.depends_on.clear();
    Ok(SlotPlan {
        slot_id,
        agent,
        model,
        instructions,
        depends_on: Vec::new(),
        required: true,
    })
}

/// The plan turn's team: the planner alone.
pub fn plan_slots(resolved: &ResolvedLoadout) -> Result<Vec<SlotPlan>, RunError> {
    let planner = agent(resolved, LoadoutRole::Planner)?;
    Ok(vec![read_only_slot(
        resolved,
        PLANNER_SLOT.into(),
        planner,
        None,
    )?])
}

/// The areas turn's team: one fresh read-only worker per area, none
/// depending on another.
pub fn area_slots(resolved: &ResolvedLoadout, plan: &AuditPlan) -> Result<Vec<SlotPlan>, RunError> {
    let worker = agent(resolved, LoadoutRole::Worker)?;
    plan.areas
        .iter()
        .map(|area| {
            read_only_slot(
                resolved,
                worker_slot_id(&area.name),
                worker,
                Some(worker_instructions(
                    worker.instructions.as_deref(),
                    area,
                    plan,
                )),
            )
        })
        .collect()
}

/// The integrate turn's team: the integrator alone. Integration is its own
/// turn, so the integrator depends on nothing.
pub fn integrate_slots(resolved: &ResolvedLoadout) -> Result<Vec<SlotPlan>, RunError> {
    let integrator = agent(resolved, LoadoutRole::Integrator)?;
    Ok(vec![read_only_slot(
        resolved,
        INTEGRATOR_SLOT.into(),
        integrator,
        None,
    )?])
}

pub fn worker_slot_id(area: &str) -> String {
    format!("{WORKER_SLOT_PREFIX}{area}")
}

/// Narrow `team_plan::team_edit`'s result to what every audit turn is: each
/// slot read-only, required and starting from a fresh context, with no
/// dependencies, no required checks and no review. Never widens anything.
pub fn read_only_edit(mut edit: SessionTeamEdit) -> SessionTeamEdit {
    for slot in &mut edit.slots {
        slot.writes = Some(Some(Vec::new()));
        slot.reset_history = true;
        slot.required = true;
    }
    edit.dependencies.clear();
    edit.required_checks.clear();
    edit.check_options.clear();
    edit.required_review = None;
    edit
}

const AREAS_SHAPE: &str = "AREAS\n```json\n{\"areas\": [{\"name\": \"auth\", \"scope\": \"what \
     this area covers and what to look for\", \"paths\": [\"src/auth/**\"]}]}\n```";

fn area_rules(min: u32, max: u32) -> String {
    format!(
        "Answer with one AREAS block of {min}-{max} areas that together cover the scope \
         without overlap. Each area is audited in parallel by its own read-only worker with a \
         fresh context, so give each a self-contained scope. Names are lowercase letters, \
         digits and hyphens, start with a letter, are at most 32 characters and unique.\n\
         {AREAS_SHAPE}\n"
    )
}

/// The plan turn's request.
pub fn plan_request(prompt: &str, min: u32, max: u32) -> String {
    format!(
        "{}\n\nPlan this audit before it starts. {}",
        prompt.trim_end(),
        area_rules(min, max)
    )
}

/// The one retry after an invalid plan, quoting why it was refused.
pub fn plan_retry_request(prompt: &str, min: u32, max: u32, error: &str) -> String {
    format!(
        "{}\n\nYour AREAS block could not be used: {error}\nAnswer again. {}",
        prompt.trim_end(),
        area_rules(min, max)
    )
}

/// An area worker's instructions: the loadout's worker instructions, its
/// area, the other areas, and the report it ends with.
pub fn worker_instructions(base: Option<&str>, area: &AuditArea, plan: &AuditPlan) -> String {
    let mut text = String::new();
    if let Some(base) = base.map(str::trim).filter(|base| !base.is_empty()) {
        text.push_str(base);
        text.push_str("\n\n");
    }
    let _ = writeln!(text, "Your area: {}\nScope: {}", area.name, area.scope);
    if area.paths.is_empty() {
        text.push_str("Paths: the plan names none; find the code this scope covers.\n");
    } else {
        text.push_str("Paths:\n");
        for path in &area.paths {
            let _ = writeln!(text, "- {path}");
        }
    }
    let others: Vec<&str> = plan
        .areas
        .iter()
        .filter(|other| other.name != area.name)
        .map(|other| other.name.as_str())
        .collect();
    let _ = writeln!(
        text,
        "Other workers audit the other areas ({}) at the same time, each with a fresh context, \
         so those areas are covered: never list them as not reached. Stay inside yours. You are \
         read-only: change no file.",
        others.join(", ")
    );
    text.push_str(
        "\nEnd your answer with two blocks:\nFINDINGS\n```json\n[{\"id\": \"F1\", \"title\": \
         \"...\", \"detail\": \"what is wrong and your evidence\", \"severity\": \
         \"low|medium|high|critical\", \"location\": \"path:line\"}]\n```\nNOT_REACHED\n```json\n\
         [\"each existing path of your area you did not examine, and why\"]\n```\nWrite [] for a \
         block with no entries: NOT_REACHED is [] when you examined all of your area.",
    );
    text
}

/// What one `NOT_REACHED` entry of an area worker's report is
/// ([`classify_not_reached`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotReachedItem {
    /// A part of the worker's own area it did not examine: not covered.
    Gap,
    /// Another planned area (by name, or a path only that area's patterns
    /// name): its own worker audits it, so it is no gap of this one.
    OtherArea(String),
    /// A repository path or pattern that names nothing in the repository: a
    /// note, not a gap.
    NoSuchPath(String),
}

/// Classify one `NOT_REACHED` entry of `area`'s worker against the plan and
/// the repository at `repo` (the run's canonical repository on the host;
/// the audit is read-only, so it holds what the workers read). When the
/// repository cannot be read, or a pattern matches too much of it to tell,
/// the entry stays a gap: nothing is dropped on a guess.
pub fn classify_not_reached(
    item: &str,
    area: &AuditArea,
    plan: &AuditPlan,
    repo: &Path,
) -> NotReachedItem {
    let subject = item_subject(item);
    let others = || plan.areas.iter().filter(|other| other.name != area.name);
    if let Some(name) = area_word(subject) {
        if let Some(other) = others().find(|other| other.name == name) {
            return NotReachedItem::OtherArea(other.name.clone());
        }
    }
    let Some(path) = path_token(subject) else {
        return NotReachedItem::Gap;
    };
    let own = area
        .paths
        .iter()
        .any(|pattern| pattern_matches(pattern, &path));
    if !own {
        if let Some(other) = others().find(|other| {
            other
                .paths
                .iter()
                .any(|pattern| pattern_matches(pattern, &path))
        }) {
            return NotReachedItem::OtherArea(other.name.clone());
        }
    }
    match path_exists(repo, &path) {
        Some(false) => NotReachedItem::NoSuchPath(path),
        Some(true) | None => NotReachedItem::Gap,
    }
}

/// The entry without a trailing `(reason)`, quotes, bold marks or end
/// punctuation: `"src/a.rs (budget)"` and `**src/a.rs**` are `src/a.rs`.
fn item_subject(item: &str) -> &str {
    let mut subject = item.trim();
    if subject.ends_with(')') {
        if let Some(open) = subject.rfind(" (") {
            subject = subject[..open].trim_end();
        }
    }
    if let Some(inner) = subject
        .strip_prefix("**")
        .and_then(|rest| rest.strip_suffix("**"))
        .filter(|inner| !inner.is_empty())
    {
        subject = inner;
    }
    subject
        .trim_matches(|c: char| matches!(c, '`' | '"' | '\''))
        .trim_end_matches(['.', ',', ';', ':'])
        .trim()
}

/// The area name an entry says, when it is only a name: `billing`, `the
/// billing area`, `Billing module`, `billing/`, `API routes`
/// (`api-routes`).
fn area_word(subject: &str) -> Option<String> {
    let lower = subject.to_ascii_lowercase();
    let mut word = lower.trim();
    word = word.strip_prefix("the ").unwrap_or(word).trim();
    for suffix in [" area", " module", " directory", " package", "/**", "/"] {
        word = word.strip_suffix(suffix).unwrap_or(word).trim();
    }
    if word.is_empty() || word.contains('/') {
        return None;
    }
    normalize_area_name(word)
}

/// The repository-relative path or pattern an entry names, when it is one:
/// one word with a `/`, a wildcard or a file extension, never absolute,
/// never leaving the repository and never inside `.git`. A `:line` suffix
/// and a leading `./` are dropped.
fn path_token(subject: &str) -> Option<String> {
    if subject.is_empty()
        || subject.contains(char::is_whitespace)
        || subject.contains("://")
        || subject.contains('\\')
        || subject.starts_with(['/', '~'])
    {
        return None;
    }
    let mut path = subject.strip_prefix("./").unwrap_or(subject);
    if let Some((head, tail)) = path.rsplit_once(':') {
        if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit() || c == '-') {
            path = head;
        }
    }
    let path = path.trim_end_matches('/');
    if path.is_empty()
        || path.contains(':')
        || in_git_directory(path)
        || path
            .split('/')
            .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return None;
    }
    let extension = path
        .rsplit('/')
        .next()
        .and_then(|name| name.rsplit_once('.'))
        .is_some_and(|(stem, extension)| {
            !stem.is_empty()
                && (1..=10).contains(&extension.len())
                && extension.chars().all(|c| c.is_ascii_alphanumeric())
        });
    (path.contains(['/', '*', '?']) || extension).then(|| path.to_owned())
}

/// Whether `path` names anything under `repo`: `Some(true)` or
/// `Some(false)`, `None` when it cannot tell (the repository cannot be read,
/// or a pattern needs more than [`MAX_PATH_WALK_ENTRIES`] entries looked
/// at). A literal path with a `/` is looked up; a pattern, or a name without
/// a `/` (which may be at any depth), is matched against the files and
/// directories under its literal leading directories. Links are not
/// followed.
fn path_exists(repo: &Path, path: &str) -> Option<bool> {
    if !std::fs::metadata(repo).ok()?.is_dir() {
        return None;
    }
    let pattern = path.contains(['*', '?']) || !path.contains('/');
    if !pattern {
        return match std::fs::symlink_metadata(repo.join(path)) {
            Ok(_) => Some(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(false),
            Err(_) => None,
        };
    }
    // The literal directories before the first wildcard (none for a bare
    // name, which matches at any depth).
    let mut start = String::new();
    if path.contains('/') {
        for segment in path.split('/') {
            if segment.contains(['*', '?']) {
                break;
            }
            if !start.is_empty() {
                start.push('/');
            }
            start.push_str(segment);
        }
    }
    let root = repo.join(&start);
    match std::fs::symlink_metadata(&root) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return Some(pattern_matches(path, &start)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Some(false),
        Err(_) => return None,
    }
    let mut pending = vec![(root, start)];
    let mut seen = 0usize;
    while let Some((directory, relative)) = pending.pop() {
        for entry in std::fs::read_dir(&directory).ok()? {
            let entry = entry.ok()?;
            seen += 1;
            if seen > MAX_PATH_WALK_ENTRIES {
                return None;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let child = if relative.is_empty() {
                name.to_owned()
            } else {
                format!("{relative}/{name}")
            };
            if in_git_directory(&child) {
                continue;
            }
            if pattern_matches(path, &child) {
                return Some(true);
            }
            if entry.file_type().ok()?.is_dir() {
                pending.push((entry.path(), child));
            }
        }
    }
    Some(false)
}

/// The areas turn's request (each worker's own instructions name its area).
pub fn areas_request(prompt: &str, plan: &AuditPlan) -> String {
    let names: Vec<&str> = plan.areas.iter().map(|area| area.name.as_str()).collect();
    format!(
        "{}\n\nThis turn audits the {} areas of the plan in parallel: {}. Your instructions \
         name your area. Audit only that area and end with the FINDINGS and NOT_REACHED blocks \
         your instructions describe.",
        prompt.trim_end(),
        names.len(),
        names.join(", ")
    )
}

/// What one area worker produced.
#[derive(Debug, Clone, PartialEq)]
pub struct AreaResult {
    pub area: AuditArea,
    pub body: AreaBody,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AreaBody {
    /// The worker's readable report.
    Report(AreaReport),
    /// An accepted answer whose blocks could not be read; the integrator
    /// receives it as text and the area is listed as not covered.
    Unreadable { answer: String, error: String },
}

/// The integrate turn's request: every worker's report, each bounded to
/// [`MAX_REPORT_BYTES`] with truncation noted, and the not-covered list.
pub fn integrate_request(
    prompt: &str,
    plan: &AuditPlan,
    results: &[AreaResult],
    not_covered: &[NotCovered],
) -> String {
    let names: Vec<&str> = plan.areas.iter().map(|area| area.name.as_str()).collect();
    let mut text = String::new();
    text.push_str(prompt.trim_end());
    let _ = writeln!(
        text,
        "\n\nThe area workers of this audit have finished. Areas planned: {}.\n\
         Merge their findings into one list: the same defect reported by more than one area \
         is one finding; keep every distinct defect with its area, location and severity. You \
         may read the repository to check a finding; add none that no worker reported unless \
         you verified it.",
        names.join(", ")
    );
    if !not_covered.is_empty() {
        text.push_str(
            "\nNot covered (no usable result; they are reported separately, do not guess \
             findings for them):\n",
        );
        for entry in not_covered.iter().take(MAX_LISTED_NOT_COVERED) {
            let _ = writeln!(
                text,
                "- {} ({}): {}",
                entry.area,
                class_name(entry.class),
                cut(&entry.detail, MAX_LISTED_DETAIL_BYTES).0
            );
        }
        if not_covered.len() > MAX_LISTED_NOT_COVERED {
            let _ = writeln!(
                text,
                "- and {} more",
                not_covered.len() - MAX_LISTED_NOT_COVERED
            );
        }
    }
    for result in results {
        let _ = writeln!(
            text,
            "\nREPORT of area {} (scope: {})",
            result.area.name,
            cut(&result.area.scope, MAX_LISTED_DETAIL_BYTES).0
        );
        text.push_str(&report_text(&result.body));
    }
    text.push_str(
        "\nAnswer with one FINDINGS block:\nFINDINGS\n```json\n[{\"id\": \"A1\", \"title\": \
         \"...\", \"detail\": \"...\", \"severity\": \"low|medium|high|critical\", \
         \"location\": \"path:line\", \"area\": \"<area name>\"}]\n```\nWrite [] when no area \
         found anything.\n",
    );
    text
}

/// One worker's report as the integrator reads it: a fenced block whose
/// body is at most [`MAX_REPORT_BYTES`], followed by a note when cut.
pub fn report_text(body: &AreaBody) -> String {
    match body {
        AreaBody::Report(report) => {
            let mut lines = Vec::new();
            let mut used = 4; // "[\n" and "\n]"
            for finding in &report.findings {
                let line = serde_json::json!({
                    "id": finding.id,
                    "title": finding.title,
                    "detail": finding.detail,
                    "severity": finding.severity,
                    "location": finding.location,
                })
                .to_string();
                // Each line costs its bytes plus ",\n".
                if used + line.len() + 2 > MAX_REPORT_BYTES {
                    break;
                }
                used += line.len() + 2;
                lines.push(line);
            }
            let mut text = format!("```json\n[\n{}\n]\n```\n", lines.join(",\n"));
            if lines.len() < report.findings.len() {
                let _ = writeln!(
                    text,
                    "(truncated: {} of {} findings shown; the rest were left out to keep this \
                     report within {} KiB)",
                    lines.len(),
                    report.findings.len(),
                    MAX_REPORT_BYTES / 1024
                );
            }
            text
        }
        AreaBody::Unreadable { answer, error } => {
            let (shown, truncated) = cut(answer, MAX_REPORT_BYTES);
            let fence = fence_for(shown);
            let mut text = format!(
                "The worker's report could not be read ({error}); its answer follows as \
                 text.\n{fence}text\n{shown}\n{fence}\n"
            );
            if truncated {
                let _ = writeln!(
                    text,
                    "(truncated: the answer was {} bytes; the first {} KiB are shown)",
                    answer.len(),
                    MAX_REPORT_BYTES / 1024
                );
            }
            text
        }
    }
}

/// A backtick fence longer than any backtick run in `text`.
fn fence_for(text: &str) -> String {
    let mut longest = 0;
    let mut run = 0;
    for character in text.chars() {
        run = if character == '`' { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    "`".repeat((longest + 1).max(3))
}

/// `text` cut to at most `max` bytes on a character boundary, and whether
/// it was cut.
fn cut(text: &str, max: usize) -> (&str, bool) {
    if text.len() <= max {
        return (text, false);
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

pub fn class_name(class: FailureClass) -> &'static str {
    match class {
        FailureClass::ProviderRefusal => "provider_refusal",
        FailureClass::ProviderFailure => "provider_failure",
        FailureClass::ProviderRejected => "provider_rejected",
        FailureClass::Budget => "budget",
        FailureClass::Blocked => "blocked",
        FailureClass::NotReached => "not_reached",
        FailureClass::RuntimeLimit => "runtime_limit",
        FailureClass::Stopped => "stopped",
        FailureClass::Other => "other",
    }
}

/// The accepted answer of `node`'s latest generation.
fn accepted_answer(node: Option<&NodeObservation>) -> Option<&str> {
    let latest = node?.latest()?;
    (latest.state == NodeState::Accepted)
        .then_some(latest.answer.as_deref())
        .flatten()
        .filter(|answer| !answer.trim().is_empty())
}

/// Why `node` has no result, as a failure class and words. A recorded
/// class is kept (refined through `classify_failure` when it is `other`);
/// a node the wall clock stopped or never reached is a budget failure.
pub fn failure_of(node: Option<&NodeObservation>, deadline_hit: bool) -> (FailureClass, String) {
    let classify = |message: &str| {
        classify_failure(&FailureFacts {
            message,
            ..FailureFacts::default()
        })
        .unwrap_or(FailureClass::Other)
    };
    let Some(node) = node else {
        return (
            FailureClass::Other,
            "its slot is missing from the turn".into(),
        );
    };
    let Some(latest) = node.latest() else {
        return if deadline_hit {
            (
                FailureClass::Budget,
                "the run's wall clock ran out before it started".into(),
            )
        } else {
            (FailureClass::NotReached, "it never started".into())
        };
    };
    if let Some(failure) = &latest.failure {
        let class = match failure.class {
            FailureClass::Other => classify(&failure.message),
            FailureClass::Stopped if deadline_hit => FailureClass::Budget,
            class => class,
        };
        return (class, failure.message.clone());
    }
    match latest.state {
        NodeState::Running | NodeState::NeverStarted | NodeState::Stopped if deadline_hit => (
            FailureClass::Budget,
            "the run's wall clock ran out before it finished".into(),
        ),
        NodeState::Stopped => (FailureClass::Stopped, "it was stopped".into()),
        NodeState::Blocked => (FailureClass::Blocked, "it was blocked".into()),
        NodeState::NeverStarted => (FailureClass::NotReached, "it never started".into()),
        NodeState::Failed => {
            let message = "it failed without a recorded reason";
            (classify(message), message.into())
        }
        NodeState::Accepted => (
            FailureClass::Other,
            "its accepted answer is empty or could not be read".into(),
        ),
        NodeState::Running => (FailureClass::Other, "it was still running".into()),
        NodeState::Superseded => (
            FailureClass::Other,
            "its answer was superseded without a replacement".into(),
        ),
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or_default()
}

/// One observed turn, and whether the wall clock stopped it.
struct Observed {
    index: usize,
    deadline_hit: bool,
}

struct Audit<'a> {
    host: &'a dyn RunHost,
    run: &'a RunContext,
    build: &'a EditBuilder,
    /// Applies made so far: the expected configuration revision of the
    /// next one, for a run Session that starts at revision 0.
    applies: u64,
    /// A turn that ended needing attention; it holds the Session until it
    /// is stopped.
    open_turn: Option<usize>,
    report: KindReport,
}

impl Audit<'_> {
    fn observation(&self, observed: &Observed) -> &TurnObservation {
        &self.report.turns[observed.index]
    }

    async fn phase(&self, phase: &str, detail: String) -> Result<(), RunError> {
        self.host
            .record(
                &self.run.run_id,
                RunEvent::Phase {
                    at_ms: now_ms(),
                    phase: phase.into(),
                    detail,
                },
            )
            .await
    }

    async fn not_covered(&mut self, entry: NotCovered) -> Result<(), RunError> {
        if entry.class == FailureClass::Budget {
            self.report.budget_exhausted = true;
        }
        self.host
            .record(
                &self.run.run_id,
                RunEvent::NotCovered {
                    at_ms: now_ms(),
                    entry: Box::new(entry.clone()),
                },
            )
            .await?;
        self.report.not_covered.push(entry);
        Ok(())
    }

    async fn findings(&mut self, findings: Vec<Finding>) -> Result<(), RunError> {
        for finding in findings {
            self.host
                .record(
                    &self.run.run_id,
                    RunEvent::Finding {
                        at_ms: now_ms(),
                        finding: Box::new(finding.clone()),
                    },
                )
                .await?;
            self.report.findings.push(finding);
        }
        Ok(())
    }

    /// Stop the turn that ended needing attention, so the Session can take
    /// the next Apply and turn. Its settled nodes were already read.
    async fn close_open_turn(&mut self) -> Result<(), RunError> {
        let Some(index) = self.open_turn.take() else {
            return Ok(());
        };
        let turn_id = self.report.turns[index].turn_id.clone();
        self.phase(
            "closing_turn",
            format!(
                "Stopping turn {turn_id}, which needs attention, so the audit can go on; \
                 its areas without a result are listed as not covered"
            ),
        )
        .await?;
        self.host.stop_turn(&self.run.session_id, &turn_id).await?;
        let closed = self
            .host
            .wait_turn(&self.run.session_id, &turn_id, Instant::now() + STOP_GRACE)
            .await?;
        self.report.turn_refs[index].state = closed.state;
        self.report.turns[index] = closed;
        Ok(())
    }

    async fn apply(&mut self, what: &str, slots: Vec<SlotPlan>) -> Result<(), RunError> {
        self.close_open_turn().await?;
        self.phase("applying_team", what.into()).await?;
        let edit = (self.build)(&self.run.resolved, &slots, false, self.applies)?;
        self.host
            .apply_team(&self.run.session_id, read_only_edit(edit))
            .await?;
        self.applies += 1;
        Ok(())
    }

    /// Send one turn and wait for it within the run's wall clock. `None`
    /// when the wall clock ran out before it could start.
    async fn turn(&mut self, request: &str, purpose: &str) -> Result<Option<Observed>, RunError> {
        self.close_open_turn().await?;
        if Instant::now() >= self.run.deadline {
            self.report.budget_exhausted = true;
            return Ok(None);
        }
        self.phase("running", purpose.into()).await?;
        let session = &self.run.session_id;
        let turn_id = self.host.send_turn(session, request).await?;
        self.host
            .record(
                &self.run.run_id,
                RunEvent::TurnStarted {
                    at_ms: now_ms(),
                    turn_id: turn_id.clone(),
                    purpose: purpose.into(),
                },
            )
            .await?;
        let mut observation = self
            .host
            .wait_turn(session, &turn_id, self.run.deadline)
            .await?;
        let deadline_hit = observation.state == TurnState::Running;
        if deadline_hit {
            self.report.budget_exhausted = true;
            self.phase(
                "running",
                format!("The run's wall clock ran out; stopping turn {turn_id}"),
            )
            .await?;
            self.host.stop_turn(session, &turn_id).await?;
            observation = self
                .host
                .wait_turn(session, &turn_id, Instant::now() + STOP_GRACE)
                .await?;
        }
        self.host
            .record(
                &self.run.run_id,
                RunEvent::TurnEnded {
                    at_ms: now_ms(),
                    turn_id: turn_id.clone(),
                    state: observation.state,
                },
            )
            .await?;
        let index = self.report.turns.len();
        if observation.state == TurnState::NeedsAttention {
            self.open_turn = Some(index);
        }
        self.report.turn_refs.push(RunTurnRef {
            turn_id,
            purpose: purpose.into(),
            state: observation.state,
        });
        self.report.turns.push(observation);
        Ok(Some(Observed {
            index,
            deadline_hit,
        }))
    }

    fn whole_scope(
        &self,
        class: FailureClass,
        detail: String,
        at: Option<&Observed>,
    ) -> NotCovered {
        let (node_id, turn_id) = match at {
            Some(observed) => {
                let observation = self.observation(observed);
                (
                    observation
                        .nodes
                        .iter()
                        .find(|node| node.slot_id == PLANNER_SLOT)
                        .map(|node| node.node_id.clone()),
                    Some(observation.turn_id.clone()),
                )
            }
            None => (None, None),
        };
        NotCovered {
            area: WHOLE_SCOPE.into(),
            class,
            detail,
            node_id,
            turn_id,
        }
    }

    /// Turn 1: the plan, with one retry after an invalid plan.
    async fn plan(&mut self, settings: &AuditSettings) -> Result<Option<AuditPlan>, RunError> {
        let (min, max) = (settings.min_areas, settings.max_areas);
        self.apply("audit plan: the planner", plan_slots(&self.run.resolved)?)
            .await?;
        let prompt = self.run.resolved.prompt.clone();
        let mut refused: Option<String> = None;
        for _attempt in 0..2 {
            let request = match &refused {
                None => plan_request(&prompt, min, max),
                Some(error) => plan_retry_request(&prompt, min, max, error),
            };
            let Some(observed) = self.turn(&request, PLAN_PURPOSE).await? else {
                let entry = self.whole_scope(
                    FailureClass::Budget,
                    "the run's wall clock ran out before the plan was ready".into(),
                    None,
                );
                self.not_covered(entry).await?;
                return Ok(None);
            };
            let observation = self.observation(&observed);
            let node = observation
                .nodes
                .iter()
                .find(|node| node.slot_id == PLANNER_SLOT);
            let Some(answer) = accepted_answer(node).map(str::to_owned) else {
                let (class, detail) = failure_of(node, observed.deadline_hit);
                let entry = self.whole_scope(
                    class,
                    format!("the planner has no answer: {detail}"),
                    Some(&observed),
                );
                self.not_covered(entry).await?;
                return Ok(None);
            };
            match parse_plan(&answer, min, max) {
                Ok(plan) => {
                    let names: Vec<&str> = plan.areas.iter().map(|a| a.name.as_str()).collect();
                    self.phase(
                        "planned",
                        format!("{} areas: {}", names.len(), names.join(", ")),
                    )
                    .await?;
                    return Ok(Some(plan));
                }
                Err(error) => {
                    let error = error.to_string();
                    self.phase("plan_refused", error.clone()).await?;
                    if refused.is_some() {
                        let entry = self.whole_scope(
                            FailureClass::Other,
                            format!(
                                "the planner's AREAS block was invalid twice; the last one: \
                                 {error}"
                            ),
                            Some(&observed),
                        );
                        self.not_covered(entry).await?;
                        return Ok(None);
                    }
                    refused = Some(error);
                }
            }
        }
        Ok(None)
    }

    /// Turn 2: every area at once.
    async fn areas(&mut self, plan: &AuditPlan) -> Result<Vec<AreaResult>, RunError> {
        let names: Vec<&str> = plan.areas.iter().map(|area| area.name.as_str()).collect();
        self.apply(
            &format!(
                "audit areas: {} read-only workers ({})",
                names.len(),
                names.join(", ")
            ),
            area_slots(&self.run.resolved, plan)?,
        )
        .await?;
        let request = areas_request(&self.run.resolved.prompt, plan);
        let Some(observed) = self.turn(&request, AREAS_PURPOSE).await? else {
            for area in &plan.areas {
                self.not_covered(NotCovered {
                    area: area.name.clone(),
                    class: FailureClass::Budget,
                    detail: "the run's wall clock ran out before the areas started".into(),
                    node_id: None,
                    turn_id: None,
                })
                .await?;
            }
            return Ok(Vec::new());
        };
        let mut results = Vec::new();
        for area in &plan.areas {
            let observation = self.observation(&observed);
            let turn_id = Some(observation.turn_id.clone());
            let slot = worker_slot_id(&area.name);
            let node = observation.nodes.iter().find(|node| node.slot_id == slot);
            let node_id = node.map(|node| node.node_id.clone());
            let Some(answer) = accepted_answer(node).map(str::to_owned) else {
                let (class, detail) = failure_of(node, observed.deadline_hit);
                self.not_covered(NotCovered {
                    area: area.name.clone(),
                    class,
                    detail: format!("the area worker has no result: {detail}"),
                    node_id,
                    turn_id,
                })
                .await?;
                continue;
            };
            match parse_area_report(&answer, &area.name) {
                Ok(report) => {
                    let classified = {
                        let (items, own, planned, repo) = (
                            report.not_reached.clone(),
                            area.clone(),
                            plan.clone(),
                            self.run.options.repo.clone(),
                        );
                        tokio::task::spawn_blocking(move || {
                            items
                                .into_iter()
                                .map(|item| {
                                    let kind = classify_not_reached(&item, &own, &planned, &repo);
                                    (item, kind)
                                })
                                .collect::<Vec<_>>()
                        })
                        .await
                        .map_err(|error| {
                            RunError::Infrastructure(format!(
                                "reading the {} worker's NOT_REACHED list: {error}",
                                area.name
                            ))
                        })?
                    };
                    let mut other_areas = Vec::new();
                    for (item, kind) in classified {
                        match kind {
                            NotReachedItem::OtherArea(name) => {
                                if !other_areas.contains(&name) {
                                    other_areas.push(name);
                                }
                            }
                            NotReachedItem::NoSuchPath(path) => {
                                self.phase(
                                    "note",
                                    format!(
                                        "{} listed {path} as not reached, and no such path \
                                         exists in the repository; a note, not a gap",
                                        worker_slot_id(&area.name)
                                    ),
                                )
                                .await?;
                            }
                            NotReachedItem::Gap => {
                                self.not_covered(NotCovered {
                                    area: area.name.clone(),
                                    class: FailureClass::NotReached,
                                    detail: format!(
                                        "{item} (the area worker reported it did not reach this)"
                                    ),
                                    node_id: node_id.clone(),
                                    turn_id: turn_id.clone(),
                                })
                                .await?;
                            }
                        }
                    }
                    if !other_areas.is_empty() {
                        self.phase(
                            "note",
                            format!(
                                "{} listed other planned areas as not reached ({}); their own \
                                 workers audit them, so they are not gaps",
                                worker_slot_id(&area.name),
                                other_areas.join(", ")
                            ),
                        )
                        .await?;
                    }
                    results.push(AreaResult {
                        area: area.clone(),
                        body: AreaBody::Report(report),
                    });
                }
                Err(error) => {
                    self.not_covered(NotCovered {
                        area: area.name.clone(),
                        class: FailureClass::Other,
                        detail: format!(
                            "the area worker's report could not be read ({error}); the \
                             integrator received its answer as text"
                        ),
                        node_id,
                        turn_id,
                    })
                    .await?;
                    results.push(AreaResult {
                        area: area.clone(),
                        body: AreaBody::Unreadable {
                            answer,
                            error: error.to_string(),
                        },
                    });
                }
            }
        }
        Ok(results)
    }

    /// The workers' own findings, reported when integration has no result.
    fn unmerged(results: &[AreaResult]) -> Vec<Finding> {
        results
            .iter()
            .filter_map(|result| match &result.body {
                AreaBody::Report(report) => Some(report.findings.iter().cloned()),
                AreaBody::Unreadable { .. } => None,
            })
            .flatten()
            .collect()
    }

    async fn integration_missing(
        &mut self,
        results: &[AreaResult],
        class: FailureClass,
        detail: String,
        at: Option<&Observed>,
    ) -> Result<(), RunError> {
        let (node_id, turn_id) = match at {
            Some(observed) => {
                let observation = self.observation(observed);
                (
                    observation
                        .nodes
                        .iter()
                        .find(|node| node.slot_id == INTEGRATOR_SLOT)
                        .map(|node| node.node_id.clone()),
                    Some(observation.turn_id.clone()),
                )
            }
            None => (None, None),
        };
        self.not_covered(NotCovered {
            area: INTEGRATOR_SLOT.into(),
            class,
            detail: format!("{detail}; the area workers' findings are reported unmerged"),
            node_id,
            turn_id,
        })
        .await?;
        self.findings(Self::unmerged(results)).await
    }

    /// Turn 3: merge every report.
    async fn integrate(
        &mut self,
        plan: &AuditPlan,
        results: &[AreaResult],
    ) -> Result<(), RunError> {
        if results.is_empty() {
            return self
                .phase(
                    "integrating",
                    "skipped: no area worker produced a report".into(),
                )
                .await;
        }
        if Instant::now() >= self.run.deadline {
            return self
                .integration_missing(
                    results,
                    FailureClass::Budget,
                    "the run's wall clock ran out before integration".into(),
                    None,
                )
                .await;
        }
        self.phase(
            "integrating",
            format!("one integrator merges {} area reports", results.len()),
        )
        .await?;
        self.apply(
            "audit integration: the integrator",
            integrate_slots(&self.run.resolved)?,
        )
        .await?;
        let request = integrate_request(
            &self.run.resolved.prompt,
            plan,
            results,
            &self.report.not_covered,
        );
        let Some(observed) = self.turn(&request, INTEGRATE_PURPOSE).await? else {
            return self
                .integration_missing(
                    results,
                    FailureClass::Budget,
                    "the run's wall clock ran out before integration".into(),
                    None,
                )
                .await;
        };
        let observation = self.observation(&observed);
        let node = observation
            .nodes
            .iter()
            .find(|node| node.slot_id == INTEGRATOR_SLOT);
        let Some(answer) = accepted_answer(node).map(str::to_owned) else {
            let (class, detail) = failure_of(node, observed.deadline_hit);
            return self
                .integration_missing(
                    results,
                    class,
                    format!("the integrator has no result: {detail}"),
                    Some(&observed),
                )
                .await;
        };
        match parse_integrated(&answer) {
            Ok(findings) => self.findings(findings).await,
            Err(error) => {
                self.integration_missing(
                    results,
                    FailureClass::Other,
                    format!("the integrator's FINDINGS block could not be read ({error})"),
                    Some(&observed),
                )
                .await
            }
        }
    }
}

#[cfg(test)]
#[path = "audit_tests.rs"]
mod tests;
