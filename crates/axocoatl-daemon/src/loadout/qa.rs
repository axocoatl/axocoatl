//! The qa loadout: one browser explorer; each finding's reproduction re-run
//! against the build under test and the reference; coverage as not covered.
//! Owner: review-qa.
//!
//! After the explorer's one turn the host reads its `FINDINGS` and
//! `COVERAGE` blocks. Each finding's reproduction must be a file under the
//! loadout's `repro_dir` in the checkout; the host runs it once with
//! `browser_check` against the build under test and, when a reference build
//! is configured and the reproduction failed there, against the reference,
//! and classifies it ([`axocoatl_session::qa_repro::classify`]). Every area
//! the explorer did not report covered, every unreadable part of its report,
//! every finding whose reproduction is missing or could not run, and the
//! rest of the app when the explorer failed is listed as not covered, so the
//! run never passes silently.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use axocoatl_config::loadout::{LoadoutRole, ParamOr, ResolvedLoadout};
use axocoatl_config::EgressAllowYaml;
use axocoatl_core::netaddr::{self, AddrClass};
use axocoatl_session::network_record::EgressScope;
use axocoatl_session::qa_repro::{
    classification_label, classify, parse_explorer_report, ReportedFinding, BLOCKED, NOT_REACHED,
};
use axocoatl_session::run_outcome::{
    FailureClass, Finding, FindingSource, NodeObservation, NodeState, NotCovered,
    ReproClassification, ReproResult, ReproRun, RunTurnRef, TurnObservation, TurnState,
};
use axocoatl_session::run_record::RunEvent;
use sha2::{Digest, Sha256};

use super::host::ReproRequest;
use super::{KindDriver, KindReport, RunContext, RunError, RunHost};
use crate::session_egress_policy::CompiledPolicy;

/// Longest one reproduction may run against one build, in milliseconds.
pub const REPRO_TIMEOUT_MS: u64 = 120_000;
/// The area named when the explorer stopped before it finished.
pub const REMAINING_APP: &str = "remaining app";
/// The area named when the explorer's answer has no `COVERAGE` block.
pub const WHOLE_APP_NO_COVERAGE: &str = "whole app (no coverage report)";
/// The area named when the explorer's report cannot be read.
pub const WHOLE_APP_UNREADABLE: &str = "whole app (unreadable report)";
/// The area named when the explorer's answer has no `FINDINGS` block.
pub const NO_FINDINGS_REPORT: &str = "findings (no FINDINGS report)";

pub struct QaDriver;

#[async_trait]
impl KindDriver for QaDriver {
    async fn drive(&self, host: &dyn RunHost, run: &RunContext) -> Result<KindReport, RunError> {
        let turn = super::driver::run_single_turn(host, run).await?;
        qa_report(host, run, turn).await
    }
}

/// The qa section of a resolved loadout, with its parameters' values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QaRunSettings {
    pub target_url: String,
    pub reference_url: Option<String>,
    /// Without a trailing `/`.
    pub repro_dir: String,
    pub fail_on_findings: bool,
}

fn param_text(resolved: &ResolvedLoadout, value: &ParamOr<String>) -> Option<String> {
    match value {
        ParamOr::Value(text) => Some(text.clone()),
        ParamOr::Param { param } => resolved.params.get(param).cloned(),
    }
    .map(|text| text.trim().to_owned())
    .filter(|text| !text.is_empty())
}

/// The qa settings of `resolved`, or why it has none a run can use.
pub fn qa_settings(resolved: &ResolvedLoadout) -> Result<QaRunSettings, RunError> {
    let qa = resolved
        .loadout
        .file
        .qa
        .as_ref()
        .ok_or_else(|| RunError::Usage("a qa loadout needs a qa: section".to_string()))?;
    let target_url = param_text(resolved, &qa.target_url)
        .ok_or_else(|| RunError::Usage("qa.target_url has no value".to_string()))?;
    let reference_url = qa
        .reference_url
        .as_ref()
        .and_then(|reference| param_text(resolved, reference));
    Ok(QaRunSettings {
        target_url,
        reference_url,
        repro_dir: qa.repro_dir.trim_end_matches('/').to_string(),
        fail_on_findings: qa.fail_on_findings,
    })
}

