//! The audit loadout: a plan, the areas with their follow-ups, and an
//! integration, each turn in the run's one Session after its own Team and
//! budget Apply. Coverage is the host's, from the files it listed and the
//! tool calls the Session recorded ([`files`]), never the workers' word.
//!
//! 1. **Plan**: the `planner` slot alone answers with an `AREAS` block of
//!    `min_areas`..`max_areas` areas. An invalid plan, or a planner without
//!    an answer (a provider failure, say), gets one retry turn (quoting the
//!    parse error after an invalid plan); a second attempt without a usable
//!    plan ends the run with the whole scope not covered. A planner stopped
//!    by the wall clock or a person, or refused by its provider (400-403),
//!    is not retried.
//! 2. **Assignment**: the host lists the repository's files and assigns
//!    each to one area ([`files::assign`]): the first area whose paths
//!    match it, else the area whose paths share the most directories with
//!    it, else the host-made area `rest`. The plan as executed (the planned
//!    areas that got files, then `rest`) and the assignment are `assigned`
//!    phase events; a planned area without files is not run (a note).
//! 3. **Areas**: one `worker-<area>` slot per executed area, instantiated
//!    from the `worker` Agent: read-only (`writes: []`; its commands run
//!    under the supervisor's write restriction), a fresh context
//!    (`reset_history`), no dependencies, required, no checks and no
//!    review, so the controller starts every one at once. Each worker's
//!    instructions name its files ([`files::file_list`]).
//! 4. **Follow-ups**: when a worker's turn ends with text files of its area
//!    unread ([`files::unread`], judged from the Session's recorded
//!    `read_file` calls of the worker's activations that answered), the
//!    host runs up to [`MAX_FOLLOW_UPS`] follow-up turns, each with a fresh
//!    activation of every such worker naming exactly its unread files; its
//!    findings join the area's report. A file still unread after them, or
//!    when the wall clock or a person's stop ends them, is not covered,
//!    listed by file. A worker without a result, an unreadable report and
//!    a record of tool calls that cannot be read are listed too. What a
//!    worker says it did not reach (`NOT_REACHED`) is a note: the host's
//!    coverage decides. Empty and binary files need no read; a file over
//!    [`files::MAX_AUDITED_FILE_BYTES`] is a note.
//! 5. **Integrate**: the `integrator` slot alone receives every worker's
//!    report (each bounded to 24 KiB, truncation noted) and the not-covered
//!    list, and answers with the merged `FINDINGS`. When integration has no
//!    readable result, the workers' findings are reported unmerged and
//!    [`INTEGRATION`] is listed as not covered, which the attention line
//!    names apart from the areas.
//!
//! The run's wall clock bounds every turn: at the deadline the turn is
//! stopped, what did not finish is not covered (budget), and no further
//! turn starts. A person's stop starts no further turn either: areas
//! planned but not started, files no follow-up read and an integration not
//! run are not covered (`stopped`), and the report keeps everything
//! observed until the stop. Findings change the exit code only with
//! `fail_on_findings` (default false); anything not covered always needs
//! attention.
//!
//! Owner: audit.

use std::fmt::Write as _;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use axocoatl_config::loadout::{AuditSettings, LoadoutAgent, LoadoutRole, ResolvedLoadout};
use axocoatl_session::audit_plan::{
    parse_area_report, parse_integrated, parse_plan, AreaReport, AuditArea, AuditPlan,
    MAX_AREA_FINDINGS,
};
use axocoatl_session::failure_class::{classify_failure, FailureFacts};
use axocoatl_session::run_outcome::{
    FailureClass, Finding, NodeObservation, NodeState, NotCovered, RunTurnRef, TurnObservation,
    TurnState,
};
use axocoatl_session::run_record::RunEvent;

use super::team_plan::{self, SlotPlan};
use super::{KindDriver, KindReport, RunContext, RunError, RunHost, ToolCallRecord};
use crate::SessionTeamEdit;

#[path = "audit_files.rs"]
pub mod files;

use files::{AssignedArea, Assignment, FileKind, RepoFile, MAX_FILE_LIST_BYTES};

/// Slot of the plan turn.
pub const PLANNER_SLOT: &str = "planner";
/// Slot of the integrate turn.
pub const INTEGRATOR_SLOT: &str = "integrator";
/// Prefix of each area worker's slot: `worker-<area>`.
pub const WORKER_SLOT_PREFIX: &str = "worker-";
/// Largest worker report the integrator receives, in bytes.
pub const MAX_REPORT_BYTES: usize = 24 * 1024;
/// The not-covered entry of a run whose plan never became usable.
pub const WHOLE_SCOPE: &str = axocoatl_session::run_outcome::WHOLE_SCOPE;
/// The not-covered entry of an integration without a readable result.
pub const INTEGRATION: &str = axocoatl_session::run_outcome::INTEGRATION;
/// The not-covered entry of the files the host could not list: past its
/// listing bound, or with names that are not UTF-8.
pub const UNLISTED: &str = "unlisted files";
/// Follow-up activations a worker gets for the files of its area it did
/// not read.
pub const MAX_FOLLOW_UPS: u32 = 2;
/// `RunTurnRef::purpose` of each turn.
pub const PLAN_PURPOSE: &str = "audit_plan";
pub const AREAS_PURPOSE: &str = "audit_areas";
pub const FOLLOW_UP_PURPOSE: &str = "audit_follow_up";
pub const INTEGRATE_PURPOSE: &str = "audit_integrate";
/// How long a stopped turn may take to settle before it is observed.
const STOP_GRACE: Duration = Duration::from_secs(30);
/// Not-covered entries listed in the integrate request; the rest are counted.
const MAX_LISTED_NOT_COVERED: usize = 64;
const MAX_LISTED_DETAIL_BYTES: usize = 300;
/// Unread files of one area listed one by one as not covered; the rest are
/// one entry naming their directories.
const MAX_LISTED_UNREAD: usize = 100;
/// Files too large to read noted one by one per area; the rest are counted.
const MAX_NOTED_TOO_LARGE: usize = 20;
/// The longest list of paths in one record line (an `assigned` or
/// `coverage` phase, a note).
const MAX_INLINE_LIST_BYTES: usize = 2 * 1024;
/// `NOT_REACHED` entries quoted in one note, and the longest quoted.
const MAX_NOTED_NOT_REACHED: usize = 16;
const MAX_NOTED_ITEM_BYTES: usize = 200;

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

