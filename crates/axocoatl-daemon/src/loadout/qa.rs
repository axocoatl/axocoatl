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
        // The explorer writes its reproductions with write_file, which
        // creates no directory, and has no bash or edit_file to create one:
        // the host creates the reproductions directory before its turn.
        let settings = qa_settings(&run.resolved)?;
        let created = repro_dir_ready(&run.options.repo, &settings.repro_dir, true).await?;
        if created {
            host.record(
                &run.run_id,
                RunEvent::Phase {
                    at_ms: now_ms(),
                    phase: "preparing".into(),
                    detail: format!(
                        "created {}/ in the repository for the explorer's reproductions",
                        settings.repro_dir
                    ),
                },
            )
            .await?;
        }
        let turn = super::driver::run_single_turn(host, run).await?;
        qa_report(host, run, turn).await
    }
}

/// Why the reproductions directory cannot be used.
#[derive(Debug)]
enum ReproDirError {
    /// A component exists as a link or as something other than a directory.
    NotADirectory(String),
    Io(String),
}

impl From<ReproDirError> for RunError {
    fn from(error: ReproDirError) -> Self {
        match error {
            ReproDirError::NotADirectory(message) => RunError::Usage(message),
            ReproDirError::Io(message) => RunError::Infrastructure(message),
        }
    }
}

/// Make `repro_dir` (a repository path `validate_qa_admission` accepted) a
/// real directory of the repository at `repo`. Each component is opened from
/// its parent's handle without following links; a missing one is created
/// (owner-only, like every directory `SecureDir` creates) when `create` is
/// set, and one that exists as a symbolic link or as anything other than a
/// directory is refused, so nothing is ever created or written through a
/// link. Returns whether a directory was created.
fn prepare_repro_dir(
    repo: &std::path::Path,
    repro_dir: &str,
    create: bool,
) -> Result<bool, ReproDirError> {
    let shown = |path: &std::path::Path| -> String {
        path.strip_prefix(repo)
            .map(|relative| relative.display().to_string())
            .unwrap_or_else(|_| path.display().to_string())
    };
    let mut dir = axocoatl_core::SecureDir::open(repo).map_err(|error| {
        ReproDirError::Io(format!(
            "the repository {} cannot be opened: {error}",
            repo.display()
        ))
    })?;
    let mut created = false;
    for component in std::path::Path::new(repro_dir).components() {
        let std::path::Component::Normal(name) = component else {
            return Err(ReproDirError::NotADirectory(format!(
                "qa.repro_dir {repro_dir:?} is not a path inside the repository"
            )));
        };
        let path = dir.path().join(name);
        match dir.existing_child(name) {
            Ok(child) => dir = child,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if !create {
                    return Ok(false);
                }
                dir = dir
                    .child(name)
                    .map_err(|error| wrong_type(&path, error, &shown))?;
                created = true;
            }
            Err(error) => return Err(wrong_type(&path, error, &shown)),
        }
    }
    Ok(created)
}

/// The error for a reproductions directory component that could not be
/// opened as a directory: what it is when it is a link or not a directory.
fn wrong_type(
    path: &std::path::Path,
    error: std::io::Error,
    shown: &dyn Fn(&std::path::Path) -> String,
) -> ReproDirError {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => ReproDirError::NotADirectory(format!(
            "{} in the repository is a symbolic link; the qa explorer writes its reproductions \
             only into a real directory, so remove the link",
            shown(path)
        )),
        Ok(metadata) if !metadata.is_dir() => ReproDirError::NotADirectory(format!(
            "{} in the repository is not a directory; the qa explorer writes its reproductions \
             there, so move it away",
            shown(path)
        )),
        _ => ReproDirError::Io(format!(
            "{} in the repository cannot be prepared: {error}",
            shown(path)
        )),
    }
}

/// [`prepare_repro_dir`] off the async runtime.
async fn repro_dir_ready(
    repo: &std::path::Path,
    repro_dir: &str,
    create: bool,
) -> Result<bool, RunError> {
    let repo = repo.to_path_buf();
    let repro_dir = repro_dir.to_string();
    tokio::task::spawn_blocking(move || prepare_repro_dir(&repo, &repro_dir, create))
        .await
        .map_err(|error| RunError::Infrastructure(error.to_string()))?
        .map_err(RunError::from)
}