/// Why the browser container cannot reach `url`: it reaches the Session's
/// exposed ports as `http://localhost:<port>` (or `127.0.0.1`/`[::1]`), and
/// other hosts only through `browser.allow` (`private` lists the private
/// ranges it may reach).
pub fn browser_reaches(
    url: &str,
    exposed_ports: &[u16],
    allow: &[EgressAllowYaml],
    private: &[String],
) -> Result<(), String> {
    axocoatl_tools::browser_tool::check_url(url, "the URL")?;
    let parsed = reqwest::Url::parse(url).map_err(|error| error.to_string())?;
    let port = parsed
        .port_or_known_default()
        .ok_or_else(|| format!("{url} has no port"))?;
    let host = parsed
        .host_str()
        .filter(|host| !host.is_empty())
        .ok_or_else(|| format!("{url} names no host"))?;
    let ip = netaddr::parse_ip_literal(host);
    let loopback = match ip {
        Some(ip) => ip.is_loopback(),
        None => host.eq_ignore_ascii_case("localhost"),
    };
    if loopback {
        if exposed_ports.contains(&port) {
            return Ok(());
        }
        return Err(format!(
            "{url} is on port {port}, which this Session does not expose (exposed ports: {}); \
             the browser container reaches only the Session's exposed ports on localhost",
            if exposed_ports.is_empty() {
                "none".to_string()
            } else {
                exposed_ports
                    .iter()
                    .map(u16::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        ));
    }
    let not_allowed = || {
        format!(
            "{url} is not a Session port on localhost and browser.allow does not list \
             {host}:{port}; add it to browser.allow or serve the build on an exposed port"
        )
    };
    let policy = CompiledPolicy::compile(EgressScope::Browser, allow, private, &[])?;
    match ip {
        Some(ip) => ip_reached(&policy, ip, port, not_allowed),
        None => {
            let name = netaddr::normalize_host_name(host).map_err(|error| error.to_string())?;
            policy
                .match_name(&name, port)
                .map(|_| ())
                .ok_or_else(not_allowed)
        }
    }
}

fn ip_reached(
    policy: &CompiledPolicy,
    ip: std::net::IpAddr,
    port: u16,
    not_allowed: impl Fn() -> String,
) -> Result<(), String> {
    match netaddr::classify(ip) {
        AddrClass::Forbidden(kind) => Err(format!(
            "{ip} is {} and never reachable from the browser container",
            kind.label()
        )),
        AddrClass::Private(_)
            if !policy
                .private_destinations()
                .iter()
                .any(|range| range.contains(ip)) =>
        {
            Err(format!(
                "{ip} is a private address that browser.private_destinations does not list"
            ))
        }
        _ => policy
            .match_ip(ip, port)
            .map(|_| ())
            .ok_or_else(not_allowed),
    }
}

/// Check a qa run before it starts: the explorer's reproductions directory
/// must be one `browser_check` can read, and the build under test and the
/// reference build must be reachable from the browser container. Core calls
/// this at admission with the new Session's exposed ports and the daemon's
/// `browser:` block; an error is a usage error (exit 3).
pub fn validate_qa_admission(
    resolved: &ResolvedLoadout,
    exposed_ports: &[u16],
    browser: Option<&axocoatl_config::BrowserConfigYaml>,
) -> Result<QaRunSettings, RunError> {
    let settings = qa_settings(resolved)?;
    let browser = browser.ok_or_else(|| {
        RunError::Usage(
            "the qa loadout needs the browser tools: add a browser: block to the configuration \
             and run axocoatl browser install"
                .to_string(),
        )
    })?;
    if axocoatl_tools::browser_tool::normalize_repo_path(&settings.repro_dir).as_deref()
        != Ok(settings.repro_dir.as_str())
    {
        return Err(RunError::Usage(format!(
            "qa.repro_dir {:?} is not a path browser_check can read: use letters, digits, '.', \
             '_', '-', '@' and '+', no segment starting with '.', and not under node_modules",
            settings.repro_dir
        )));
    }
    let mut urls = vec![("target_url", settings.target_url.as_str())];
    if let Some(reference) = &settings.reference_url {
        if reference.trim_end_matches('/') == settings.target_url.trim_end_matches('/') {
            return Err(RunError::Usage(
                "qa.reference_url is the build under test; give a clean reference build, or \
                 leave it out"
                    .to_string(),
            ));
        }
        urls.push(("reference_url", reference.as_str()));
    }
    for (field, url) in urls {
        browser_reaches(
            url,
            exposed_ports,
            &browser.allow,
            &browser.private_destinations,
        )
        .map_err(|reason| RunError::Usage(format!("qa.{field}: {reason}")))?;
    }
    Ok(settings)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

/// The explorer's node: the slot of the loadout's explorer Agent, or the
/// first required node that is not the reviewer.
fn explorer_node<'a>(
    resolved: &ResolvedLoadout,
    turn: &'a TurnObservation,
) -> Option<&'a NodeObservation> {
    let explorer = resolved
        .loadout
        .file
        .agents
        .iter()
        .find(|agent| agent.role == LoadoutRole::Explorer)
        .map(|agent| agent.id.as_str());
    turn.nodes
        .iter()
        .find(|node| Some(node.slot_id.as_str()) == explorer)
        .or_else(|| {
            turn.nodes
                .iter()
                .find(|node| node.required && node.kind != "reviewer")
        })
}

/// Why the explorer did not finish, when it did not: its failure class and
/// what happened.
fn explorer_failure(
    run: &RunContext,
    turn: &TurnObservation,
    node: Option<&NodeObservation>,
) -> Option<(FailureClass, String)> {
    let Some(node) = node else {
        return Some((
            FailureClass::NotReached,
            "the explorer never ran in this turn".to_string(),
        ));
    };
    let Some(latest) = node.latest() else {
        return Some((
            FailureClass::NotReached,
            "the explorer never started".to_string(),
        ));
    };
    if latest.state == NodeState::Accepted {
        return None;
    }
    let out_of_time = Instant::now() >= run.deadline;
    let (class, message) = match &latest.failure {
        Some(failure) => (failure.class, failure.message.clone()),
        None => match latest.state {
            NodeState::Stopped | NodeState::Running if out_of_time => (
                FailureClass::Budget,
                "the run's wall clock ran out".to_string(),
            ),
            NodeState::Stopped => (FailureClass::Stopped, "the explorer was stopped".into()),
            NodeState::Blocked => (FailureClass::Blocked, "the explorer was blocked".into()),
            NodeState::NeverStarted => (
                FailureClass::NotReached,
                "the explorer never started".into(),
            ),
            NodeState::Running => (
                FailureClass::Other,
                "the turn ended while the explorer was still running".into(),
            ),
            _ => (
                FailureClass::Other,
                turn.attention_reason
                    .clone()
                    .unwrap_or_else(|| "the explorer did not finish".to_string()),
            ),
        },
    };
    Some((class, message))
}

fn class_name(class: FailureClass) -> String {
    serde_json::to_value(class)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "other".into())
}

fn status_class(status: &str) -> FailureClass {
    match status {
        NOT_REACHED => FailureClass::NotReached,
        BLOCKED => FailureClass::Blocked,
        _ => FailureClass::Other,
    }
}

/// Where a reported reproduction lives in the checkout, or why it cannot be
/// used.
fn repro_path(settings: &QaRunSettings, repro: Option<&str>) -> Result<String, String> {
    let Some(repro) = repro else {
        return Err("the finding names no reproduction".to_string());
    };
    let path = axocoatl_tools::browser_tool::normalize_repo_path(repro.trim())
        .map_err(|reason| format!("its reproduction {repro:?} cannot be used: {reason}"))?;
    if !path.starts_with(&format!("{}/", settings.repro_dir)) {
        return Err(format!(
            "its reproduction {path} is not under {}/, the only directory the explorer may write",
            settings.repro_dir
        ));
    }
    Ok(path)
}

/// The bytes of `path` in the repository at `repo`, read without following
/// links. `None` when it does not exist.
async fn read_repro(repo: &std::path::Path, path: &str) -> Result<Option<Vec<u8>>, String> {
    let repo = repo.to_path_buf();
    let path = path.to_string();
    tokio::task::spawn_blocking(move || {
        let root = axocoatl_core::SecureDir::open(&repo).map_err(|error| error.to_string())?;
        match root.read_limited(&path, axocoatl_tools::browser_tool::MAX_CHECK_FILE_BYTES) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!("{path} cannot be read: {error}")),
        }
    })
    .await
    .map_err(|error| error.to_string())?
}