/// Run the audit's turns with `build` making each turn's edit.
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
    // A person's stop ends the drive, but what it observed until then (its
    // turns, not-covered entries, findings and notes) stays in the report.
    match audit.run(&settings).await {
        Ok(()) => {}
        Err(RunError::Stopped) => audit.report.stopped = true,
        Err(error) => return Err(error),
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

/// The areas turn's team: one fresh read-only worker per area of the plan
/// as executed, none depending on another, each told its files.
pub fn area_slots(
    resolved: &ResolvedLoadout,
    assignment: &Assignment,
) -> Result<Vec<SlotPlan>, RunError> {
    let worker = agent(resolved, LoadoutRole::Worker)?;
    let names = assignment.names();
    assignment
        .areas
        .iter()
        .map(|assigned| {
            let others: Vec<&str> = names
                .iter()
                .copied()
                .filter(|name| *name != assigned.area.name)
                .collect();
            read_only_slot(
                resolved,
                worker_slot_id(&assigned.area.name),
                worker,
                Some(worker_instructions(
                    worker.instructions.as_deref(),
                    assigned,
                    &others,
                )),
            )
        })
        .collect()
}

/// A follow-up turn's team: one fresh read-only worker per area with files
/// left unread, each told exactly those files.
pub fn follow_up_slots(
    resolved: &ResolvedLoadout,
    due: &[(&AuditArea, Vec<&str>)],
    round: u32,
) -> Result<Vec<SlotPlan>, RunError> {
    let worker = agent(resolved, LoadoutRole::Worker)?;
    due.iter()
        .map(|(area, unread)| {
            read_only_slot(
                resolved,
                worker_slot_id(&area.name),
                worker,
                Some(follow_up_instructions(
                    worker.instructions.as_deref(),
                    area,
                    unread,
                    round,
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
         fresh context, so give each a self-contained scope. The host lists the repository's \
         files and gives each to the first area whose paths match it (a file no area's paths \
         match goes to the area that shares its directories, or to an area of its own), and \
         each worker must read every file it is given, so name each area's files in its \
         paths. Names are lowercase letters, digits and hyphens, start with a letter, are at \
         most 32 characters and unique.\n{AREAS_SHAPE}\n"
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

/// How a worker reads, as the host checks it.
const READ_RULES: &str = "The host checks your read_file calls, not your answer: a file \
     counts as examined only when a read_file of it succeeded. read_file returns at most 64 \
     KiB from its offset (0 by default), or limit bytes; read a longer file to its end with \
     more calls at each result's next_offset, with a smaller limit when a whole 64 KiB would \
     not fit your context. grep, glob, list_dir and bash find and search files, but what they \
     show does not count as reading. The host names any file of yours you did not read back \
     to you to read.\n";

/// The blocks a worker's answer ends with.
const REPORT_SHAPE: &str = "\nEnd your answer with two blocks:\nFINDINGS\n```json\n[{\"id\": \
     \"F1\", \"title\": \"...\", \"detail\": \"what is wrong and your evidence\", \"severity\": \
     \"low|medium|high|critical\", \"location\": \"path:line\"}]\n```\nNOT_REACHED\n```json\n\
     [\"each file of yours you could not examine, and why\"]\n```\nWrite [] for a block with \
     no entries.";

fn instructions_head(base: Option<&str>, area: &AuditArea) -> String {
    let mut text = String::new();
    if let Some(base) = base.map(str::trim).filter(|base| !base.is_empty()) {
        text.push_str(base);
        text.push_str("\n\n");
    }
    let _ = writeln!(text, "Your area: {}\nScope: {}", area.name, area.scope);
    text
}

/// An area worker's instructions: the loadout's worker instructions, its
/// area, the files the host assigned to it, the other areas, how the host
/// checks its reads and the report it ends with.
pub fn worker_instructions(base: Option<&str>, assigned: &AssignedArea, others: &[&str]) -> String {
    let area = &assigned.area;
    let mut text = instructions_head(base, area);
    if assigned.host_made {
        text.push_str("The host made this area for the files no planned area's paths name.\n");
    } else if area.paths.is_empty() {
        text.push_str("Paths: the plan names none.\n");
    } else {
        text.push_str("Paths:\n");
        for path in &area.paths {
            let _ = writeln!(text, "- {path}");
        }
    }
    let to_read: Vec<&str> = assigned
        .text_files()
        .map(|file| file.path.as_str())
        .collect();
    let skipped = assigned.files.len() - to_read.len();
    if to_read.is_empty() {
        text.push_str(
            "Your files: the host listed the repository, and none of the files it gave your \
             area needs a read (each is empty, binary or over 256 KiB).\n",
        );
    } else {
        let (list, grouped) = files::file_list(&to_read, MAX_FILE_LIST_BYTES);
        if grouped {
            let _ = writeln!(
                text,
                "Your files: the host listed the repository and gave your area {} files to \
                 read, too many to name one by one. Read all of them: every file of yours in or \
                 below these directories (each count is of your files there):",
                to_read.len()
            );
        } else {
            let _ = writeln!(
                text,
                "Your files: the host listed the repository and gave your area {} file{} to \
                 read. Read every one of them:",
                to_read.len(),
                if to_read.len() == 1 { "" } else { "s" }
            );
        }
        text.push_str(&list);
        if skipped > 0 {
            let _ = writeln!(
                text,
                "({skipped} more file{} of your area {} empty, binary or over 256 KiB and \
                 need{} no read.)",
                if skipped == 1 { "" } else { "s" },
                if skipped == 1 { "is" } else { "are" },
                if skipped == 1 { "s" } else { "" }
            );
        }
    }
    if others.is_empty() {
        text.push_str("Stay inside your area. You are read-only: change no file.\n");
    } else {
        let _ = writeln!(
            text,
            "Other workers audit the other areas ({}) at the same time, each with a fresh \
             context: stay inside yours. You are read-only: change no file.",
            others.join(", ")
        );
    }
    text.push_str(READ_RULES);
    text.push_str(REPORT_SHAPE);
    text
}

/// A follow-up activation's instructions: its area and exactly the files of
/// it the worker did not read.
pub fn follow_up_instructions(
    base: Option<&str>,
    area: &AuditArea,
    unread: &[&str],
    round: u32,
) -> String {
    let mut text = instructions_head(base, area);
    let (list, grouped) = files::file_list(unread, MAX_FILE_LIST_BYTES);
    if grouped {
        let _ = writeln!(
            text,
            "Follow-up {round} of at most {MAX_FOLLOW_UPS}: the host checked the read_file calls \
             of your area's worker, and {} files of your area were not read, too many to name \
             one by one: every unread file of yours in or below these directories (each count \
             is of those files). Read these files and report additional findings in the same \
             format:",
            unread.len()
        );
    } else {
        let _ = writeln!(
            text,
            "Follow-up {round} of at most {MAX_FOLLOW_UPS}: the host checked the read_file calls \
             of your area's worker, and these files of your area were not read. Read these \
             files and report additional findings in the same format:"
        );
    }
    text.push_str(&list);
    text.push_str("You are read-only: change no file.\n");
    text.push_str(READ_RULES);
    text.push_str(REPORT_SHAPE);
    text
}

/// The areas turn's request (each worker's own instructions name its area
/// and its files).
pub fn areas_request(prompt: &str, assignment: &Assignment) -> String {
    let names = assignment.names();
    format!(
        "{}\n\nThis turn audits the {} areas of the plan as executed in parallel: {}. The host \
         listed the repository's files and gave each to one area. Your instructions name your \
         area and its files: read them, audit only that area, and end with the FINDINGS and \
         NOT_REACHED blocks your instructions describe.",
        prompt.trim_end(),
        names.len(),
        names.join(", ")
    )
}

/// A follow-up turn's request.
pub fn follow_up_request(prompt: &str, names: &[&str], round: u32) -> String {
    format!(
        "{}\n\nFollow-up {round} of the audit's areas ({}): the host found files of these areas \
         that their workers did not read. Your instructions name the files of yours to read. \
         Read them and end with the FINDINGS and NOT_REACHED blocks your instructions describe.",
        prompt.trim_end(),
        names.join(", ")
    )
}

/// What one area of the plan as executed produced, for the integrator.
#[derive(Debug, Clone, PartialEq)]
pub struct AreaResult {
    pub area: AuditArea,
    /// Its worker's readable reports merged into one, then each answer
    /// that could not be read.
    pub bodies: Vec<AreaBody>,
    /// Text files of the area its worker had to read.
    pub files: usize,
    /// Of those, the ones it did not read; `None` when its record of tool
    /// calls could not be read. The integrator is told.
    pub unread: Option<usize>,
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
    assignment: &Assignment,
    results: &[AreaResult],
    not_covered: &[NotCovered],
) -> String {
    let names = assignment.names();
    let mut text = String::new();
    text.push_str(prompt.trim_end());
    let _ = writeln!(
        text,
        "\n\nThe area workers of this audit have finished. Areas audited: {}.\n\
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
        match result.unread {
            None => text.push_str(
                "Whether its worker read this area's files is not known, so the area is not \
                 covered: keep a finding of it only if you verify it in the repository.\n",
            ),
            Some(0) => {}
            Some(unread) => {
                let _ = writeln!(
                    text,
                    "Its worker did not read {unread} of this area's {} files to read (they are \
                     listed as not covered): keep a finding of those only if you verify it in \
                     the repository.",
                    result.files
                );
            }
        }
        for body in &result.bodies {
            text.push_str(&report_text(body));
        }
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

/// `paths` as one line, `a, b, c`, within [`MAX_INLINE_LIST_BYTES`]: past
/// it, the directories they are in with counts.
fn inline_list(paths: &[&str]) -> String {
    let (list, _) = files::file_list(paths, MAX_INLINE_LIST_BYTES);
    list.lines()
        .map(|line| line.strip_prefix("- ").unwrap_or(line))
        .collect::<Vec<_>>()
        .join(", ")
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

/// A failure that would end a follow-up the same way: the wall clock, a
/// person's stop, or a provider refusing the request itself (400-403).
fn final_failure(class: FailureClass) -> bool {
    matches!(
        class,
        FailureClass::Budget | FailureClass::Stopped | FailureClass::ProviderRejected
    )
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

/// One area of the plan as executed, across its worker's activations.
struct AreaState {
    assigned: AssignedArea,
    /// The tool calls of its worker's activations that answered.
    calls: Vec<ToolCallRecord>,
    /// Each readable report, in order; a follow-up's finding ids say so.
    reports: Vec<AreaReport>,
    /// Each accepted answer that could not be read, with why.
    unreadable: Vec<(String, String)>,
    /// Activations so far: its turn, then each follow-up.
    activations: u32,
    follow_ups: u32,
    /// An activation of it answered.
    answered: bool,
    /// Why its latest activation has no result, when it has none.
    failure: Option<(FailureClass, String)>,
    /// Its record of tool calls could not be read: coverage is not known.
    unknown: Option<String>,
    /// Its latest node and turn, for the not-covered entries.
    node_id: Option<String>,
    turn_id: Option<String>,
}

impl AreaState {
    fn new(assigned: &AssignedArea) -> Self {
        Self {
            assigned: assigned.clone(),
            calls: Vec::new(),
            reports: Vec::new(),
            unreadable: Vec::new(),
            activations: 0,
            follow_ups: 0,
            answered: false,
            failure: None,
            unknown: None,
            node_id: None,
            turn_id: None,
        }
    }

    fn name(&self) -> &str {
        &self.assigned.area.name
    }

    /// Its text files no activation that answered read.
    fn unread(&self, repo: &std::path::Path) -> Vec<&RepoFile> {
        let calls: Vec<&ToolCallRecord> = self.calls.iter().collect();
        files::unread(&self.assigned.files, &calls, repo)
    }

    /// Whether another follow-up could read its unread files.
    fn may_follow_up(&self, reads: bool) -> bool {
        reads
            && self.unknown.is_none()
            && self.follow_ups < MAX_FOLLOW_UPS
            && !self
                .failure
                .as_ref()
                .is_some_and(|(class, _)| final_failure(*class))
    }
}

/// What the activation of one area worker in a turn did.
struct Activation {
    index: usize,
    node_id: Option<String>,
    answer: Option<String>,
    generation: Option<u32>,
    failure: (FailureClass, String),
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
    /// The turns, in order.
    async fn run(&mut self, settings: &AuditSettings) -> Result<(), RunError> {
        let Some(plan) = self.plan(settings).await? else {
            return Ok(());
        };
        let Some(assignment) = self.assign(&plan).await? else {
            return Ok(());
        };
        let (results, stopped) = self.areas(&assignment).await?;
        if stopped {
            self.integration_missing(
                &results,
                FailureClass::Stopped,
                "the run was stopped before integration".into(),
                None,
            )
            .await?;
            return Err(RunError::Stopped);
        }
        self.integrate(&assignment, &results).await
    }

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

    /// A note: a `note` phase event in the record (and on standard error)
    /// and a line of the Outcome's notes.
    async fn note(&mut self, detail: String) -> Result<(), RunError> {
        self.phase("note", detail.clone()).await?;
        self.report.notes.push(detail);
        Ok(())
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
        // A person who stopped the run gets no further Apply.
        if self.host.stop_requested(&self.run.run_id).await {
            return Err(RunError::Stopped);
        }
        self.phase("applying_team", what.into()).await?;
        let edit = (self.build)(&self.run.resolved, &slots, false, self.applies)?;
        self.host
            .apply_team(&self.run.session_id, read_only_edit(edit))
            .await?;
        self.applies += 1;
        Ok(())
    }

    /// Apply `slots`, then send `request` as one turn ([`Self::turn`]).
    async fn apply_and_turn(
        &mut self,
        what: &str,
        slots: Vec<SlotPlan>,
        request: &str,
        purpose: &str,
    ) -> Result<Option<Observed>, RunError> {
        self.apply(what, slots).await?;
        self.turn(request, purpose).await
    }

    /// Send one turn and wait for it within the run's wall clock. `None`
    /// when the wall clock ran out before it could start.
    async fn turn(&mut self, request: &str, purpose: &str) -> Result<Option<Observed>, RunError> {
        self.close_open_turn().await?;
        // A person who stopped the run gets no further turn.
        if self.host.stop_requested(&self.run.run_id).await {
            return Err(RunError::Stopped);
        }
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

    /// Turn 1: the plan, with one retry after an invalid plan or a planner
    /// without an answer.
    async fn plan(&mut self, settings: &AuditSettings) -> Result<Option<AuditPlan>, RunError> {
        let (min, max) = (settings.min_areas, settings.max_areas);
        self.apply("audit plan: the planner", plan_slots(&self.run.resolved)?)
            .await?;
        let prompt = self.run.resolved.prompt.clone();
        // Why the attempt before this one gave no plan: its parse error,
        // which the retry quotes, or its missing answer.
        let mut refused: Option<String> = None;
        let mut failed: Option<String> = None;
        for attempt in 0..2 {
            let last = attempt == 1;
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
                // The wall clock, a person's stop and a provider's refusal
                // of the request itself (400-403) end the same way again.
                let retry = !last
                    && !observed.deadline_hit
                    && !final_failure(class)
                    && !self.host.stop_requested(&self.run.run_id).await;
                if !retry {
                    let detail = match (&failed, &refused) {
                        (Some(first), _) => format!(
                            "the planner has no answer in two attempts: {detail} (the first: \
                             {first})"
                        ),
                        (None, Some(error)) => format!(
                            "the planner has no answer after its AREAS block was refused \
                             ({error}): {detail}"
                        ),
                        (None, None) => format!("the planner has no answer: {detail}"),
                    };
                    let entry = self.whole_scope(class, detail, Some(&observed));
                    self.not_covered(entry).await?;
                    return Ok(None);
                }
                // The retry stands for this attempt: its failure is in the
                // run's phases, and in the whole scope's entry if the retry
                // fails too, never a separate gap.
                if let Some(node) = node {
                    self.report
                        .accounted
                        .push((observation.turn_id.clone(), node.node_id.clone()));
                }
                self.phase(
                    "plan_failed",
                    format!(
                        "the planner has no answer ({}: {detail}); it gets one more turn",
                        class_name(class)
                    ),
                )
                .await?;
                failed = Some(format!("{}: {detail}", class_name(class)));
                refused = None;
                continue;
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
                    if last {
                        let detail = match &failed {
                            Some(first) => format!(
                                "the planner's AREAS block was invalid after it had no answer \
                                 ({first}): {error}"
                            ),
                            None => format!(
                                "the planner's AREAS block was invalid twice; the last one: \
                                 {error}"
                            ),
                        };
                        let entry = self.whole_scope(FailureClass::Other, detail, Some(&observed));
                        self.not_covered(entry).await?;
                        return Ok(None);
                    }
                    refused = Some(error);
                }
            }
        }
        Ok(None)
    }

    /// The host lists the repository and assigns its files to the plan's
    /// areas; the plan as executed and the assignment are recorded. `None`
    /// when no area is left to run.
    async fn assign(&mut self, plan: &AuditPlan) -> Result<Option<Assignment>, RunError> {
        let listing = match files::list_repository(&self.run.options.repo).await {
            Ok(listing) => listing,
            Err(error) => {
                let entry = self.whole_scope(
                    FailureClass::Other,
                    format!(
                        "the host could not list the repository's files, so no worker's \
                         coverage can be checked: {error}"
                    ),
                    None,
                );
                self.not_covered(entry).await?;
                return Ok(None);
            }
        };
        let assignment = files::assign(plan, &listing.files);
        let counts: Vec<String> = assignment
            .areas
            .iter()
            .map(|assigned| {
                format!(
                    "{} {}{}",
                    assigned.area.name,
                    assigned.files.len(),
                    if assigned.host_made {
                        " (host-made)"
                    } else {
                        ""
                    }
                )
            })
            .collect();
        self.phase(
            "assigned",
            format!(
                "{} file{} listed by {} in {} area{}: {}",
                listing.files.len(),
                if listing.files.len() == 1 { "" } else { "s" },
                listing.method.describe(),
                assignment.areas.len(),
                if assignment.areas.len() == 1 { "" } else { "s" },
                if counts.is_empty() {
                    "none".to_owned()
                } else {
                    counts.join(", ")
                }
            ),
        )
        .await?;
        for assigned in &assignment.areas {
            let shown: Vec<String> = assigned
                .files
                .iter()
                .map(|file| match file.kind {
                    FileKind::Text => file.path.clone(),
                    kind => format!("{} ({})", file.path, kind.label()),
                })
                .collect();
            let shown: Vec<&str> = shown.iter().map(String::as_str).collect();
            let paths = if assigned.host_made {
                "host-made for the files no planned area's paths name".to_owned()
            } else if assigned.area.paths.is_empty() {
                "no paths".to_owned()
            } else {
                format!("paths {}", assigned.area.paths.join(", "))
            };
            self.phase(
                "assigned",
                format!("{} ({paths}): {}", assigned.area.name, inline_list(&shown)),
            )
            .await?;
        }
        for assigned in &assignment.areas {
            if assigned.host_made {
                let paths: Vec<&str> = assigned.files.iter().map(|f| f.path.as_str()).collect();
                self.note(format!(
                    "the host made area {} for {} file{} no planned area's paths name and that \
                     share no directory with them: {}",
                    assigned.area.name,
                    paths.len(),
                    if paths.len() == 1 { "" } else { "s" },
                    inline_list(&paths)
                ))
                .await?;
            } else if !assigned.by_path.is_empty() {
                let paths: Vec<&str> = assigned.by_path.iter().map(String::as_str).collect();
                self.note(format!(
                    "{} file{} no area's paths match {} assigned to {}, whose paths share {} \
                     directories: {}",
                    paths.len(),
                    if paths.len() == 1 { "" } else { "s" },
                    if paths.len() == 1 { "was" } else { "were" },
                    assigned.area.name,
                    if paths.len() == 1 { "its" } else { "their" },
                    inline_list(&paths)
                ))
                .await?;
            }
        }
        if !assignment.without_files.is_empty() {
            let names = assignment.without_files.join(", ");
            self.note(format!(
                "planned area{} {names} got no file (no file its paths name is left to it), so \
                 {} not run",
                if assignment.without_files.len() == 1 {
                    ""
                } else {
                    "s"
                },
                if assignment.without_files.len() == 1 {
                    "it was"
                } else {
                    "they were"
                }
            ))
            .await?;
        }
        if listing.capped {
            self.not_covered(NotCovered {
                area: UNLISTED.into(),
                class: FailureClass::Other,
                detail: format!(
                    "the repository has more than {} files; the host listed and assigned the \
                     first {} by path, and the rest were given to no worker",
                    files::MAX_LISTED_FILES,
                    files::MAX_LISTED_FILES
                ),
                node_id: None,
                turn_id: None,
            })
            .await?;
        }
        if listing.unnamed > 0 {
            self.not_covered(NotCovered {
                area: UNLISTED.into(),
                class: FailureClass::Other,
                detail: format!(
                    "{} name{} in the repository {} not UTF-8, so no worker could name {} to \
                     read_file; {} not audited",
                    listing.unnamed,
                    if listing.unnamed == 1 { "" } else { "s" },
                    if listing.unnamed == 1 { "is" } else { "are" },
                    if listing.unnamed == 1 { "it" } else { "them" },
                    if listing.unnamed == 1 {
                        "it was"
                    } else {
                        "they were"
                    },
                ),
                node_id: None,
                turn_id: None,
            })
            .await?;
        }
        if assignment.areas.is_empty() {
            self.note("the repository has no files to audit".into())
                .await?;
            return Ok(None);
        }
        Ok(Some(assignment))
    }

    /// Turn 2 and the follow-ups: every area at once, then, up to
    /// [`MAX_FOLLOW_UPS`] times, a fresh activation of each worker that left
    /// files of its area unread, naming them. Returns each area's result and
    /// whether a person stopped the run.
    async fn areas(
        &mut self,
        assignment: &Assignment,
    ) -> Result<(Vec<AreaResult>, bool), RunError> {
        let names = assignment.names();
        let what = format!(
            "audit areas: {} read-only workers ({})",
            names.len(),
            names.join(", ")
        );
        let slots = area_slots(&self.run.resolved, assignment)?;
        let request = areas_request(&self.run.resolved.prompt, assignment);
        let started = match self
            .apply_and_turn(&what, slots, &request, AREAS_PURPOSE)
            .await
        {
            Err(RunError::Stopped) => {
                for assigned in &assignment.areas {
                    self.not_covered(NotCovered {
                        area: assigned.area.name.clone(),
                        class: FailureClass::Stopped,
                        detail: "the run was stopped before the areas started".into(),
                        node_id: None,
                        turn_id: None,
                    })
                    .await?;
                }
                return Err(RunError::Stopped);
            }
            started => started?,
        };
        let Some(observed) = started else {
            for assigned in &assignment.areas {
                self.not_covered(NotCovered {
                    area: assigned.area.name.clone(),
                    class: FailureClass::Budget,
                    detail: "the run's wall clock ran out before the areas started".into(),
                    node_id: None,
                    turn_id: None,
                })
                .await?;
            }
            return Ok((Vec::new(), false));
        };
        let reads = agent(&self.run.resolved, LoadoutRole::Worker)?
            .tools
            .iter()
            .any(|tool| tool == "read_file");
        let repo = self.run.options.repo.clone();
        let mut states: Vec<AreaState> = assignment.areas.iter().map(AreaState::new).collect();
        let every: Vec<usize> = (0..states.len()).collect();
        self.observe_workers(&observed, &mut states, &every).await?;
        let mut deadline_hit = observed.deadline_hit;
        let mut stopped = false;
        for round in 1..=MAX_FOLLOW_UPS {
            if deadline_hit {
                break;
            }
            let due: Vec<usize> = states
                .iter()
                .enumerate()
                .filter(|(_, state)| state.may_follow_up(reads) && !state.unread(&repo).is_empty())
                .map(|(index, _)| index)
                .collect();
            if due.is_empty() {
                break;
            }
            let (slots, names) = {
                let unread: Vec<(&AuditArea, Vec<&str>)> = due
                    .iter()
                    .map(|index| {
                        let state = &states[*index];
                        (
                            &state.assigned.area,
                            state
                                .unread(&repo)
                                .into_iter()
                                .map(|file| file.path.as_str())
                                .collect(),
                        )
                    })
                    .collect();
                let names: Vec<String> = unread
                    .iter()
                    .map(|(area, files)| format!("{} ({} unread)", area.name, files.len()))
                    .collect();
                (follow_up_slots(&self.run.resolved, &unread, round)?, names)
            };
            for index in &due {
                states[*index].follow_ups += 1;
            }
            let area_names: Vec<&str> = due.iter().map(|index| states[*index].name()).collect();
            let request = follow_up_request(&self.run.resolved.prompt, &area_names, round);
            let what = format!(
                "audit follow-up {round}: {} read-only worker{} ({})",
                due.len(),
                if due.len() == 1 { "" } else { "s" },
                names.join(", ")
            );
            match self
                .apply_and_turn(&what, slots, &request, FOLLOW_UP_PURPOSE)
                .await
            {
                Err(RunError::Stopped) => {
                    // The follow-up never started: it is not counted.
                    for index in &due {
                        states[*index].follow_ups -= 1;
                    }
                    stopped = true;
                    break;
                }
                Err(error) => return Err(error),
                Ok(None) => {
                    for index in &due {
                        states[*index].follow_ups -= 1;
                    }
                    deadline_hit = true;
                    break;
                }
                Ok(Some(observed)) => {
                    self.observe_workers(&observed, &mut states, &due).await?;
                    deadline_hit = observed.deadline_hit;
                }
            }
        }
        let results = self.conclude(&states, reads, stopped, deadline_hit).await?;
        Ok((results, stopped))
    }

    /// Read what each area worker of `which` did in the observed turn: its
    /// answer and report, or why it has none, and the tool calls of its
    /// activation when it answered.
    async fn observe_workers(
        &mut self,
        observed: &Observed,
        states: &mut [AreaState],
        which: &[usize],
    ) -> Result<(), RunError> {
        let turn_id = self.observation(observed).turn_id.clone();
        // The tool calls the Session recorded for the turn, read once.
        let recorded: Result<Vec<ToolCallRecord>, String> =
            match self.host.tool_calls(&self.run.session_id, &turn_id).await {
                Ok(Some(calls)) => Ok(calls),
                Ok(None) => Err("the Session's record of tool calls could not be read".into()),
                Err(error) => Err(format!(
                    "the Session's record of tool calls could not be read: {error}"
                )),
            };
        let activations: Vec<Activation> = {
            let observation = self.observation(observed);
            which
                .iter()
                .map(|index| {
                    let slot = worker_slot_id(states[*index].name());
                    let node = observation.nodes.iter().find(|node| node.slot_id == slot);
                    Activation {
                        index: *index,
                        node_id: node.map(|node| node.node_id.clone()),
                        answer: accepted_answer(node).map(str::to_owned),
                        generation: node
                            .and_then(|node| node.latest())
                            .map(|latest| latest.generation),
                        failure: failure_of(node, observed.deadline_hit),
                    }
                })
                .collect()
        };
        for activation in activations {
            let state = &mut states[activation.index];
            state.activations += 1;
            state.node_id = activation.node_id.clone();
            state.turn_id = Some(turn_id.clone());
            let slot = worker_slot_id(state.name());
            let Some(answer) = activation.answer else {
                let (class, detail) = activation.failure;
                // The audit lists what this worker left not covered itself.
                if let Some(node_id) = &activation.node_id {
                    self.report
                        .accounted
                        .push((turn_id.clone(), node_id.clone()));
                }
                if state.answered {
                    self.phase(
                        "follow_up_failed",
                        format!(
                            "{slot} has no result in follow-up {} ({}: {})",
                            state.follow_ups,
                            class_name(class),
                            cut(&detail, MAX_LISTED_DETAIL_BYTES).0
                        ),
                    )
                    .await?;
                }
                state.failure = Some((class, detail));
                continue;
            };
            state.answered = true;
            state.failure = None;
            match &recorded {
                Ok(calls) => state.calls.extend(
                    calls
                        .iter()
                        .filter(|call| {
                            Some(&call.node_id) == activation.node_id.as_ref()
                                && Some(call.generation) == activation.generation
                        })
                        .cloned(),
                ),
                Err(reason) => state.unknown = Some(reason.clone()),
            }
            match parse_area_report(&answer, state.name()) {
                Ok(mut report) => {
                    if state.activations > 1 {
                        follow_up_ids(&mut report, state.name(), state.activations - 1);
                    }
                    if !report.not_reached.is_empty() {
                        let note = not_reached_note(&slot, &report.not_reached);
                        self.note(note).await?;
                    }
                    state.reports.push(report);
                }
                Err(error) => state.unreadable.push((answer, error.to_string())),
            }
        }
        Ok(())
    }

    /// Record each area's coverage and what it left not covered, and build
    /// the results the integrator receives.
    async fn conclude(
        &mut self,
        states: &[AreaState],
        reads: bool,
        stopped: bool,
        deadline_hit: bool,
    ) -> Result<Vec<AreaResult>, RunError> {
        let repo = self.run.options.repo.clone();
        let mut results = Vec::new();
        for state in states {
            let name = state.name().to_owned();
            let entry = |class: FailureClass, detail: String| NotCovered {
                area: name.clone(),
                class,
                detail,
                node_id: state.node_id.clone(),
                turn_id: state.turn_id.clone(),
            };
            if !state.answered {
                let (class, detail) = state
                    .failure
                    .clone()
                    .unwrap_or((FailureClass::Other, "it never answered".into()));
                self.not_covered(entry(
                    class,
                    format!("the area worker has no result: {detail}"),
                ))
                .await?;
            }
            if let Some(reason) = &state.unknown {
                self.not_covered(entry(
                    FailureClass::Other,
                    format!("whether the area worker read its files is not known: {reason}"),
                ))
                .await?;
            }
            for (_, error) in &state.unreadable {
                self.not_covered(entry(
                    FailureClass::Other,
                    format!(
                        "the area worker's report could not be read ({error}); the integrator \
                         received its answer as text"
                    ),
                ))
                .await?;
            }
            let left_out: usize = state.reports.iter().map(|report| report.left_out).sum();
            if left_out > 0 {
                self.not_covered(entry(
                    FailureClass::Other,
                    format!(
                        "{left_out} finding{} beyond the first {MAX_AREA_FINDINGS} of a report \
                         {} left out of it",
                        if left_out == 1 { "" } else { "s" },
                        if left_out == 1 { "was" } else { "were" }
                    ),
                ))
                .await?;
            }
            let known = state.unknown.is_none();
            let unread: Vec<&RepoFile> = if known {
                state.unread(&repo)
            } else {
                Vec::new()
            };
            self.coverage(state, &unread, known).await?;
            if !unread.is_empty() {
                let (class, why) = unread_reason(state, reads, stopped, deadline_hit);
                for file in unread.iter().take(MAX_LISTED_UNREAD) {
                    self.not_covered(entry(class, format!("{}: not read ({why})", file.path)))
                        .await?;
                }
                if unread.len() > MAX_LISTED_UNREAD {
                    let rest: Vec<&str> = unread[MAX_LISTED_UNREAD..]
                        .iter()
                        .map(|file| file.path.as_str())
                        .collect();
                    self.not_covered(entry(
                        class,
                        format!(
                            "{} more files of this area: not read ({why}): {}",
                            rest.len(),
                            inline_list(&rest)
                        ),
                    ))
                    .await?;
                }
            }
            if state.answered {
                let mut bodies = Vec::new();
                if !state.reports.is_empty() {
                    let mut merged = AreaReport::default();
                    for report in &state.reports {
                        merged.findings.extend(report.findings.iter().cloned());
                        merged
                            .not_reached
                            .extend(report.not_reached.iter().cloned());
                        merged.left_out += report.left_out;
                    }
                    bodies.push(AreaBody::Report(merged));
                }
                for (answer, error) in &state.unreadable {
                    bodies.push(AreaBody::Unreadable {
                        answer: answer.clone(),
                        error: error.clone(),
                    });
                }
                results.push(AreaResult {
                    area: state.assigned.area.clone(),
                    bodies,
                    files: state.assigned.text_files().count(),
                    unread: known.then_some(unread.len()),
                });
            }
        }
        Ok(results)
    }

    /// The `coverage` phase of one area, and a note for each file too large
    /// to read.
    async fn coverage(
        &mut self,
        state: &AreaState,
        unread: &[&RepoFile],
        known: bool,
    ) -> Result<(), RunError> {
        let files = &state.assigned.files;
        let of_kind = |kind: FileKind| -> Vec<&str> {
            files
                .iter()
                .filter(|file| file.kind == kind)
                .map(|file| file.path.as_str())
                .collect()
        };
        let (empty, binary, too_large) = (
            of_kind(FileKind::Empty),
            of_kind(FileKind::Binary),
            of_kind(FileKind::TooLarge),
        );
        let text = state.assigned.text_files().count();
        let mut detail = if known {
            let read = text - unread.len();
            format!(
                "{}: {} of {} files examined: {read} read",
                state.name(),
                read + empty.len() + binary.len(),
                files.len()
            )
        } else {
            format!(
                "{}: {} files; whether its worker read the {text} to read is not known",
                state.name(),
                files.len()
            )
        };
        for (label, paths) in [
            (FileKind::Empty.not_read(), &empty),
            (FileKind::Binary.not_read(), &binary),
            (FileKind::TooLarge.not_read(), &too_large),
        ] {
            if !paths.is_empty() {
                let _ = write!(detail, "; {label}: {}", inline_list(paths));
            }
        }
        if !unread.is_empty() {
            let paths: Vec<&str> = unread.iter().map(|file| file.path.as_str()).collect();
            let _ = write!(detail, "; not read, not covered: {}", inline_list(&paths));
        }
        self.phase("coverage", detail).await?;
        let large: Vec<&RepoFile> = files
            .iter()
            .filter(|file| file.kind == FileKind::TooLarge)
            .collect();
        for file in large.iter().take(MAX_NOTED_TOO_LARGE) {
            self.note(format!(
                "{} ({} bytes) of area {} was not read: too large (over {} KiB, the most a \
                 worker is asked to read); a note, not a gap",
                file.path,
                file.size,
                state.name(),
                files::MAX_AUDITED_FILE_BYTES / 1024
            ))
            .await?;
        }
        if large.len() > MAX_NOTED_TOO_LARGE {
            self.note(format!(
                "{} more files of area {} were not read: too large (over {} KiB); a note, not a \
                 gap",
                large.len() - MAX_NOTED_TOO_LARGE,
                state.name(),
                files::MAX_AUDITED_FILE_BYTES / 1024
            ))
            .await?;
        }
        Ok(())
    }

    /// The workers' own findings, reported when integration has no result.
    fn unmerged(results: &[AreaResult]) -> Vec<Finding> {
        results
            .iter()
            .flat_map(|result| result.bodies.iter())
            .filter_map(|body| match body {
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
            area: INTEGRATION.into(),
            class,
            detail: format!("{detail}; the area workers' findings are reported unmerged"),
            node_id,
            turn_id,
        })
        .await?;
        self.findings(Self::unmerged(results)).await
    }

    /// The last turn: merge every report.
    async fn integrate(
        &mut self,
        assignment: &Assignment,
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
        let request = integrate_request(
            &self.run.resolved.prompt,
            assignment,
            results,
            &self.report.not_covered,
        );
        let started = match self
            .apply_and_turn(
                "audit integration: the integrator",
                integrate_slots(&self.run.resolved)?,
                &request,
                INTEGRATE_PURPOSE,
            )
            .await
        {
            Err(RunError::Stopped) => {
                self.integration_missing(
                    results,
                    FailureClass::Stopped,
                    "the run was stopped before integration".into(),
                    None,
                )
                .await?;
                return Err(RunError::Stopped);
            }
            started => started?,
        };
        let Some(observed) = started else {
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

/// Why an area's files are still unread, as the class and words of their
/// not-covered entries.
fn unread_reason(
    state: &AreaState,
    reads: bool,
    stopped: bool,
    deadline_hit: bool,
) -> (FailureClass, String) {
    if !state.answered {
        let class = state
            .failure
            .as_ref()
            .map_or(FailureClass::Other, |(class, _)| *class);
        return (class, "the area worker has no result".into());
    }
    if !reads {
        return (
            FailureClass::NotReached,
            "the worker Agent has no read_file tool".into(),
        );
    }
    if state.may_follow_up(reads) {
        if stopped {
            return (
                FailureClass::Stopped,
                "the run was stopped before a follow-up read it".into(),
            );
        }
        if deadline_hit {
            return (
                FailureClass::Budget,
                "the run's wall clock ran out before a follow-up read it".into(),
            );
        }
    }
    if let Some((class, detail)) = &state.failure {
        return (
            *class,
            format!(
                "the area worker's last follow-up has no result: {}",
                cut(detail, MAX_LISTED_DETAIL_BYTES).0
            ),
        );
    }
    (
        FailureClass::NotReached,
        format!(
            "the area worker did not read it in its turn or its {} follow-up{}",
            state.follow_ups,
            if state.follow_ups == 1 { "" } else { "s" }
        ),
    )
}

/// A follow-up's finding ids: `<area>-followup<n>-<id>`, so they never
/// repeat an earlier activation's.
fn follow_up_ids(report: &mut AreaReport, area: &str, round: u32) {
    let prefix = format!("{area}-");
    for finding in &mut report.findings {
        let own = finding
            .id
            .strip_prefix(&prefix)
            .unwrap_or(&finding.id)
            .to_owned();
        finding.id = cut(&format!("{area}-followup{round}-{own}"), 64)
            .0
            .to_owned();
    }
}

/// The note of what a worker listed as not reached, quoted and bounded.
fn not_reached_note(slot: &str, items: &[String]) -> String {
    let mut quoted: Vec<String> = items
        .iter()
        .take(MAX_NOTED_NOT_REACHED)
        .map(|item| {
            let (shown, cut_off) = cut(item.trim(), MAX_NOTED_ITEM_BYTES);
            if cut_off {
                format!("{shown}…")
            } else {
                shown.to_owned()
            }
        })
        .collect();
    if items.len() > MAX_NOTED_NOT_REACHED {
        quoted.push(format!("and {} more", items.len() - MAX_NOTED_NOT_REACHED));
    }
    format!(
        "{slot} listed as not reached: {}; a note: the host decides coverage from the files \
         its workers read",
        quoted.join("; ")
    )
}

#[cfg(test)]
#[path = "audit_tests.rs"]
mod tests;