/// Refuse a qa run whose reproductions directory exists in the repository
/// as a symbolic link or as something other than a directory. Creates
/// nothing: the driver creates what is missing before the explorer's turn.
pub fn check_repro_dir(repo: &std::path::Path, settings: &QaRunSettings) -> Result<(), RunError> {
    prepare_repro_dir(repo, &settings.repro_dir, false)
        .map(|_| ())
        .map_err(RunError::from)
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

/// Whether `host` (a URL's host) is this machine as the browser container
/// sees it: `localhost` or a loopback address, which reach the Session's
/// exposed ports.
fn is_loopback_host(host: &str) -> bool {
    match netaddr::parse_ip_literal(host) {
        Some(ip) => ip.is_loopback(),
        None => host.eq_ignore_ascii_case("localhost"),
    }
}

/// The Session port a qa URL names: its port when its host is `localhost`
/// or a loopback address, `None` for any other host (which only
/// `browser.allow` can admit, see [`browser_reaches`]) or a URL that does not
/// parse.
pub fn session_port(url: &str) -> Option<u16> {
    let parsed = reqwest::Url::parse(url).ok()?;
    let host = parsed.host_str().filter(|host| !host.is_empty())?;
    is_loopback_host(host)
        .then(|| parsed.port_or_known_default())
        .flatten()
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
    if is_loopback_host(host) {
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
/// this at admission with the new Session's exposed ports and the
/// `browser.allow` and `browser.private_destinations` lists in force now
/// (`None` when the daemon has no `browser:` block), so `axocoatl network
/// reload` applies to the next run; an error is a usage error (exit 3).
pub fn validate_qa_admission(
    resolved: &ResolvedLoadout,
    exposed_ports: &[u16],
    browser: Option<(&[EgressAllowYaml], &[String])>,
) -> Result<QaRunSettings, RunError> {
    let settings = qa_settings(resolved)?;
    let (allow, private) = browser.ok_or_else(|| {
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
        browser_reaches(url, exposed_ports, allow, private)
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
                // The class already says not_reached or blocked; only a
                // status it does not name stays in the detail.
                let class = status_class(&area.status);
                let detail = match (&area.reason, class) {
                    (Some(reason), FailureClass::Other) => format!("{}: {reason}", area.status),
                    (Some(reason), _) => reason.clone(),
                    (None, FailureClass::Other) => area.status.clone(),
                    (None, _) => "the explorer gave no reason".to_string(),
                };
                not_covered.push(entry(&area.area, class, detail));
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
        // The entry's class is `class`; the detail does not repeat it.
        let mut detail = format!("the explorer did not finish ({message})");
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
            remaining.detail.starts_with("the explorer did not finish"),
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
        let report = QaDriver.drive(&host, &run).await.unwrap();
        assert_eq!(report.turns, vec![observed]);
        assert_eq!(
            classification(&report, "B1"),
            ReproClassification::Reproduced
        );
        assert_eq!(host.sent.lock().unwrap().len(), 1);
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
        let browser: (&[EgressAllowYaml], &[String]) = (&[], &[]);
        let run = context(
            "qa",
            &[("explorer_model", "openrouter:qwen/qwen3-coder")],
            repo.path(),
        );
        let settings = validate_qa_admission(&run.resolved, &[3000], Some(browser)).unwrap();
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
        assert!(
            usage(validate_qa_admission(&run.resolved, &[8080], Some(browser)))
                .starts_with("qa.target_url:")
        );
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
            Some(browser)
        ))
        .starts_with("qa.reference_url:"));
        assert!(
            validate_qa_admission(&with_reference.resolved, &[3000, 3001], Some(browser)).is_ok()
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
            Some(browser)
        ))
        .contains("is the build under test"));
        // A hidden reproductions directory cannot be re-run by browser_check.
        let mut hidden = run.resolved.clone();
        hidden.loadout.file.qa.as_mut().unwrap().repro_dir = ".axocoatl/qa".into();
        assert!(
            usage(validate_qa_admission(&hidden, &[3000], Some(browser)))
                .contains("not a path browser_check can read")
        );
    }

    /// A cidr entry of `browser.allow` admits an address in its range on
    /// one of its ports, as the browser container's own policy does; the
    /// range must also be a listed private destination.
    #[test]
    fn admission_accepts_a_cidr_entry_of_browser_allow() {
        let repo = repo_with(&[]);
        let run = context(
            "qa",
            &[
                ("explorer_model", "openrouter:qwen/qwen3-coder"),
                ("target_url", "http://192.168.1.5:8766"),
            ],
            repo.path(),
        );
        let allow = vec![EgressAllowYaml::Cidr(axocoatl_config::EgressCidrYaml {
            cidr: "192.168.1.0/24".into(),
            ports: Some(vec![8766]),
        })];
        let private = vec!["192.168.1.0/24".to_string()];
        let settings = validate_qa_admission(&run.resolved, &[], Some((&allow, &private))).unwrap();
        assert_eq!(settings.target_url, "http://192.168.1.5:8766");
        // Not a Session port: the URL names no port the Session exposes.
        assert_eq!(session_port(&settings.target_url), None);
        assert_eq!(session_port("http://localhost:3000/x"), Some(3000));
        assert_eq!(session_port("http://127.0.0.1:3001"), Some(3001));
        assert_eq!(session_port("http://[::1]:3002"), Some(3002));
        assert_eq!(session_port("https://shop.example.test"), None);
        // Another port of the range, or the range without its private
        // destination, is refused.
        let other_port = context(
            "qa",
            &[
                ("explorer_model", "openrouter:qwen/qwen3-coder"),
                ("target_url", "http://192.168.1.5:8767"),
            ],
            repo.path(),
        );
        assert!(matches!(
            validate_qa_admission(&other_port.resolved, &[], Some((&allow, &private))),
            Err(RunError::Usage(message)) if message.contains("browser.allow")
        ));
        assert!(matches!(
            validate_qa_admission(&run.resolved, &[], Some((&allow, &[]))),
            Err(RunError::Usage(message)) if message.contains("private")
        ));
    }

    /// A coverage area's detail does not repeat the class its status maps
    /// to; a status no class names stays in the detail.
    #[tokio::test]
    async fn not_covered_details_do_not_repeat_the_class() {
        let repo = repo_with(&[]);
        let run = context(
            "qa",
            &[("explorer_model", "openrouter:qwen/qwen3-coder")],
            repo.path(),
        );
        let answer = "FINDINGS\n```json\n[]\n```\nCOVERAGE\n```json\n[\
            {\"area\": \"checkout\", \"status\": \"not_reached\", \"reason\": \"ran out of steps\"},\
            {\"area\": \"admin\", \"status\": \"blocked\"},\
            {\"area\": \"search\", \"status\": \"partly\", \"reason\": \"only the first page\"}\
            ]\n```\n";
        let report = qa_report(
            &FakeHost::default(),
            &run,
            explorer_turn(NodeState::Accepted, Some(answer), None),
        )
        .await
        .unwrap();
        let entries: Vec<_> = report
            .not_covered
            .iter()
            .map(|entry| (entry.area.as_str(), entry.class, entry.detail.as_str()))
            .collect();
        assert_eq!(
            entries,
            [
                ("checkout", FailureClass::NotReached, "ran out of steps"),
                (
                    "admin",
                    FailureClass::Blocked,
                    "the explorer gave no reason"
                ),
                ("search", FailureClass::Other, "partly: only the first page"),
            ]
        );
    }

    /// Runs argv on this machine in the repository, as the Session container
    /// runs the explorer's file tools: write_file's own `sh -c 'cat > "$1"'`,
    /// with its real exit status.
    struct HostDirSandbox {
        root: std::path::PathBuf,
    }

    impl HostDirSandbox {
        fn run(
            &self,
            argv: &[&str],
            stdin: Option<&str>,
        ) -> Result<axocoatl_isolation::ExecResult, axocoatl_isolation::IsolationError> {
            use std::io::Write;
            use std::process::Stdio;
            let mut child = std::process::Command::new(argv[0])
                .args(&argv[1..])
                .current_dir(&self.root)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?;
            if let Some(text) = stdin {
                child
                    .stdin
                    .take()
                    .expect("piped stdin")
                    .write_all(text.as_bytes())?;
            }
            drop(child.stdin.take());
            let output = child.wait_with_output()?;
            Ok(axocoatl_isolation::ExecResult {
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                exit_code: output.status.code().unwrap_or(-1),
            })
        }
    }

    #[async_trait]
    impl axocoatl_isolation::Sandbox for HostDirSandbox {
        fn root(&self) -> &Path {
            &self.root
        }
        async fn exec(
            &self,
            argv: &[&str],
            _timeout: Duration,
        ) -> Result<axocoatl_isolation::ExecResult, axocoatl_isolation::IsolationError> {
            self.run(argv, None)
        }
        async fn exec_stdin(
            &self,
            argv: &[&str],
            stdin: &str,
            _timeout: Duration,
        ) -> Result<axocoatl_isolation::ExecResult, axocoatl_isolation::IsolationError> {
            self.run(argv, Some(stdin))
        }
        fn spawn_background(&self, _command: &str) -> String {
            unreachable!("the explorer's file tools never run in the background")
        }
        fn spawn_pty(
            &self,
            _command: &str,
            _rows: u16,
            _cols: u16,
        ) -> Result<std::sync::Arc<axocoatl_isolation::pty::PtyTerminal>, String> {
            Err("unused".to_string())
        }
        fn get_terminal(
            &self,
            _id: &str,
        ) -> Option<std::sync::Arc<axocoatl_isolation::pty::PtyTerminal>> {
            None
        }
        fn kill_terminal(&self, _id: &str) -> bool {
            false
        }
        fn list_terminals(&self) -> Vec<(String, String, bool)> {
            Vec::new()
        }
        fn list_tasks(&self) -> Vec<axocoatl_isolation::session_sandbox::BgTask> {
            Vec::new()
        }
        fn with_root(&self, root: &Path) -> std::sync::Arc<dyn axocoatl_isolation::Sandbox> {
            std::sync::Arc::new(Self {
                root: root.to_path_buf(),
            })
        }
        async fn stop(&self) {}
    }

    /// The real session file tools over the repository at `root`.
    fn file_tools(root: &Path) -> axocoatl_tools::ToolExecutor {
        let mut tools = axocoatl_tools::ToolExecutor::new();
        axocoatl_tools::register_session_tools(
            &mut tools,
            std::sync::Arc::new(HostDirSandbox {
                root: root.to_path_buf(),
            }),
        );
        tools
    }

    /// A host whose explorer turn writes its reproductions with the real
    /// write_file tool, as the explorer does in the Session container, and
    /// keeps each tool result.
    struct WritingHost {
        inner: FakeHost,
        tools: axocoatl_tools::ToolExecutor,
        writes: Vec<(&'static str, &'static str)>,
        results: Mutex<Vec<Result<serde_json::Value, String>>>,
    }

    #[async_trait]
    impl RunHost for WritingHost {
        async fn apply_team(
            &self,
            session_id: &str,
            edit: crate::SessionTeamEdit,
        ) -> Result<(), RunError> {
            self.inner.apply_team(session_id, edit).await
        }
        async fn send_turn(&self, session_id: &str, request: &str) -> Result<String, RunError> {
            for (path, content) in &self.writes {
                let result = self
                    .tools
                    .execute(
                        "write_file",
                        serde_json::json!({ "path": path, "content": content }),
                    )
                    .await
                    .map_err(|error| error.to_string());
                self.results.lock().unwrap().push(result);
            }
            self.inner.send_turn(session_id, request).await
        }
        async fn wait_turn(
            &self,
            session_id: &str,
            turn_id: &str,
            deadline: Instant,
        ) -> Result<TurnObservation, RunError> {
            self.inner.wait_turn(session_id, turn_id, deadline).await
        }
        async fn stop_turn(&self, session_id: &str, turn_id: &str) -> Result<(), RunError> {
            self.inner.stop_turn(session_id, turn_id).await
        }
        async fn run_repro(
            &self,
            session_id: &str,
            request: &ReproRequest,
        ) -> Result<ReproRun, RunError> {
            self.inner.run_repro(session_id, request).await
        }
        async fn read_sandbox_file(
            &self,
            session_id: &str,
            path: &str,
            max_bytes: usize,
        ) -> Result<Option<Vec<u8>>, RunError> {
            self.inner
                .read_sandbox_file(session_id, path, max_bytes)
                .await
        }
        async fn record(&self, run_id: &str, event: RunEvent) -> Result<(), RunError> {
            self.inner.record(run_id, event).await
        }
    }

    /// The explorer has write_file but no bash or edit_file, and write_file
    /// creates no directory. On a repository without `axocoatl-qa/` the
    /// driver creates it before the explorer's turn, so the real tool's
    /// write lands and the finding is reproduced from that file; without the
    /// driver the same write fails.
    #[tokio::test]
    async fn the_explorer_writes_reproductions_into_a_repository_without_the_directory() {
        let repo = repo_with(&[]);
        let root = repo.path().canonicalize().unwrap();
        let spec = "import { test } from '@playwright/test';\ntest('b1', async () => {});\n";
        // The real tool on its own: the directory is missing, so it fails.
        let refused = file_tools(&root)
            .execute(
                "write_file",
                serde_json::json!({ "path": "axocoatl-qa/probe.spec.ts", "content": spec }),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(!root.join("axocoatl-qa").exists(), "{refused}");

        let run = context(
            "qa",
            &[("explorer_model", "openrouter:qwen/qwen3-coder")],
            &root,
        );
        let answer = report(
            &[("B1", Some("axocoatl-qa/b1.spec.ts"))],
            &[("checkout", "covered")],
        );
        let host = WritingHost {
            inner: FakeHost::with_turn(explorer_turn(NodeState::Accepted, Some(&answer), None)),
            tools: file_tools(&root),
            writes: vec![("axocoatl-qa/b1.spec.ts", spec)],
            results: Mutex::new(Vec::new()),
        };
        host.inner
            .script("axocoatl-qa/b1.spec.ts", TARGET, "failed");
        let report = QaDriver.drive(&host, &run).await.unwrap();
        let results = host.results.lock().unwrap().clone();
        assert!(results.iter().all(Result::is_ok), "{results:?}");
        assert_eq!(
            classification(&report, "B1"),
            ReproClassification::Reproduced
        );
        let metadata = std::fs::symlink_metadata(root.join("axocoatl-qa")).unwrap();
        assert!(metadata.is_dir() && !metadata.file_type().is_symlink());
        assert_eq!(
            std::fs::read_to_string(root.join("axocoatl-qa/b1.spec.ts")).unwrap(),
            spec
        );
        let repro = report.findings[0].repro.as_ref().unwrap();
        assert_eq!(
            repro.sha256.as_deref(),
            Some(format!("{:x}", Sha256::digest(spec.as_bytes())).as_str())
        );
        assert!(host.inner.events().iter().any(|event| matches!(
            event,
            RunEvent::Phase { phase, detail, .. }
                if phase == "preparing" && detail.contains("created axocoatl-qa/")
        )));

        // A later run on the same repository finds the directory and
        // creates nothing.
        let again = FakeHost::with_turn(explorer_turn(
            NodeState::Accepted,
            Some(&report_text_without_findings()),
            None,
        ));
        QaDriver.drive(&again, &run).await.unwrap();
        assert!(!again
            .events()
            .iter()
            .any(|event| matches!(event, RunEvent::Phase { phase, .. } if phase == "preparing")));
    }

    fn report_text_without_findings() -> String {
        report(&[], &[("checkout", "covered")])
    }

    /// A reproductions directory that exists as a symbolic link or as a file
    /// is refused before the explorer's turn, and nothing is created or
    /// written through the link. Admission refuses it too, and creates
    /// nothing for a missing directory.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_linked_or_file_reproductions_directory_is_refused() {
        let run_on = |root: &Path| {
            context(
                "qa",
                &[("explorer_model", "openrouter:qwen/qwen3-coder")],
                root,
            )
        };
        let refused = |result: Result<KindReport, RunError>| match result {
            Err(RunError::Usage(message)) => message,
            other => panic!("{other:?}"),
        };

        let linked = repo_with(&[]);
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), linked.path().join("axocoatl-qa")).unwrap();
        let run = run_on(linked.path());
        let host = FakeHost::with_turn(explorer_turn(
            NodeState::Accepted,
            Some(&report_text_without_findings()),
            None,
        ));
        let message = refused(QaDriver.drive(&host, &run).await);
        assert!(
            message.contains("axocoatl-qa") && message.contains("symbolic link"),
            "{message}"
        );
        assert!(
            host.sent.lock().unwrap().is_empty(),
            "no turn after a refusal"
        );
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
        let settings = qa_settings(&run.resolved).unwrap();
        assert!(matches!(
            check_repro_dir(linked.path(), &settings),
            Err(RunError::Usage(message)) if message.contains("symbolic link")
        ));

        // A dangling link is refused the same way.
        let dangling = repo_with(&[]);
        std::os::unix::fs::symlink(
            outside.path().join("missing"),
            dangling.path().join("axocoatl-qa"),
        )
        .unwrap();
        let message = refused(QaDriver.drive(&host, &run_on(dangling.path())).await);
        assert!(message.contains("symbolic link"), "{message}");
        assert!(!outside.path().join("missing").exists());

        // A file in its place.
        let file = repo_with(&["axocoatl-qa"]);
        let message = refused(QaDriver.drive(&host, &run_on(file.path())).await);
        assert!(message.contains("not a directory"), "{message}");
        assert!(std::fs::symlink_metadata(file.path().join("axocoatl-qa"))
            .unwrap()
            .is_file());

        // A nested directory under a linked parent.
        let nested = repo_with(&[]);
        std::os::unix::fs::symlink(outside.path(), nested.path().join("qa")).unwrap();
        let mut deep = run_on(nested.path());
        deep.resolved.loadout.file.qa.as_mut().unwrap().repro_dir = "qa/repros".into();
        let message = refused(QaDriver.drive(&host, &deep).await);
        assert!(message.contains("symbolic link"), "{message}");
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);

        // Admission creates nothing for a missing directory.
        let fresh = repo_with(&[]);
        check_repro_dir(fresh.path(), &settings).unwrap();
        assert!(!fresh.path().join("axocoatl-qa").exists());
    }
}