/// One reproduction run, with infrastructure errors kept as an `error` run.
/// A stop, an unfilled hook or the deadline ends the run instead.
async fn repro_run(
    host: &dyn RunHost,
    run: &RunContext,
    path: &str,
    base_url: &str,
) -> Result<ReproRun, RunError> {
    let remaining = run.deadline.saturating_duration_since(Instant::now());
    if remaining < Duration::from_secs(1) {
        return Err(RunError::Deadline);
    }
    let request = ReproRequest {
        path: path.to_string(),
        base_url: base_url.to_string(),
        timeout_ms: REPRO_TIMEOUT_MS.min(remaining.as_millis() as u64),
    };
    match host.run_repro(&run.session_id, &request).await {
        Ok(result) => Ok(result),
        Err(error @ (RunError::NotImplemented(_) | RunError::Stopped | RunError::Deadline)) => {
            Err(error)
        }
        Err(other) => Ok(ReproRun {
            base_url: base_url.to_string(),
            status: "error".into(),
            first_error: Some(other.to_string()),
        }),
    }
}

/// The finding the Outcome lists for one reported finding.
fn finding(reported: &ReportedFinding, detail: String, repro: ReproResult) -> Finding {
    Finding {
        id: reported.id.clone(),
        source: FindingSource::Explorer,
        title: reported.title.clone(),
        detail,
        severity: reported.severity,
        area: reported.area.clone(),
        location: None,
        repro: Some(repro),
    }
}

fn base_detail(reported: &ReportedFinding) -> String {
    format!(
        "Expected: {}\nActual: {}",
        reported.expected, reported.actual
    )
}

/// Reproduce one finding and classify it, with its not-covered entry when
/// the reproduction is missing or could not run.
async fn reproduce(
    host: &dyn RunHost,
    run: &RunContext,
    settings: &QaRunSettings,
    reported: &ReportedFinding,
) -> Result<(Finding, Option<NotCovered>), RunError> {
    let mut detail = base_detail(reported);
    let area = format!("finding {}: {}", reported.id, reported.title);
    let not_covered = |class, why: &str| NotCovered {
        area: area.clone(),
        class,
        detail: format!("its reproduction gave no result: {why}"),
        node_id: None,
        turn_id: None,
    };
    let path = match repro_path(settings, reported.repro.as_deref()) {
        Ok(path) => path,
        Err(why) => {
            detail.push_str(&format!("\nReproduction: {why}."));
            let repro = ReproResult {
                path: reported.repro.clone().unwrap_or_default(),
                sha256: None,
                classification: ReproClassification::Missing,
                target: None,
                reference: None,
            };
            return Ok((
                finding(reported, detail, repro),
                Some(not_covered(FailureClass::Other, &why)),
            ));
        }
    };
    let missing = |detail: &mut String, why: String| {
        detail.push_str(&format!("\nReproduction: {why}."));
        let repro = ReproResult {
            path: path.clone(),
            sha256: None,
            classification: ReproClassification::Missing,
            target: None,
            reference: None,
        };
        (
            finding(reported, detail.clone(), repro),
            Some(not_covered(FailureClass::Other, &why)),
        )
    };
    let sha256 = match read_repro(&run.options.repo, &path).await {
        Ok(Some(bytes)) => format!("{:x}", Sha256::digest(&bytes)),
        Ok(None) => {
            return Ok(missing(
                &mut detail,
                format!("{path} does not exist in the repository"),
            ))
        }
        Err(why) => return Ok(missing(&mut detail, why)),
    };
    let unfinished = |detail: &mut String, ran: Option<ReproRun>| {
        let why = "the run's wall clock ran out before it ran";
        detail.push_str(&format!("\nReproduction: {why}."));
        let repro = ReproResult {
            path: path.clone(),
            sha256: Some(sha256.clone()),
            classification: ReproClassification::ReproError,
            target: ran,
            reference: None,
        };
        (
            finding(reported, detail.clone(), repro),
            Some(not_covered(FailureClass::Budget, why)),
        )
    };
    let target = match repro_run(host, run, &path, &settings.target_url).await {
        Ok(target) => target,
        Err(RunError::Deadline) => return Ok(unfinished(&mut detail, None)),
        Err(error) => return Err(error),
    };
    let reference = match (&settings.reference_url, target.status.as_str()) {
        (Some(reference_url), "failed") => match repro_run(host, run, &path, reference_url).await {
            Ok(reference) => Some(reference),
            Err(RunError::Deadline) => return Ok(unfinished(&mut detail, Some(target))),
            Err(error) => return Err(error),
        },
        _ => None,
    };
    let classification = classify(&target, reference.as_ref());
    detail.push_str(&format!(
        "\nReproduction {path}: {}.",
        classification_label(classification)
    ));
    let entry = (classification == ReproClassification::ReproError).then(|| {
        let why = reference
            .iter()
            .chain(std::iter::once(&target))
            .filter(|run| run.status != "passed" && run.status != "failed")
            .find_map(|run| run.first_error.clone())
            .unwrap_or_else(|| "the reproduction could not run".to_string());
        not_covered(FailureClass::Other, &why)
    });
    let repro = ReproResult {
        path,
        sha256: Some(sha256),
        classification,
        target: Some(target),
        reference,
    };
    Ok((finding(reported, detail, repro), entry))
}

/// What a finished qa turn found: the explorer's findings, each reproduced
/// and classified, and everything not covered.
pub async fn qa_report(
    host: &dyn RunHost,
    run: &RunContext,
    turn: TurnObservation,
) -> Result<KindReport, RunError> {
    let settings = qa_settings(&run.resolved)?;
    let node = explorer_node(&run.resolved, &turn);
    let failure = explorer_failure(run, &turn, node);
    let answer = node
        .and_then(NodeObservation::latest)
        .and_then(|generation| generation.answer.clone());
    let node_id = node.map(|node| node.node_id.clone());
    let turn_id = Some(turn.turn_id.clone());
    let entry = |area: &str, class: FailureClass, detail: String| NotCovered {
        area: area.to_string(),
        class,
        detail,
        node_id: node_id.clone(),
        turn_id: turn_id.clone(),
    };
    let mut not_covered = Vec::new();
    let mut reported = Vec::new();
    let parsed = answer.as_deref().map(parse_explorer_report);
    match &parsed {
        Some(Ok(report)) => {
            reported = report.findings.clone();
            for area in report.coverage.iter().filter(|area| !area.is_covered()) {
                let detail = match &area.reason {
                    Some(reason) => format!("{}: {reason}", area.status),
                    None => area.status.clone(),
                };
                not_covered.push(entry(&area.area, status_class(&area.status), detail));
            }
            for problem in &report.problems {
                not_covered.push(entry(
                    "explorer report",
                    FailureClass::Other,
                    problem.clone(),
                ));
            }
            if failure.is_none() {
                if !report.coverage_block || report.coverage.is_empty() {
                    not_covered.push(entry(
                        WHOLE_APP_NO_COVERAGE,
                        FailureClass::Other,
                        "the explorer's answer has no COVERAGE block listing the areas it \
                         covered, so no area counts as covered"
                            .to_string(),
                    ));
                }
                if !report.findings_block {
                    not_covered.push(entry(
                        NO_FINDINGS_REPORT,
                        FailureClass::Other,
                        "the explorer's answer has no FINDINGS block, so its findings are \
                         unknown"
                            .to_string(),
                    ));
                }
            }
        }
        Some(Err(error)) if failure.is_none() => {
            not_covered.push(entry(
                WHOLE_APP_UNREADABLE,
                FailureClass::Other,
                error.to_string(),
            ));
        }
        Some(Err(_)) => {}
        None if failure.is_none() => {
            not_covered.push(entry(
                WHOLE_APP_NO_COVERAGE,
                FailureClass::Other,
                "the explorer finished without an answer".to_string(),
            ));
        }
        None => {}
    }
    let mut budget_exhausted = false;
    if let Some((class, message)) = &failure {
        budget_exhausted = *class == FailureClass::Budget;
        let covered: Vec<&str> = match &parsed {
            Some(Ok(report)) => report
                .coverage
                .iter()
                .filter(|area| area.is_covered())
                .map(|area| area.area.as_str())
                .collect(),
            _ => Vec::new(),
        };
        let mut detail = format!(
            "{}: the explorer did not finish ({message})",
            class_name(*class)
        );
        if covered.is_empty() {
            detail.push_str("; it reported no area covered");
        } else {
            detail.push_str(&format!(
                "; only the areas it reported covered count: {}",
                covered.join(", ")
            ));
        }
        if let Some(Err(error)) = &parsed {
            detail.push_str(&format!("; its partial report cannot be read: {error}"));
        }
        not_covered.push(entry(REMAINING_APP, *class, detail));
    }
    if !reported.is_empty() {
        host.record(
            &run.run_id,
            RunEvent::Phase {
                at_ms: now_ms(),
                phase: "repro".into(),
                detail: format!(
                    "reproducing {} finding{} on {}{}",
                    reported.len(),
                    if reported.len() == 1 { "" } else { "s" },
                    settings.target_url,
                    settings
                        .reference_url
                        .as_ref()
                        .map(|reference| format!(" and, when they fail, on {reference}"))
                        .unwrap_or_default()
                ),
            },
        )
        .await?;
    }
    let mut findings = Vec::with_capacity(reported.len());
    for item in &reported {
        let (found, unreproduced) = reproduce(host, run, &settings, item).await?;
        if unreproduced
            .as_ref()
            .is_some_and(|entry| entry.class == FailureClass::Budget)
        {
            budget_exhausted = true;
        }
        host.record(
            &run.run_id,
            RunEvent::Finding {
                at_ms: now_ms(),
                finding: Box::new(found.clone()),
            },
        )
        .await?;
        findings.push(found);
        not_covered.extend(unreproduced);
    }
    for item in &not_covered {
        host.record(
            &run.run_id,
            RunEvent::NotCovered {
                at_ms: now_ms(),
                entry: Box::new(item.clone()),
            },
        )
        .await?;
    }
    let turn_ref = RunTurnRef {
        turn_id: turn.turn_id.clone(),
        purpose: "run".into(),
        state: turn.state,
    };
    budget_exhausted |= turn.state == TurnState::Stopped && Instant::now() >= run.deadline;
    Ok(KindReport {
        turns: vec![turn],
        turn_refs: vec![turn_ref],
        findings,
        not_covered,
        fail_on_findings: settings.fail_on_findings,
        budget_exhausted,
        ..KindReport::default()
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::Mutex;

    use axocoatl_config::loadout::{builtin_loadouts, resolve_loadout, ParamValues};
    use axocoatl_session::run_outcome::{
        GenerationObservation, ModelIdentity, NodeFailure, RunUsage,
    };

    use super::super::{KeepMode, RunOptions};
    use super::*;

    /// A host that runs nothing: it answers the scripted turn and the
    /// scripted reproduction runs, and keeps every call.
    #[derive(Default)]
    pub(crate) struct FakeHost {
        pub turn: Mutex<Option<TurnObservation>>,
        /// `(path, base_url)` → the run, or an infrastructure error.
        pub repros: Mutex<HashMap<(String, String), Result<ReproRun, String>>>,
        pub repro_calls: Mutex<Vec<ReproRequest>>,
        pub events: Mutex<Vec<RunEvent>>,
        pub sent: Mutex<Vec<String>>,
    }

    impl FakeHost {
        pub(crate) fn with_turn(turn: TurnObservation) -> Self {
            Self {
                turn: Mutex::new(Some(turn)),
                ..Self::default()
            }
        }

        fn script(&self, path: &str, base_url: &str, status: &str) {
            self.repros.lock().unwrap().insert(
                (path.into(), base_url.into()),
                Ok(ReproRun {
                    base_url: base_url.into(),
                    status: status.into(),
                    first_error: (status != "passed").then(|| format!("{status} on {base_url}")),
                }),
            );
        }

        pub(crate) fn events(&self) -> Vec<RunEvent> {
            self.events.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl RunHost for FakeHost {
        async fn apply_team(
            &self,
            _session_id: &str,
            _edit: crate::SessionTeamEdit,
        ) -> Result<(), RunError> {
            Ok(())
        }

        async fn send_turn(&self, _session_id: &str, request: &str) -> Result<String, RunError> {
            self.sent.lock().unwrap().push(request.to_string());
            Ok("turn-1".into())
        }

        async fn wait_turn(
            &self,
            _session_id: &str,
            _turn_id: &str,
            _deadline: Instant,
        ) -> Result<TurnObservation, RunError> {
            self.turn
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| RunError::Infrastructure("no scripted turn".into()))
        }

        async fn stop_turn(&self, _session_id: &str, _turn_id: &str) -> Result<(), RunError> {
            Ok(())
        }

        async fn run_repro(
            &self,
            _session_id: &str,
            request: &ReproRequest,
        ) -> Result<ReproRun, RunError> {
            self.repro_calls.lock().unwrap().push(request.clone());
            match self
                .repros
                .lock()
                .unwrap()
                .get(&(request.path.clone(), request.base_url.clone()))
            {
                Some(Ok(run)) => Ok(run.clone()),
                Some(Err(reason)) => Err(RunError::Infrastructure(reason.clone())),
                None => Err(RunError::Infrastructure("no scripted reproduction".into())),
            }
        }

        async fn read_sandbox_file(
            &self,
            _session_id: &str,
            _path: &str,
            _max_bytes: usize,
        ) -> Result<Option<Vec<u8>>, RunError> {
            Ok(None)
        }

        async fn record(&self, _run_id: &str, event: RunEvent) -> Result<(), RunError> {
            self.events.lock().unwrap().push(event);
            Ok(())
        }
    }

    /// A run of the built-in loadout `id` with `params` on `repo`.
    pub(crate) fn context(id: &str, params: &[(&str, &str)], repo: &Path) -> RunContext {
        let loadout = builtin_loadouts()
            .into_iter()
            .map(|loadout| loadout.expect("built-in loadouts parse"))
            .find(|loadout| loadout.file.id == id)
            .expect("the built-in exists");
        let params: ParamValues = params
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect();
        let task = "Find the checkout bugs";
        let resolved =
            resolve_loadout(&loadout, &params, task, &repo.to_string_lossy()).expect("resolves");
        RunContext {
            run_id: "run-test".into(),
            session_id: "ses-test".into(),
            workspace_id: "wsp-test".into(),
            resolved,
            options: RunOptions {
                task: task.into(),
                repo: repo.to_path_buf(),
                params,
                keep: KeepMode::None,
                check_command: Some("npm test".into()),
                setup_command: None,
            },
            deadline: Instant::now() + Duration::from_secs(600),
        }
    }

    pub(crate) fn node(
        slot: &str,
        kind: &str,
        model: &str,
        generations: Vec<GenerationObservation>,
    ) -> NodeObservation {
        NodeObservation {
            node_id: format!("{slot}-node"),
            slot_id: slot.into(),
            model: ModelIdentity {
                provider: "openrouter".into(),
                model: model.into(),
                runtime: "native".into(),
            },
            required: kind != "reviewer",
            kind: kind.into(),
            generations,
        }
    }

    pub(crate) fn generation(
        number: u32,
        state: NodeState,
        answer: Option<&str>,
        failure: Option<NodeFailure>,
    ) -> GenerationObservation {
        GenerationObservation {
            generation: number,
            state,
            answer: answer.map(str::to_owned),
            failure,
        }
    }

    pub(crate) fn turn(state: TurnState, nodes: Vec<NodeObservation>) -> TurnObservation {
        TurnObservation {
            session_id: "ses-test".into(),
            turn_id: "turn-1".into(),
            state,
            attention_reason: None,
            nodes,
            checks: Vec::new(),
            review: None,
            usage: RunUsage::default(),
        }
    }

    fn explorer_turn(
        state: NodeState,
        answer: Option<&str>,
        failure: Option<NodeFailure>,
    ) -> TurnObservation {
        let turn_state = if state == NodeState::Accepted {
            TurnState::Completed
        } else {
            TurnState::NeedsAttention
        };
        turn(
            turn_state,
            vec![node(
                "explorer",
                "slot",
                "explorer-model",
                vec![generation(1, state, answer, failure)],
            )],
        )
    }

    const TARGET: &str = "http://localhost:3000";
    const REFERENCE: &str = "http://localhost:3001";

    fn report(findings: &[(&str, Option<&str>)], coverage: &[(&str, &str)]) -> String {
        let findings: Vec<_> = findings
            .iter()
            .map(|(id, repro)| {
                serde_json::json!({"id": id, "title": format!("bug {id}"), "area": "checkout",
                    "severity": "high", "expected": "works", "actual": "broken", "repro": repro})
            })
            .collect();
        let coverage: Vec<_> = coverage
            .iter()
            .map(|(area, status)| serde_json::json!({"area": area, "status": status, "reason": "why"}))
            .collect();
        format!(
            "I explored the app.\n\nFINDINGS\n```json\n{}\n```\n\nCOVERAGE\n```json\n{}\n```\n",
            serde_json::to_string_pretty(&findings).unwrap(),
            serde_json::to_string_pretty(&coverage).unwrap()
        )
    }

    fn repo_with(files: &[&str]) -> tempfile::TempDir {
        let repo = tempfile::tempdir().unwrap();
        for file in files {
            let path = repo.path().join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, format!("// repro {file}\n")).unwrap();
        }
        repo
    }

    fn classification(report: &KindReport, id: &str) -> ReproClassification {
        report
            .findings
            .iter()
            .find(|finding| finding.id == id)
            .and_then(|finding| finding.repro.as_ref())
            .map(|repro| repro.classification)
            .unwrap_or_else(|| panic!("no finding {id}: {:?}", report.findings))
    }

    /// With a reference build: fails on the target and passes on the
    /// reference is confirmed; fails on both is "fails on clean build"; a
    /// reproduction that passes on the target is not reproduced and the
    /// reference is not run for it; a missing file is missing and not
    /// covered. Each reproduction runs once per build, through the host.
    #[tokio::test]
    async fn reproductions_are_classified_against_the_reference() {
        let repo = repo_with(&[
            "axocoatl-qa/b1.spec.ts",
            "axocoatl-qa/b2.spec.ts",
            "axocoatl-qa/b3.spec.ts",
        ]);
        let run = context(
            "qa",
            &[
                ("explorer_model", "openrouter:qwen/qwen3-coder"),
                ("reference_url", REFERENCE),
            ],
            repo.path(),
        );
        let answer = report(
            &[
                ("B1", Some("axocoatl-qa/b1.spec.ts")),
                ("B2", Some("./axocoatl-qa/b2.spec.ts")),
                ("B3", Some("axocoatl-qa/b3.spec.ts")),
                ("B4", Some("axocoatl-qa/b4.spec.ts")),
                ("B5", None),
                ("B6", Some("src/app.spec.ts")),
            ],
            &[("checkout", "covered"), ("search", "covered")],
        );
        let host = FakeHost::default();
        host.script("axocoatl-qa/b1.spec.ts", TARGET, "failed");
        host.script("axocoatl-qa/b1.spec.ts", REFERENCE, "passed");
        host.script("axocoatl-qa/b2.spec.ts", TARGET, "failed");
        host.script("axocoatl-qa/b2.spec.ts", REFERENCE, "failed");
        host.script("axocoatl-qa/b3.spec.ts", TARGET, "passed");
        let turn = explorer_turn(NodeState::Accepted, Some(&answer), None);
        let report = qa_report(&host, &run, turn).await.unwrap();
        assert_eq!(
            classification(&report, "B1"),
            ReproClassification::Confirmed
        );
        assert_eq!(
            classification(&report, "B2"),
            ReproClassification::FailsOnCleanBuild
        );
        assert_eq!(
            classification(&report, "B3"),
            ReproClassification::NotReproduced
        );
        assert_eq!(classification(&report, "B4"), ReproClassification::Missing);
        assert_eq!(classification(&report, "B5"), ReproClassification::Missing);
        assert_eq!(classification(&report, "B6"), ReproClassification::Missing);
        let b2 = report.findings.iter().find(|f| f.id == "B2").unwrap();
        assert!(b2.detail.contains("fails on clean build"), "{}", b2.detail);
        assert_eq!(b2.source, FindingSource::Explorer);
        let repro = b2.repro.as_ref().unwrap();
        assert_eq!(repro.path, "axocoatl-qa/b2.spec.ts");
        assert_eq!(
            repro.sha256.as_deref(),
            Some(format!("{:x}", Sha256::digest(b"// repro axocoatl-qa/b2.spec.ts\n")).as_str())
        );
        assert_eq!(repro.target.as_ref().unwrap().status, "failed");
        assert_eq!(repro.reference.as_ref().unwrap().status, "failed");
        let calls: Vec<(String, String)> = host
            .repro_calls
            .lock()
            .unwrap()
            .iter()
            .map(|call| (call.path.clone(), call.base_url.clone()))
            .collect();
        assert_eq!(
            calls,
            [
                ("axocoatl-qa/b1.spec.ts", TARGET),
                ("axocoatl-qa/b1.spec.ts", REFERENCE),
                ("axocoatl-qa/b2.spec.ts", TARGET),
                ("axocoatl-qa/b2.spec.ts", REFERENCE),
                ("axocoatl-qa/b3.spec.ts", TARGET),
            ]
            .map(|(path, url)| (path.to_string(), url.to_string()))
        );
        assert!(host
            .repro_calls
            .lock()
            .unwrap()
            .iter()
            .all(|call| call.timeout_ms > 0 && call.timeout_ms <= REPRO_TIMEOUT_MS));
        // The three findings without a usable reproduction are not covered;
        // nothing else is.
        let areas: Vec<_> = report.not_covered.iter().map(|e| e.area.as_str()).collect();
        assert_eq!(
            areas,
            [
                "finding B4: bug B4",
                "finding B5: bug B5",
                "finding B6: bug B6"
            ]
        );
        assert!(report.not_covered[2]
            .detail
            .contains("not under axocoatl-qa/"));
        assert!(report.fail_on_findings);
        // Each finding and each not-covered entry is in the run's events.
        let events = host.events();
        assert!(matches!(&events[0], RunEvent::Phase { phase, .. } if phase == "repro"));
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, RunEvent::Finding { .. }))
                .count(),
            6
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, RunEvent::NotCovered { .. }))
                .count(),
            3
        );
    }

    /// Without a reference build a failing reproduction is reproduced, never
    /// confirmed; one that cannot run is a reproduction error and not
    /// covered.
    #[tokio::test]
    async fn without_a_reference_a_failure_is_reproduced() {
        let repo = repo_with(&["axocoatl-qa/b1.spec.ts", "axocoatl-qa/b2.spec.ts"]);
        let run = context(
            "qa",
            &[("explorer_model", "openrouter:qwen/qwen3-coder")],
            repo.path(),
        );
        let answer = report(
            &[
                ("B1", Some("axocoatl-qa/b1.spec.ts")),
                ("B2", Some("axocoatl-qa/b2.spec.ts")),
            ],
            &[("checkout", "covered")],
        );
        let host = FakeHost::default();
        host.script("axocoatl-qa/b1.spec.ts", TARGET, "failed");
        host.repros.lock().unwrap().insert(
            ("axocoatl-qa/b2.spec.ts".into(), TARGET.into()),
            Err("the browser container did not start".into()),
        );
        let turn = explorer_turn(NodeState::Accepted, Some(&answer), None);
        let report = qa_report(&host, &run, turn).await.unwrap();
        assert_eq!(
            classification(&report, "B1"),
            ReproClassification::Reproduced
        );
        assert_eq!(
            classification(&report, "B2"),
            ReproClassification::ReproError
        );
        let b2 = &report.findings[1].repro.as_ref().unwrap().target;
        assert!(b2
            .as_ref()
            .unwrap()
            .first_error
            .as_deref()
            .unwrap()
            .contains("did not start"));
        assert_eq!(report.not_covered.len(), 1);
        assert!(report.not_covered[0].detail.contains("did not start"));
        assert_eq!(host.repro_calls.lock().unwrap().len(), 2);
    }

    /// A refusal or classifier stop ends the explorer: every area it did not
    /// report covered, and the rest of the app, are not covered with the
    /// refusal's class. Findings in its partial answer are still reproduced.
    #[tokio::test]
    async fn an_explorer_stopped_by_a_refusal_leaves_the_rest_not_covered() {
        let repo = repo_with(&["axocoatl-qa/b1.spec.ts"]);
        let run = context(
            "qa",
            &[("explorer_model", "openrouter:qwen/qwen3-coder")],
            repo.path(),
        );
        let partial = report(
            &[("B1", Some("axocoatl-qa/b1.spec.ts"))],
            &[("checkout", "covered"), ("gift cards", "blocked")],
        );
        let host = FakeHost::default();
        host.script("axocoatl-qa/b1.spec.ts", TARGET, "failed");
        let failure = NodeFailure {
            class: FailureClass::ProviderRefusal,
            message: "the provider's safety classifier stopped the stream".into(),
        };
        let turn = explorer_turn(NodeState::Failed, Some(&partial), Some(failure));
        let report = qa_report(&host, &run, turn).await.unwrap();
        assert_eq!(
            classification(&report, "B1"),
            ReproClassification::Reproduced
        );
        let entries: Vec<_> = report
            .not_covered
            .iter()
            .map(|e| (e.area.as_str(), e.class))
            .collect();
        assert_eq!(
            entries,
            [
                ("gift cards", FailureClass::Blocked),
                (REMAINING_APP, FailureClass::ProviderRefusal)
            ]
        );
        let remaining = &report.not_covered[1];
        assert!(
            remaining
                .detail
                .starts_with("provider_refusal: the explorer did not finish"),
            "{}",
            remaining.detail
        );
        assert!(
            remaining.detail.contains("checkout"),
            "{}",
            remaining.detail
        );
        assert_eq!(remaining.node_id.as_deref(), Some("explorer-node"));
        assert_eq!(remaining.turn_id.as_deref(), Some("turn-1"));
        assert!(!report.budget_exhausted);

        // Without any answer the whole remaining app is not covered, and a
        // budget failure marks the run's budget as exhausted.
        let host = FakeHost::default();
        let failure = NodeFailure {
            class: FailureClass::Budget,
            message: "the grant's tokens ran out".into(),
        };
        let turn = explorer_turn(NodeState::Failed, None, Some(failure));
        let report = qa_report(&host, &run, turn).await.unwrap();
        assert_eq!(report.not_covered.len(), 1);
        assert_eq!(report.not_covered[0].area, REMAINING_APP);
        assert_eq!(report.not_covered[0].class, FailureClass::Budget);
        assert!(report.not_covered[0]
            .detail
            .contains("it reported no area covered"));
        assert!(report.budget_exhausted);
        assert!(report.findings.is_empty());
    }

    /// An answer without a COVERAGE block covers nothing: one entry, "whole
    /// app (no coverage report)". Not-reached areas and a missing FINDINGS
    /// block are listed too; an unreadable report is the whole app.
    #[tokio::test]
    async fn without_a_coverage_report_the_whole_app_is_not_covered() {
        let repo = repo_with(&[]);
        let run = context(
            "qa",
            &[("explorer_model", "openrouter:qwen/qwen3-coder")],
            repo.path(),
        );
        let host = FakeHost::default();
        let answer = "No bugs found.\n\nFINDINGS\n```json\n[]\n```\n";
        let turn = explorer_turn(NodeState::Accepted, Some(answer), None);
        let report = qa_report(&host, &run, turn).await.unwrap();
        assert_eq!(report.not_covered.len(), 1);
        assert_eq!(report.not_covered[0].area, WHOLE_APP_NO_COVERAGE);
        assert!(report.findings.is_empty());

        let answer = "COVERAGE\n```json\n[{\"area\": \"search\", \"status\": \"not reached\"}, \
                      {\"area\": \"cart\", \"status\": \"covered\"}]\n```";
        let turn = explorer_turn(NodeState::Accepted, Some(answer), None);
        let report = qa_report(&host, &run, turn).await.unwrap();
        let entries: Vec<_> = report
            .not_covered
            .iter()
            .map(|e| (e.area.as_str(), e.class))
            .collect();
        assert_eq!(
            entries,
            [
                ("search", FailureClass::NotReached),
                (NO_FINDINGS_REPORT, FailureClass::Other)
            ]
        );

        let answer = "FINDINGS\n```json\n[{\"id\": \"B1\",]\n```";
        let turn = explorer_turn(NodeState::Accepted, Some(answer), None);
        let report = qa_report(&host, &run, turn).await.unwrap();
        assert_eq!(report.not_covered.len(), 1);
        assert_eq!(report.not_covered[0].area, WHOLE_APP_UNREADABLE);
        assert!(host.repro_calls.lock().unwrap().is_empty());
    }

    /// A clean report (everything covered, no findings) adds nothing to the
    /// Outcome, and its turn is listed.
    #[tokio::test]
    async fn a_clean_report_adds_nothing() {
        let repo = repo_with(&[]);
        let run = context(
            "qa",
            &[("explorer_model", "openrouter:qwen/qwen3-coder")],
            repo.path(),
        );
        let host = FakeHost::default();
        let answer = report(&[], &[("checkout", "covered")]);
        let report = qa_report(
            &host,
            &run,
            explorer_turn(NodeState::Accepted, Some(&answer), None),
        )
        .await
        .unwrap();
        assert!(report.findings.is_empty() && report.not_covered.is_empty());
        assert_eq!(report.turns.len(), 1);
        assert_eq!(report.turn_refs[0].turn_id, "turn-1");
        assert!(host.events().is_empty());
    }

    /// The run's deadline stops the reproductions that have not run: they
    /// are not covered for budget, and the run's budget is exhausted.
    #[tokio::test]
    async fn the_deadline_leaves_reproductions_not_covered() {
        let repo = repo_with(&["axocoatl-qa/b1.spec.ts"]);
        let mut run = context(
            "qa",
            &[("explorer_model", "openrouter:qwen/qwen3-coder")],
            repo.path(),
        );
        run.deadline = Instant::now();
        let host = FakeHost::default();
        let answer = report(
            &[("B1", Some("axocoatl-qa/b1.spec.ts"))],
            &[("checkout", "covered")],
        );
        let report = qa_report(
            &host,
            &run,
            explorer_turn(NodeState::Accepted, Some(&answer), None),
        )
        .await
        .unwrap();
        assert_eq!(
            classification(&report, "B1"),
            ReproClassification::ReproError
        );
        assert_eq!(report.not_covered[0].class, FailureClass::Budget);
        assert!(report.budget_exhausted);
        assert!(host.repro_calls.lock().unwrap().is_empty());
    }

    /// A stop or an unfilled hook ends the run instead of becoming a
    /// reproduction error.
    #[tokio::test]
    async fn a_stop_during_reproduction_ends_the_run() {
        struct Stopping(FakeHost);
        #[async_trait]
        impl RunHost for Stopping {
            async fn apply_team(
                &self,
                session_id: &str,
                edit: crate::SessionTeamEdit,
            ) -> Result<(), RunError> {
                self.0.apply_team(session_id, edit).await
            }
            async fn send_turn(&self, session_id: &str, request: &str) -> Result<String, RunError> {
                self.0.send_turn(session_id, request).await
            }
            async fn wait_turn(
                &self,
                session_id: &str,
                turn_id: &str,
                deadline: Instant,
            ) -> Result<TurnObservation, RunError> {
                self.0.wait_turn(session_id, turn_id, deadline).await
            }
            async fn stop_turn(&self, session_id: &str, turn_id: &str) -> Result<(), RunError> {
                self.0.stop_turn(session_id, turn_id).await
            }
            async fn run_repro(&self, _: &str, _: &ReproRequest) -> Result<ReproRun, RunError> {
                Err(RunError::Stopped)
            }
            async fn read_sandbox_file(
                &self,
                session_id: &str,
                path: &str,
                max_bytes: usize,
            ) -> Result<Option<Vec<u8>>, RunError> {
                self.0.read_sandbox_file(session_id, path, max_bytes).await
            }
            async fn record(&self, run_id: &str, event: RunEvent) -> Result<(), RunError> {
                self.0.record(run_id, event).await
            }
        }
        let repo = repo_with(&["axocoatl-qa/b1.spec.ts"]);
        let run = context(
            "qa",
            &[("explorer_model", "openrouter:qwen/qwen3-coder")],
            repo.path(),
        );
        let answer = report(
            &[("B1", Some("axocoatl-qa/b1.spec.ts"))],
            &[("checkout", "covered")],
        );
        let result = qa_report(
            &Stopping(FakeHost::default()),
            &run,
            explorer_turn(NodeState::Accepted, Some(&answer), None),
        )
        .await;
        assert!(matches!(result, Err(RunError::Stopped)), "{result:?}");
    }

    /// The driver runs the loadout's one turn and then the report. Until
    /// core's `run_single_turn` lands on this branch it answers not
    /// implemented; after that the driver's report is the one `qa_report`
    /// builds from the observed turn.
    #[tokio::test]
    async fn the_driver_reports_the_observed_turn() {
        let repo = repo_with(&["axocoatl-qa/b1.spec.ts"]);
        let run = context(
            "qa",
            &[("explorer_model", "openrouter:qwen/qwen3-coder")],
            repo.path(),
        );
        let answer = report(
            &[("B1", Some("axocoatl-qa/b1.spec.ts"))],
            &[("checkout", "covered")],
        );
        let observed = explorer_turn(NodeState::Accepted, Some(&answer), None);
        let host = FakeHost::with_turn(observed.clone());
        host.script("axocoatl-qa/b1.spec.ts", TARGET, "failed");
        match QaDriver.drive(&host, &run).await {
            Err(RunError::NotImplemented(what)) => {
                assert!(what.starts_with("loadout::"), "{what}")
            }
            Ok(report) => {
                assert_eq!(report.turns, vec![observed]);
                assert_eq!(
                    classification(&report, "B1"),
                    ReproClassification::Reproduced
                );
                assert_eq!(host.sent.lock().unwrap().len(), 1);
            }
            Err(other) => panic!("{other:?}"),
        }
    }

    #[test]
    fn urls_must_be_reachable_from_the_browser_container() {
        use axocoatl_config::EgressHostYaml;
        let allow = vec![EgressAllowYaml::Host(EgressHostYaml {
            host: "staging.example.com".into(),
            ports: Some(vec![443, 8443]),
        })];
        let ok = |url: &str| browser_reaches(url, &[3000, 3001], &allow, &[]);
        assert!(ok("http://localhost:3000").is_ok());
        assert!(ok("http://127.0.0.1:3001/app").is_ok());
        assert!(ok("http://[::1]:3000").is_ok());
        assert!(ok("https://staging.example.com/").is_ok());
        assert!(ok("https://STAGING.example.com:8443/").is_ok());
        let refused = |url: &str| ok(url).unwrap_err();
        assert!(refused("http://localhost:4000").contains("does not expose"));
        assert!(refused("http://localhost").contains("port 80"));
        assert!(refused("https://prod.example.com").contains("browser.allow"));
        assert!(refused("http://staging.example.com").contains("browser.allow"));
        assert!(refused("http://10.0.0.5:3000").contains("private"));
        assert!(refused("http://169.254.169.254/").contains("never reachable"));
        assert!(refused("file:///etc/passwd").contains("http"));
        // A private range listed with its address reaches it.
        let ranged = vec![EgressAllowYaml::Cidr(axocoatl_config::EgressCidrYaml {
            cidr: "10.0.0.0/24".into(),
            ports: Some(vec![3000]),
        })];
        assert!(browser_reaches(
            "http://10.0.0.5:3000",
            &[],
            &ranged,
            &["10.0.0.0/24".to_string()]
        )
        .is_ok());
    }

    #[test]
    fn admission_checks_the_browser_urls_and_the_reproductions_directory() {
        let repo = repo_with(&[]);
        let browser = axocoatl_config::BrowserConfigYaml::default();
        let run = context(
            "qa",
            &[("explorer_model", "openrouter:qwen/qwen3-coder")],
            repo.path(),
        );
        let settings = validate_qa_admission(&run.resolved, &[3000], Some(&browser)).unwrap();
        assert_eq!(settings.target_url, TARGET);
        assert_eq!(settings.reference_url, None);
        assert_eq!(settings.repro_dir, "axocoatl-qa");
        let usage = |result: Result<QaRunSettings, RunError>| match result {
            Err(RunError::Usage(message)) => message,
            other => panic!("{other:?}"),
        };
        assert!(
            usage(validate_qa_admission(&run.resolved, &[3000], None)).contains("browser: block")
        );
        assert!(usage(validate_qa_admission(
            &run.resolved,
            &[8080],
            Some(&browser)
        ))
        .starts_with("qa.target_url:"));
        let with_reference = context(
            "qa",
            &[
                ("explorer_model", "openrouter:qwen/qwen3-coder"),
                ("reference_url", "http://localhost:3001"),
            ],
            repo.path(),
        );
        assert!(usage(validate_qa_admission(
            &with_reference.resolved,
            &[3000],
            Some(&browser)
        ))
        .starts_with("qa.reference_url:"));
        assert!(
            validate_qa_admission(&with_reference.resolved, &[3000, 3001], Some(&browser)).is_ok()
        );
        let same = context(
            "qa",
            &[
                ("explorer_model", "openrouter:qwen/qwen3-coder"),
                ("reference_url", "http://localhost:3000/"),
            ],
            repo.path(),
        );
        assert!(usage(validate_qa_admission(
            &same.resolved,
            &[3000],
            Some(&browser)
        ))
        .contains("is the build under test"));
        // A hidden reproductions directory cannot be re-run by browser_check.
        let mut hidden = run.resolved.clone();
        hidden.loadout.file.qa.as_mut().unwrap().repro_dir = ".axocoatl/qa".into();
        assert!(
            usage(validate_qa_admission(&hidden, &[3000], Some(&browser)))
                .contains("not a path browser_check can read")
        );
    }
}
