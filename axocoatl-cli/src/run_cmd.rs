//! `axocoatl run`, `axocoatl loadouts`, `axocoatl record`. They talk to the
//! running daemon over its HTTP API with the local API token (or
//! `AXOCOATL_URL` / `AXOCOATL_TOKEN`). Owner: workstream `core`.
//! Contract: docs/design/1.3-loadouts.md ("axocoatl run").
//!
//! `axocoatl run` never starts a daemon. Progress goes to stderr, the
//! summary (or the Outcome as JSON) to stdout, and the process exits with
//! the Outcome's exit code: 0 pass, 1 checks failed, 2 needs attention, 3
//! usage, 4 daemon unreachable or token refused, 5 infrastructure, 6
//! interrupted.

use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use axocoatl_daemon::loadout::api::{
    LoadoutSummary, LoadoutView, RunAccepted, RunEventsPage, RunStatusView,
};
use axocoatl_session::run_outcome::{
    exit_code, AdjudicationDecision, CheckState, ReproClassification, RunOutcome, RunWarning,
};
use axocoatl_session::run_record::RunEvent;
use clap::{Args, Subcommand};

/// `axocoatl run <loadout> --task "..."`.
#[derive(Debug, Args)]
pub struct RunArgs {
    /// Loadout id (`fix`, `qa`, `audit`, or a user loadout)
    pub loadout: String,
    /// The task, as the person would type it
    #[arg(long, conflicts_with = "task_file")]
    pub task: Option<String>,
    /// Read the task from a file (`-` for stdin)
    #[arg(long)]
    pub task_file: Option<PathBuf>,
    /// Repository directory (default: the current directory)
    #[arg(long)]
    pub repo: Option<PathBuf>,
    /// Write JUnit XML of checks and findings here
    #[arg(long)]
    pub junit: Option<PathBuf>,
    /// Write the run's record bundle (one JSON Lines file) here
    #[arg(long)]
    pub record: Option<PathBuf>,
    /// A model parameter as ROLE=provider:model (sets the `ROLE_model` parameter)
    #[arg(long = "model", value_name = "ROLE=PROVIDER:MODEL")]
    pub models: Vec<String>,
    /// A loadout parameter as NAME=VALUE
    #[arg(long = "param", value_name = "NAME=VALUE")]
    pub params: Vec<String>,
    /// The command a `detected` check runs, in place of the repository's
    /// detected check command (needed when none is detected)
    #[arg(long)]
    pub check: Option<String>,
    /// The exact setup command this run approves
    #[arg(long)]
    pub setup: Option<String>,
    /// What to do with a passing run's changes: none, branch or pr
    #[arg(long, default_value = "none")]
    pub keep: String,
    /// Print the Outcome as JSON on stdout instead of the summary
    #[arg(long)]
    pub json: bool,
    /// Daemon URL (default: AXOCOATL_URL, then the configured server address)
    #[arg(long)]
    pub url: Option<String>,
    /// Config file used to find the daemon's address and data directory
    #[arg(short, long, default_value_os_t = crate::default_config_path_for_clap())]
    pub config: PathBuf,
}

#[derive(Debug, Subcommand)]
pub enum LoadoutCommands {
    /// List built-in and user loadouts
    List {
        #[arg(long)]
        url: Option<String>,
        /// Config file used to find the daemon's address and data directory
        #[arg(short, long, default_value_os_t = crate::default_config_path_for_clap())]
        config: PathBuf,
    },
    /// Show one loadout's file and graph
    Show {
        id: String,
        #[arg(long)]
        url: Option<String>,
        /// Config file used to find the daemon's address and data directory
        #[arg(short, long, default_value_os_t = crate::default_config_path_for_clap())]
        config: PathBuf,
    },
    /// Validate a loadout file without the daemon
    Validate { file: PathBuf },
}

#[derive(Debug, Subcommand)]
pub enum RecordCommands {
    /// Verify a record bundle's order, line count and digest
    Verify { file: PathBuf },
}

/// Why a command stopped, with its exit code.
#[derive(Debug, PartialEq, Eq)]
pub struct Failure {
    pub code: i32,
    pub message: String,
}

impl Failure {
    fn usage(message: impl Into<String>) -> Self {
        Self {
            code: exit_code::USAGE,
            message: message.into(),
        }
    }
    fn unreachable(message: impl Into<String>) -> Self {
        Self {
            code: exit_code::DAEMON_UNAVAILABLE,
            message: message.into(),
        }
    }
    fn infrastructure(message: impl Into<String>) -> Self {
        Self {
            code: exit_code::INFRASTRUCTURE,
            message: message.into(),
        }
    }
}

/// What to do with a passing run's changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keep {
    None,
    Branch,
    Pr,
}

/// `RunArgs` checked and turned into a request.
#[derive(Debug, PartialEq, Eq)]
pub struct RunPlan {
    pub loadout: String,
    pub task: String,
    pub repo: PathBuf,
    pub params: BTreeMap<String, String>,
    pub keep: Keep,
    pub check_command: Option<String>,
    pub setup_command: Option<String>,
}

/// Check the flags of `axocoatl run`: `--model ROLE=provider:model` is
/// `--param ROLE_model=provider:model`; a parameter given twice with two
/// values is refused.
pub fn plan(args: &RunArgs, task_from_file: Option<String>) -> Result<RunPlan, Failure> {
    let keep = match args.keep.as_str() {
        "none" => Keep::None,
        "branch" => Keep::Branch,
        "pr" => Keep::Pr,
        other => {
            return Err(Failure::usage(format!(
                "--keep {other:?}: write none, branch or pr"
            )))
        }
    };
    let task = match (&args.task, task_from_file) {
        (Some(task), None) => task.clone(),
        (None, Some(task)) => task,
        (None, None) => {
            return Err(Failure::usage(
                "say what to do: --task \"...\" or --task-file <file|->",
            ))
        }
        (Some(_), Some(_)) => return Err(Failure::usage("give --task or --task-file, not both")),
    };
    if task.trim().is_empty() {
        return Err(Failure::usage("the task is empty"));
    }
    let mut params = BTreeMap::new();
    let mut set = |name: String, value: String, flag: &str| -> Result<(), Failure> {
        if name.is_empty() || value.is_empty() {
            return Err(Failure::usage(format!("{flag}: write NAME=VALUE")));
        }
        match params.get(&name) {
            Some(existing) if existing != &value => Err(Failure::usage(format!(
                "parameter {name} is given twice with different values"
            ))),
            _ => {
                params.insert(name, value);
                Ok(())
            }
        }
    };
    for model in &args.models {
        let Some((role, value)) = model.split_once('=') else {
            return Err(Failure::usage(format!(
                "--model {model:?}: write ROLE=provider:model, such as writer=openrouter:qwen/qwen3-coder"
            )));
        };
        let role = role.trim();
        if !value.contains(':') {
            return Err(Failure::usage(format!(
                "--model {model:?}: the model is provider:model"
            )));
        }
        set(format!("{role}_model"), value.trim().to_string(), "--model")?;
    }
    for param in &args.params {
        let Some((name, value)) = param.split_once('=') else {
            return Err(Failure::usage(format!(
                "--param {param:?}: write NAME=VALUE"
            )));
        };
        set(name.trim().to_string(), value.to_string(), "--param")?;
    }
    let repo = match &args.repo {
        Some(repo) => repo.clone(),
        None => std::env::current_dir()
            .map_err(|error| Failure::usage(format!("the current directory: {error}")))?,
    };
    let repo = std::fs::canonicalize(&repo)
        .map_err(|error| Failure::usage(format!("--repo {}: {error}", repo.display())))?;
    if !repo.is_dir() {
        return Err(Failure::usage(format!(
            "--repo {} is not a directory",
            repo.display()
        )));
    }
    Ok(RunPlan {
        loadout: args.loadout.clone(),
        task,
        repo,
        params,
        keep,
        check_command: args.check.clone(),
        setup_command: args.setup.clone(),
    })
}

fn read_task_file(path: &Path) -> Result<String, Failure> {
    let mut text = String::new();
    if path == Path::new("-") {
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)
            .map_err(|error| Failure::usage(format!("reading the task from stdin: {error}")))?;
    } else {
        text = std::fs::read_to_string(path)
            .map_err(|error| Failure::usage(format!("--task-file {}: {error}", path.display())))?;
    }
    Ok(text)
}

/// The daemon to talk to and how to authenticate.
pub struct Daemon {
    base: String,
    token: Option<String>,
    client: reqwest::Client,
}

fn connect_host(host: &str) -> &str {
    match host {
        "0.0.0.0" | "" => "127.0.0.1",
        "::" | "[::]" => "[::1]",
        other => other,
    }
}

impl Daemon {
    /// `--url`, else `AXOCOATL_URL`, else the configured server address;
    /// `AXOCOATL_TOKEN`, else the local API token from the data directory.
    pub async fn locate(url: Option<&str>, config: &Path) -> Result<Self, Failure> {
        let base = match url.map(str::to_string).or_else(|| {
            std::env::var("AXOCOATL_URL")
                .ok()
                .filter(|url| !url.is_empty())
        }) {
            Some(url) => url,
            None => {
                let loaded = axocoatl_config::load_config(config)
                    .await
                    .map_err(|error| {
                        Failure::unreachable(format!(
                        "cannot tell where the daemon listens: {} could not be read ({error}); \
                         pass --url or set AXOCOATL_URL",
                        config.display()
                    ))
                    })?;
                crate::plain_server_url(connect_host(&loaded.server.host), loaded.server.port)
            }
        };
        let base = base.trim_end_matches('/').to_string();
        if !(base.starts_with("http://") || base.starts_with("https://")) {
            return Err(Failure::usage(format!(
                "{base:?} is not an http:// or https:// URL"
            )));
        }
        let token = match std::env::var("AXOCOATL_TOKEN")
            .ok()
            .filter(|t| !t.is_empty())
        {
            Some(token) => Some(token),
            None => {
                let data_dir = std::env::var_os("AXOCOATL_DATA_DIR")
                    .filter(|value| !value.is_empty())
                    .map(PathBuf::from)
                    .unwrap_or_else(|| crate::default_data_dir_for_config(config));
                match axocoatl_server::auth::read_local_token_at(&data_dir) {
                    Ok(Some(secret)) => Some(secret.expose_secret().to_string()),
                    Ok(None) => None,
                    Err(error) => {
                        return Err(Failure::unreachable(format!(
                            "the local API token cannot be read: {error}"
                        )))
                    }
                }
            }
        };
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|error| Failure::infrastructure(error.to_string()))?;
        Ok(Self {
            base,
            token,
            client,
        })
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let builder = self.client.request(method, format!("{}{path}", self.base));
        match &self.token {
            Some(token) => builder.bearer_auth(token),
            None => builder,
        }
    }

    async fn send(&self, builder: reqwest::RequestBuilder) -> Result<reqwest::Response, Failure> {
        let response = builder.send().await.map_err(|error| {
            Failure::unreachable(format!(
                "the daemon at {} cannot be reached: {error}. Start it with `axocoatl serve`",
                self.base
            ))
        })?;
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let text = response.text().await.unwrap_or_default();
        let message = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|value| {
                value
                    .get("error")
                    .and_then(|e| e.as_str())
                    .map(str::to_string)
            })
            .unwrap_or(text);
        Err(match status.as_u16() {
            401 | 403 => Failure::unreachable(format!(
                "the daemon refused the API token ({status}): {message}"
            )),
            404 | 422 => Failure::usage(message),
            _ => Failure::infrastructure(format!("{status}: {message}")),
        })
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T, Failure> {
        self.send(self.request(reqwest::Method::GET, path))
            .await?
            .json()
            .await
            .map_err(|error| Failure::infrastructure(format!("reading {path}: {error}")))
    }

    async fn post_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<T, Failure> {
        self.send(self.request(reqwest::Method::POST, path).json(body))
            .await?
            .json()
            .await
            .map_err(|error| Failure::infrastructure(format!("reading {path}: {error}")))
    }
}

/// Write `bytes` to `path` atomically: a temporary file beside it, then a
/// rename.
fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("output");
    let temporary = parent.join(format!(".{name}.{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

/// Stream the record bundle to `path` atomically.
async fn download_record(daemon: &Daemon, run_id: &str, path: &Path) -> Result<(), Failure> {
    let mut response = daemon
        .send(daemon.request(reqwest::Method::GET, &format!("/api/runs/{run_id}/record")))
        .await?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("record");
    let temporary = parent.join(format!(".{name}.{}.tmp", uuid::Uuid::new_v4()));
    let write = async {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|error| error.to_string())?;
        while let Some(chunk) = response.chunk().await.map_err(|error| error.to_string())? {
            file.write_all(&chunk).map_err(|error| error.to_string())?;
        }
        file.sync_all().map_err(|error| error.to_string())?;
        std::fs::rename(&temporary, path).map_err(|error| error.to_string())
    };
    write.await.map_err(|error| {
        let _ = std::fs::remove_file(&temporary);
        Failure::infrastructure(format!("writing the record to {}: {error}", path.display()))
    })
}

/// Where `axocoatl run` writes: the summary (or the Outcome JSON) on `out`,
/// progress, warnings and errors on `err`.
pub struct Console {
    out: Box<dyn Write + Send>,
    err: Box<dyn Write + Send>,
}

impl Console {
    /// Standard output and standard error.
    pub fn stdio() -> Self {
        Self {
            out: Box::new(std::io::stdout()),
            err: Box::new(std::io::stderr()),
        }
    }

    /// `text` on standard output, as given.
    fn print(&mut self, text: &str) {
        let _ = self.out.write_all(text.as_bytes());
        let _ = self.out.flush();
    }

    /// One line on standard output.
    fn say(&mut self, line: &str) {
        self.print(&format!("{line}\n"));
    }

    /// One line of progress on standard error.
    fn note(&mut self, line: &str) {
        let _ = writeln!(self.err, "{line}");
        let _ = self.err.flush();
    }
}

/// The warnings already printed. Admission returns the run's warnings and
/// also records each one as a `Warning` run event, so without this every
/// admission warning would print twice.
#[derive(Default)]
struct ShownWarnings(HashSet<(String, String)>);

impl ShownWarnings {
    /// The line for `warning`, unless the same warning was printed already.
    fn line(&mut self, warning: &RunWarning) -> Option<String> {
        self.0
            .insert((warning.code.clone(), warning.message.clone()))
            .then(|| warning_line(warning))
    }
}

fn warning_line(warning: &RunWarning) -> String {
    format!("! warning {}: {}", warning.code, warning.message)
}

/// One line of progress for a recorded event, when it is worth one. Classes
/// and states are written as the Outcome JSON and the JUnit file write them
/// (`not_reached`, `needs_attention`).
pub fn progress_line(event: &RunEvent) -> Option<String> {
    match event {
        RunEvent::Phase { phase, detail, .. } => Some(if detail.is_empty() {
            format!("· {phase}")
        } else {
            format!("· {phase}: {detail}")
        }),
        RunEvent::TurnStarted {
            turn_id, purpose, ..
        } => Some(format!("· turn {turn_id} started ({purpose})")),
        RunEvent::TurnEnded { turn_id, state, .. } => {
            Some(format!("· turn {turn_id} ended: {}", class_name(state)))
        }
        RunEvent::Warning { warning, .. } => Some(warning_line(warning)),
        RunEvent::NotCovered { entry, .. } => {
            Some(format!("! not covered: {}: {}", entry.area, entry.reason()))
        }
        RunEvent::ProviderRetry {
            node_id,
            status,
            reason,
            ..
        } => Some(format!(
            "· provider call retried for {node_id}{}: {reason}",
            status.map(|s| format!(" (HTTP {s})")).unwrap_or_default()
        )),
        _ => None,
    }
}

fn class_name<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// The summary `axocoatl run` prints.
pub fn summary(outcome: &RunOutcome) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Verdict: {} (exit {})",
        class_name(&outcome.verdict),
        outcome.exit_code
    );
    let _ = writeln!(
        out,
        "Loadout: {}@{} sha256:{}",
        outcome.loadout.id, outcome.loadout.version, outcome.loadout.digest
    );
    if let Some(error) = &outcome.error {
        let _ = writeln!(out, "Error: {error}");
    }
    for reason in &outcome.attention {
        let _ = writeln!(out, "Needs attention: {reason}");
    }
    if outcome.checks.is_empty() {
        let _ = writeln!(out, "Checks: none");
    } else {
        let _ = writeln!(out, "Checks:");
        for check in &outcome.checks {
            let state = match check.state {
                CheckState::Passed => "passed",
                CheckState::Failed => "FAILED",
                CheckState::TimedOut => "TIMED OUT",
                CheckState::NotRun => "not run",
                CheckState::Unavailable => "unavailable",
            };
            let mut line = format!("  {:<16} {state}", check.name);
            if let Some(code) = check.exit_code {
                let _ = write!(line, ", exit {code}");
            }
            if let Some(report) = &check.report {
                let _ = write!(
                    line,
                    ", report: {} passed, {} failed, {} skipped, {} errors",
                    report.passed, report.failed, report.skipped, report.errors
                );
            }
            if let Some(reason) = &check.reason {
                let _ = write!(line, " ({reason})");
            }
            let _ = writeln!(out, "{line}");
        }
    }
    match &outcome.review {
        Some(review) => {
            let _ = writeln!(
                out,
                "Review: {} after {} of {} round{} by {}:{}{}",
                review.state,
                review.rounds.len(),
                review.max_rounds,
                if review.max_rounds == 1 { "" } else { "s" },
                review.reviewer.provider,
                review.reviewer.model,
                if review.passed { "" } else { " (not passed)" }
            );
        }
        None => {
            let _ = writeln!(out, "Review: none");
        }
    }
    if !outcome.adjudications.is_empty() {
        let _ = writeln!(out, "Adjudications:");
        for adjudication in &outcome.adjudications {
            let decision = match adjudication.decision {
                AdjudicationDecision::Accept => "accept",
                AdjudicationDecision::Reject => "reject",
                AdjudicationDecision::Missing => "MISSING",
            };
            let _ = writeln!(
                out,
                "  round {} {:<4} {decision}: {}",
                adjudication.round,
                adjudication.finding_id,
                if adjudication.reason.is_empty() {
                    "the writer did not answer this finding"
                } else {
                    adjudication.reason.as_str()
                }
            );
        }
    }
    if !outcome.findings.is_empty() {
        let _ = writeln!(out, "Findings:");
        for finding in &outcome.findings {
            let label = match finding.repro.as_ref().map(|repro| repro.classification) {
                Some(ReproClassification::Confirmed) => "confirmed".to_string(),
                Some(ReproClassification::FailsOnCleanBuild) => "fails on clean build".into(),
                Some(ReproClassification::Reproduced) => "reproduced (no reference)".into(),
                Some(ReproClassification::NotReproduced) => "not reproduced".into(),
                Some(ReproClassification::ReproError) => "reproduction error".into(),
                Some(ReproClassification::Missing) => "no reproduction".into(),
                None => class_name(&finding.source),
            };
            let area = finding
                .area
                .as_deref()
                .map(|area| format!(" [{area}]"))
                .unwrap_or_default();
            let mut facts = Vec::new();
            if let Some(severity) = &finding.severity {
                facts.push(format!("{} severity", class_name(severity)));
            }
            if let Some(location) = finding
                .location
                .as_deref()
                .filter(|location| !location.trim().is_empty())
            {
                facts.push(location.to_string());
            }
            let facts = if facts.is_empty() {
                String::new()
            } else {
                format!(" ({})", facts.join(", "))
            };
            let _ = writeln!(
                out,
                "  {} {}{area}{facts}: {label}",
                finding.id, finding.title
            );
        }
    }
    if !outcome.not_covered.is_empty() {
        let _ = writeln!(out, "Not covered:");
        for entry in &outcome.not_covered {
            let _ = writeln!(out, "  {}: {}", entry.area, entry.reason());
        }
    }
    if !outcome.warnings.is_empty() {
        let _ = writeln!(out, "Warnings:");
        for warning in &outcome.warnings {
            let _ = writeln!(out, "  {}: {}", warning.code, warning.message);
        }
    }
    let _ = writeln!(out, "Usage: {}", outcome.usage.text());
    let network = &outcome.network;
    let _ = writeln!(
        out,
        "Network: {} recorded events, {} allowed and {} refused connections, {} route requests",
        network.events,
        network.allowed_connections,
        network.refused_connections,
        network.route_requests
    );
    if let Some(keep) = &outcome.keep {
        let _ = writeln!(out, "Kept: branch {} at {}", keep.branch, keep.commit);
    }
    let _ = writeln!(out, "Record: {}", outcome.run_id);
    out
}

/// How long admission may take before `axocoatl run` says it is waiting.
const ADMISSION_NOTE_AFTER: Duration = Duration::from_secs(3);

/// Run `axocoatl run`; returns the process exit code.
pub async fn cmd_run(args: RunArgs) -> i32 {
    run_on(args, &mut Console::stdio(), ADMISSION_NOTE_AFTER).await
}

/// `axocoatl run` writing to `console`; returns the process exit code.
async fn run_on(args: RunArgs, console: &mut Console, note_after: Duration) -> i32 {
    match run(args, console, note_after).await {
        Ok(code) => code,
        Err(failure) => {
            console.note(&format!("axocoatl run: {}", failure.message));
            failure.code
        }
    }
}

async fn run(args: RunArgs, console: &mut Console, note_after: Duration) -> Result<i32, Failure> {
    let task_from_file = match &args.task_file {
        Some(path) => Some(read_task_file(path)?),
        None => None,
    };
    let plan = plan(&args, task_from_file)?;
    let daemon = Daemon::locate(args.url.as_deref(), &args.config).await?;
    let request = serde_json::json!({
        "loadout": plan.loadout,
        "task": plan.task,
        "repo": plan.repo.display().to_string(),
        "params": plan.params,
        "keep": match plan.keep { Keep::None => "none", Keep::Branch => "branch", Keep::Pr => "pr" },
        "check_command": plan.check_command,
        "setup_command": plan.setup_command,
        "request_id": format!("cli-{}", uuid::Uuid::new_v4()),
    });
    let accepted = admit(&daemon, &request, console, note_after).await?;
    // Only an admitted run starts: a refused one prints its reason and
    // nothing that says otherwise.
    console.note(&format!(
        "· starting loadout {} in {}",
        plan.loadout,
        plan.repo.display()
    ));
    console.note(&format!(
        "· run {} in Session {}",
        accepted.run_id, accepted.session_id
    ));
    let mut shown = ShownWarnings::default();
    for warning in &accepted.warnings {
        if let Some(line) = shown.line(warning) {
            console.note(&line);
        }
    }
    let interrupted = follow(&daemon, &accepted.run_id, console, &mut shown).await?;
    let status = if interrupted {
        console.note("· stopping the run (Ctrl-C)");
        let _ = daemon
            .send(daemon.request(
                reqwest::Method::POST,
                &format!("/api/runs/{}/stop", accepted.run_id),
            ))
            .await;
        wait_finished(&daemon, &accepted.run_id, Duration::from_secs(30)).await
    } else {
        daemon
            .get_json::<RunStatusView>(&format!("/api/runs/{}", accepted.run_id))
            .await
    };
    let outcome = status
        .as_ref()
        .ok()
        .and_then(|status| status.outcome.clone());
    match &outcome {
        Some(outcome) if args.json => {
            console.say(&serde_json::to_string_pretty(outcome).unwrap_or_default());
        }
        Some(outcome) => console.print(&summary(outcome)),
        None => console.note("· the run has not finished; its record is incomplete"),
    }
    // Keep runs before the files are written: it records a `keep` event,
    // and `--record` must be the same bundle `GET /api/runs/{id}/record`
    // returns once the command has ended.
    let mut keep_failed = false;
    if let (Some(outcome), false) = (&outcome, interrupted) {
        if plan.keep != Keep::None {
            if outcome.exit_code == exit_code::PASS {
                keep_failed = !keep_changes(&daemon, outcome, plan.keep, args.json, console).await;
            } else {
                console.note("· nothing kept: the run did not pass");
            }
        }
    }
    // The files are written whatever the verdict, also after a failure.
    let mut file_failure = None;
    if let Some(path) = &args.junit {
        let written = async {
            let xml = daemon
                .send(daemon.request(
                    reqwest::Method::GET,
                    &format!("/api/runs/{}/junit", accepted.run_id),
                ))
                .await?
                .bytes()
                .await
                .map_err(|error| Failure::infrastructure(error.to_string()))?;
            write_atomically(path, &xml).map_err(|error| {
                Failure::infrastructure(format!("writing {}: {error}", path.display()))
            })
        }
        .await;
        match written {
            Ok(()) => console.note(&format!("· JUnit written to {}", path.display())),
            Err(failure) => {
                console.note(&format!("axocoatl run: JUnit: {}", failure.message));
                file_failure = Some(failure);
            }
        }
    }
    if let Some(path) = &args.record {
        match download_record(&daemon, &accepted.run_id, path).await {
            Ok(()) => console.note(&format!("· record written to {}", path.display())),
            Err(failure) => {
                console.note(&format!("axocoatl run: record: {}", failure.message));
                file_failure = Some(failure);
            }
        }
    }
    if interrupted {
        return Ok(exit_code::INTERRUPTED);
    }
    let outcome = match outcome {
        Some(outcome) => outcome,
        None => {
            return Err(status
                .err()
                .unwrap_or_else(|| Failure::infrastructure("the run ended without an Outcome")))
        }
    };
    if keep_failed {
        return Ok(exit_code::NEEDS_ATTENTION);
    }
    if outcome.exit_code == exit_code::PASS {
        if let Some(failure) = file_failure {
            return Err(failure);
        }
    }
    Ok(outcome.exit_code)
}

/// `POST /api/runs`. The daemon prepares the Session's environment before
/// it answers, which can take minutes (an image pull, the setup command);
/// when it takes longer than `note_after`, say that the command is waiting,
/// without saying that the run started.
async fn admit(
    daemon: &Daemon,
    request: &serde_json::Value,
    console: &mut Console,
    note_after: Duration,
) -> Result<RunAccepted, Failure> {
    let admission = daemon.post_json::<RunAccepted>("/api/runs", request);
    tokio::pin!(admission);
    tokio::select! {
        accepted = &mut admission => return accepted,
        () = tokio::time::sleep(note_after) => {}
    }
    console.note(
        "· waiting for the daemon to admit the run (it prepares the Session's environment first)",
    );
    admission.await
}

/// Keep a passing run's changes (`POST /api/sessions/{id}/keep-pr`) and
/// say what was kept: on standard output after the summary, or on standard
/// error with `--json`, so standard output stays one JSON document. Returns
/// whether Keep succeeded.
async fn keep_changes(
    daemon: &Daemon,
    outcome: &RunOutcome,
    keep: Keep,
    json: bool,
    console: &mut Console,
) -> bool {
    let report = |console: &mut Console, line: &str| {
        if json {
            console.note(line)
        } else {
            console.say(line)
        }
    };
    let body = serde_json::json!({
        "run_id": outcome.run_id,
        "open_pr": keep == Keep::Pr,
    });
    match daemon
        .post_json::<serde_json::Value>(
            &format!("/api/sessions/{}/keep-pr", outcome.session_id),
            &body,
        )
        .await
    {
        Ok(kept) => {
            report(
                console,
                &format!(
                    "Kept: branch {} at {}",
                    kept["branch"].as_str().unwrap_or("?"),
                    kept["commit"].as_str().unwrap_or("?")
                ),
            );
            if let Some(url) = kept["pull_request_url"].as_str() {
                report(console, &format!("Pull request: {url}"));
            }
            for warning in kept["warnings"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|warning| warning.as_str())
            {
                console.note(&format!("! keep: {warning}"));
            }
            true
        }
        Err(failure) => {
            report(console, &format!("Keep failed: {}", failure.message));
            false
        }
    }
}

/// Print the run's progress until it finishes. Returns whether the person
/// pressed Ctrl-C.
async fn follow(
    daemon: &Daemon,
    run_id: &str,
    console: &mut Console,
    shown: &mut ShownWarnings,
) -> Result<bool, Failure> {
    let mut after: Option<u64> = None;
    let mut failures = 0u32;
    let interrupt = tokio::signal::ctrl_c();
    tokio::pin!(interrupt);
    loop {
        let path = match after {
            Some(after) => format!("/api/runs/{run_id}/events?after={after}&wait_ms=1000"),
            None => format!("/api/runs/{run_id}/events?wait_ms=1000"),
        };
        let page = tokio::select! {
            _ = &mut interrupt => return Ok(true),
            page = daemon.get_json::<RunEventsPage>(&path) => page,
        };
        let page = match page {
            Ok(page) => {
                failures = 0;
                page
            }
            Err(failure) if failure.code == exit_code::DAEMON_UNAVAILABLE && failures < 5 => {
                failures += 1;
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
            Err(failure) => return Err(failure),
        };
        for (_, event) in &page.events {
            let line = match event {
                RunEvent::Warning { warning, .. } => shown.line(warning),
                event => progress_line(event),
            };
            if let Some(line) = line {
                console.note(&line);
            }
        }
        after = page.next_after.or(after);
        if page.finished && page.events.is_empty() {
            return Ok(false);
        }
    }
}

async fn wait_finished(
    daemon: &Daemon,
    run_id: &str,
    limit: Duration,
) -> Result<RunStatusView, Failure> {
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        let status: RunStatusView = daemon.get_json(&format!("/api/runs/{run_id}")).await?;
        if status.outcome.is_some() || tokio::time::Instant::now() >= deadline {
            return Ok(status);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Run `axocoatl loadouts ...`; returns the process exit code.
pub async fn cmd_loadouts(command: LoadoutCommands) -> i32 {
    let result = match command {
        LoadoutCommands::List { url, config } => list(url.as_deref(), &config).await,
        LoadoutCommands::Show { id, url, config } => show(&id, url.as_deref(), &config).await,
        LoadoutCommands::Validate { file } => validate(&file),
    };
    match result {
        Ok(code) => code,
        Err(failure) => {
            eprintln!("axocoatl loadouts: {}", failure.message);
            failure.code
        }
    }
}

async fn list(url: Option<&str>, config: &Path) -> Result<i32, Failure> {
    let daemon = Daemon::locate(url, config).await?;
    let loadouts: Vec<LoadoutSummary> = daemon.get_json("/api/loadouts").await?;
    for loadout in loadouts {
        match &loadout.error {
            Some(error) => println!(
                "{:<16} INVALID  {}: {error}",
                loadout.id,
                loadout.path.as_deref().unwrap_or("")
            ),
            None => println!(
                "{:<16} v{:<3} {:<7} {:<8}{}{} {}",
                loadout.id,
                loadout.version,
                loadout.kind,
                if loadout.builtin { "built-in" } else { "user" },
                if loadout.opt_in { " opt-in" } else { "" },
                if loadout.warnings.is_empty() {
                    String::new()
                } else {
                    format!(" ({} warnings)", loadout.warnings.len())
                },
                loadout.name
            ),
        }
    }
    Ok(0)
}

async fn show(id: &str, url: Option<&str>, config: &Path) -> Result<i32, Failure> {
    let daemon = Daemon::locate(url, config).await?;
    let view: LoadoutView = daemon.get_json(&format!("/api/loadouts/{id}")).await?;
    println!(
        "# {}@{} ({}, sha256:{})",
        view.summary.id, view.summary.version, view.summary.kind, view.summary.digest
    );
    for warning in &view.summary.warnings {
        println!("# warning {}: {}", warning.code, warning.message);
    }
    print!("{}", view.text);
    if !view.text.ends_with('\n') {
        println!();
    }
    println!("# graph");
    for node in &view.graph.nodes {
        println!("#   {} [{}] {}", node.id, node.kind, node.detail.join("; "));
    }
    for edge in &view.graph.edges {
        println!("#   {} -> {}", edge.from, edge.to);
    }
    println!(
        "# run: axocoatl run {} --task \"...\"{}",
        view.summary.id,
        view.summary
            .params
            .iter()
            .filter(|param| param.required)
            .map(|param| format!(" --param {}=...", param.name))
            .collect::<String>()
    );
    Ok(0)
}

fn validate(file: &Path) -> Result<i32, Failure> {
    let text = std::fs::read_to_string(file)
        .map_err(|error| Failure::usage(format!("{}: {error}", file.display())))?;
    let response = axocoatl_daemon::bootstrap::loadout_runs::validate_text(&text);
    for warning in &response.warnings {
        println!(
            "warning {} ({}): {}",
            warning.code, warning.field, warning.message
        );
    }
    match (&response.error, &response.summary) {
        (None, Some(summary)) => {
            println!(
                "valid: {}@{} ({}), sha256:{}",
                summary.id, summary.version, summary.kind, summary.digest
            );
            Ok(0)
        }
        (Some(error), _) => {
            println!("invalid: {error}");
            Ok(exit_code::USAGE)
        }
        (None, None) => Ok(exit_code::USAGE),
    }
}

/// Run `axocoatl record ...`; returns the process exit code.
pub async fn cmd_record(command: RecordCommands) -> i32 {
    match command {
        RecordCommands::Verify { file } => verify(&file),
    }
}

fn verify(file: &Path) -> i32 {
    let opened = match std::fs::File::open(file) {
        Ok(opened) => opened,
        Err(error) => {
            eprintln!("axocoatl record: {}: {error}", file.display());
            return exit_code::USAGE;
        }
    };
    match axocoatl_session::record_bundle::verify_bundle(std::io::BufReader::new(opened)) {
        Ok(summary) => {
            println!(
                "valid: run {} in Session {}, written by Axocoatl {}, {} section lines",
                summary.header.run_id,
                summary.header.session_id,
                summary.header.axocoatl_version,
                summary.lines
            );
            for (section, count) in &summary.sections {
                println!("  {section}: {count}");
            }
            0
        }
        Err(error) => {
            println!("invalid: {error}");
            exit_code::NEEDS_ATTENTION
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        run: RunArgs,
    }

    fn args(extra: &[&str]) -> RunArgs {
        let mut argv = vec!["axocoatl", "fix"];
        argv.extend_from_slice(extra);
        Cli::try_parse_from(argv).unwrap().run
    }

    #[test]
    fn model_flags_set_role_model_parameters() {
        let repo = tempfile::tempdir().unwrap();
        let repo_arg = repo.path().to_str().unwrap();
        let parsed = args(&[
            "--task",
            "fix the bug",
            "--repo",
            repo_arg,
            "--model",
            "writer=openrouter:qwen/qwen3-coder",
            "--model",
            "reviewer=ollama:qwen3:32b",
            "--param",
            "target_url=http://localhost:3000",
            "--keep",
            "branch",
        ]);
        let plan = plan(&parsed, None).unwrap();
        assert_eq!(plan.params["writer_model"], "openrouter:qwen/qwen3-coder");
        assert_eq!(plan.params["reviewer_model"], "ollama:qwen3:32b");
        assert_eq!(plan.params["target_url"], "http://localhost:3000");
        assert_eq!(plan.keep, Keep::Branch);
        assert_eq!(plan.repo, std::fs::canonicalize(repo.path()).unwrap());
    }

    #[test]
    fn bad_flags_are_usage_errors() {
        let repo = tempfile::tempdir().unwrap();
        let repo_arg = repo.path().to_str().unwrap();
        for extra in [
            vec!["--repo", repo_arg],
            vec!["--task", " ", "--repo", repo_arg],
            vec!["--task", "t", "--repo", repo_arg, "--keep", "merge"],
            vec!["--task", "t", "--repo", repo_arg, "--model", "writer"],
            vec!["--task", "t", "--repo", repo_arg, "--model", "writer=qwen"],
            vec!["--task", "t", "--repo", repo_arg, "--param", "novalue"],
            vec![
                "--task",
                "t",
                "--repo",
                repo_arg,
                "--model",
                "writer=a:b",
                "--param",
                "writer_model=c:d",
            ],
            vec!["--task", "t", "--repo", "/definitely/not/here"],
        ] {
            let parsed = args(&extra);
            let failure = plan(&parsed, None).unwrap_err();
            assert_eq!(
                failure.code,
                exit_code::USAGE,
                "{extra:?}: {}",
                failure.message
            );
        }
        assert!(
            Cli::try_parse_from(["axocoatl", "fix", "--task", "a", "--task-file", "b"]).is_err()
        );
    }

    #[tokio::test]
    async fn an_unreachable_daemon_exits_four() {
        // A port nothing listens on.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let repo = tempfile::tempdir().unwrap();
        // A config path of its own, so no person's local API token is read.
        let config = repo.path().join("axocoatl.yaml");
        let parsed = args(&[
            "--task",
            "t",
            "--repo",
            repo.path().to_str().unwrap(),
            "--url",
            &format!("http://127.0.0.1:{port}"),
            "-c",
            config.to_str().unwrap(),
        ]);
        assert_eq!(cmd_run(parsed).await, exit_code::DAEMON_UNAVAILABLE);
    }

    #[tokio::test]
    async fn a_refused_token_exits_four_and_an_unknown_loadout_three() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/runs"))
            .and(header("authorization", "Bearer good"))
            .respond_with(
                ResponseTemplate::new(404)
                    .set_body_json(serde_json::json!({"error": "no loadout named \"nope\""})),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/runs"))
            .respond_with(
                ResponseTemplate::new(401).set_body_json(serde_json::json!({"error": "no"})),
            )
            .mount(&server)
            .await;
        let repo = tempfile::tempdir().unwrap();
        let daemon = Daemon {
            base: server.uri(),
            token: Some("bad".into()),
            client: reqwest::Client::new(),
        };
        let failure = daemon
            .post_json::<serde_json::Value>("/api/runs", &serde_json::json!({}))
            .await
            .unwrap_err();
        assert_eq!(failure.code, exit_code::DAEMON_UNAVAILABLE);
        let daemon = Daemon {
            token: Some("good".into()),
            ..daemon
        };
        let failure = daemon
            .post_json::<serde_json::Value>("/api/runs", &serde_json::json!({}))
            .await
            .unwrap_err();
        assert_eq!(failure.code, exit_code::USAGE, "{}", failure.message);
        drop(repo);
    }

    #[test]
    fn files_are_written_atomically_and_bundles_verified() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("report.xml");
        write_atomically(&path, b"<x/>").unwrap();
        write_atomically(&path, b"<y/>").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"<y/>");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        let header = axocoatl_session::record_bundle::BundleHeader {
            schema: axocoatl_session::record_bundle::RECORD_BUNDLE_SCHEMA.into(),
            run_id: "run-1".into(),
            session_id: "ses-1".into(),
            created_at_ms: 1,
            axocoatl_version: "1.3.0".into(),
        };
        let mut writer =
            axocoatl_session::record_bundle::BundleWriter::new(Vec::new(), &header).unwrap();
        writer
            .section("manifest", &serde_json::json!({"a": 1}))
            .unwrap();
        let bytes = writer.finish().unwrap();
        let bundle = dir.path().join("run.axorecord.jsonl");
        std::fs::write(&bundle, &bytes).unwrap();
        assert_eq!(verify(&bundle), 0);
        let mut tampered = bytes.clone();
        let at = tampered.iter().position(|byte| *byte == b'1').unwrap();
        tampered[at] = b'2';
        std::fs::write(&bundle, &tampered).unwrap();
        assert_eq!(verify(&bundle), exit_code::NEEDS_ATTENTION);
        assert_eq!(verify(&dir.path().join("missing")), exit_code::USAGE);
    }

    #[test]
    fn validate_works_without_the_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("mine.yaml");
        std::fs::write(&file, "schema: axocoatl.loadout/1\nid: x\n").unwrap();
        assert_eq!(validate(&file).unwrap(), exit_code::USAGE);
        let fix = axocoatl_config::loadout::builtin_loadouts()
            .into_iter()
            .map(Result::unwrap)
            .find(|loadout| loadout.file.id == "fix")
            .unwrap();
        std::fs::write(&file, fix.text.replace("id: fix", "id: my-fix")).unwrap();
        assert_eq!(validate(&file).unwrap(), 0);
    }

    /// The qa smoke test printed "checkout: not_reached: not_reached: ran out
    /// of steps" and "(NotReached)": the summary and the progress line now
    /// share the Outcome's one rendering.
    #[test]
    fn not_covered_lines_name_their_class_once() {
        let outcome: RunOutcome = serde_json::from_value(serde_json::json!({
            "schema": "axocoatl.run-outcome/1",
            "run_id": "run-1",
            "session_id": "ses-1",
            "workspace_id": "wsp-1",
            "loadout": {"id": "qa", "version": 1, "kind": "qa", "digest": "0", "builtin": true},
            "task": "t",
            "started_at_ms": 1,
            "finished_at_ms": 2,
            "verdict": "needs_attention",
            "exit_code": 2,
            "not_covered": [
                {"area": "checkout", "class": "not_reached", "detail": "not_reached: ran out of steps"},
                {"area": "writer", "class": "other", "detail": ""}
            ]
        }))
        .unwrap();
        let text = summary(&outcome);
        assert!(
            text.contains(
                "Not covered:\n  checkout: not_reached: ran out of steps\n  writer: other\n"
            ),
            "{text}"
        );
        let event = RunEvent::NotCovered {
            at_ms: 1,
            entry: Box::new(outcome.not_covered[0].clone()),
        };
        assert_eq!(
            progress_line(&event).as_deref(),
            Some("! not covered: checkout: not_reached: ran out of steps")
        );
    }

    /// Bytes written to a `Console` stream, kept for the assertions.
    #[derive(Clone, Default)]
    struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl Write for Captured {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Captured {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    fn captured_console() -> (Console, Captured, Captured) {
        let out = Captured::default();
        let err = Captured::default();
        let console = Console {
            out: Box::new(out.clone()),
            err: Box::new(err.clone()),
        };
        (console, out, err)
    }

    const RUN: &str = "run-0c1d0000-0000-4000-8000-000000000001";
    const SESSION: &str = "ses-0c1d0000-0000-4000-8000-000000000002";

    fn same_model() -> RunWarning {
        RunWarning {
            code: "same_model_reviewer".into(),
            message: "the reviewer runs the same model as the writer".into(),
        }
    }

    fn outcome(verdict: &str, extra: serde_json::Value) -> RunOutcome {
        let mut value = serde_json::json!({
            "schema": "axocoatl.run-outcome/1",
            "run_id": RUN,
            "session_id": SESSION,
            "workspace_id": "wsp-1",
            "loadout": {"id": "fix", "version": 1, "kind": "fix",
                "digest": "0".repeat(64), "builtin": true},
            "task": "fix the pagination bug",
            "started_at_ms": 1,
            "finished_at_ms": 2,
            "verdict": verdict,
            "exit_code": match verdict {
                "pass" => exit_code::PASS,
                "checks_failed" => exit_code::CHECKS_FAILED,
                _ => exit_code::NEEDS_ATTENTION,
            },
            "warnings": [same_model()],
        });
        for (key, field) in extra.as_object().unwrap() {
            value[key] = field.clone();
        }
        serde_json::from_value(value).unwrap()
    }

    /// A record bundle as the daemon writes it: the Outcome, then one
    /// `run_event` section per recorded event.
    fn bundle(outcome: &RunOutcome, events: &[RunEvent]) -> Vec<u8> {
        use axocoatl_session::record_bundle::{BundleHeader, BundleWriter, RECORD_BUNDLE_SCHEMA};
        let header = BundleHeader {
            schema: RECORD_BUNDLE_SCHEMA.into(),
            run_id: RUN.into(),
            session_id: SESSION.into(),
            created_at_ms: 3,
            axocoatl_version: "1.3.0".into(),
        };
        let mut writer = BundleWriter::new(Vec::new(), &header).unwrap();
        writer
            .section("manifest", &serde_json::json!({"run_id": RUN}))
            .unwrap();
        writer
            .section("outcome", &serde_json::to_value(outcome).unwrap())
            .unwrap();
        for (index, event) in events.iter().enumerate() {
            writer
                .section(
                    "run_event",
                    &serde_json::json!({"seq": index + 1, "event": event}),
                )
                .unwrap();
        }
        writer.finish().unwrap()
    }

    /// `GET /api/runs/{id}/record` as the daemon serves it: Keep appends a
    /// `keep` event to the run record, so the bundle holds it once Keep ran.
    struct RecordResponder {
        kept: std::sync::Arc<std::sync::atomic::AtomicBool>,
        before_keep: Vec<u8>,
        after_keep: Vec<u8>,
    }

    impl wiremock::Respond for RecordResponder {
        fn respond(&self, _: &wiremock::Request) -> wiremock::ResponseTemplate {
            let kept = self.kept.load(std::sync::atomic::Ordering::SeqCst);
            wiremock::ResponseTemplate::new(200).set_body_raw(
                if kept {
                    self.after_keep.clone()
                } else {
                    self.before_keep.clone()
                },
                axocoatl_session::record_bundle::RECORD_BUNDLE_MEDIA_TYPE,
            )
        }
    }

    /// `POST /api/sessions/{id}/keep-pr`: records the keep event (also when
    /// it fails, as the daemon does) and answers `status`.
    struct KeepResponder {
        kept: std::sync::Arc<std::sync::atomic::AtomicBool>,
        status: u16,
    }

    impl wiremock::Respond for KeepResponder {
        fn respond(&self, _: &wiremock::Request) -> wiremock::ResponseTemplate {
            self.kept.store(true, std::sync::atomic::Ordering::SeqCst);
            if self.status == 200 {
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "branch": "axocoatl/fix-0c1d0000",
                    "commit": "4b825dc642cb6eb9a060e54bf8d69288fbee4904",
                    "paths": ["src/paginate.js"],
                    "warnings": ["1 changed path no Agent of the run changed was not committed"],
                }))
            } else {
                wiremock::ResponseTemplate::new(self.status)
                    .set_body_json(serde_json::json!({"error": "the run path src/paginate.js changed after the run ended"}))
            }
        }
    }

    struct FakeRun {
        server: wiremock::MockServer,
        /// The bundle `GET /api/runs/{id}/record` serves after Keep.
        after_keep: Vec<u8>,
    }

    /// The daemon's run API for one finished run: admission returns
    /// `same_model()` and also records it as the first run event, as
    /// `admit_loadout_run` does.
    async fn fake_daemon(
        outcome: RunOutcome,
        mut events: Vec<RunEvent>,
        keep_status: u16,
    ) -> FakeRun {
        use wiremock::matchers::{method, path, query_param, query_param_is_missing};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        events.insert(
            0,
            RunEvent::Warning {
                at_ms: 1,
                warning: same_model(),
            },
        );
        let keep_event = RunEvent::Phase {
            at_ms: 4,
            phase: "keep".into(),
            detail: r#"{"branch":"axocoatl/fix-0c1d0000","commit":"4b825dc642cb6eb9a060e54bf8d69288fbee4904"}"#.into(),
        };
        let before_keep = bundle(&outcome, &events);
        let after_keep = bundle(&outcome, &[events.clone(), vec![keep_event]].concat());
        Mock::given(method("POST"))
            .and(path("/api/runs"))
            .respond_with(ResponseTemplate::new(202).set_body_json(RunAccepted {
                run_id: RUN.into(),
                session_id: SESSION.into(),
                workspace_id: "wsp-1".into(),
                warnings: vec![same_model()],
            }))
            .mount(&server)
            .await;
        let events_path = format!("/api/runs/{RUN}/events");
        let count = events.len() as u64;
        Mock::given(method("GET"))
            .and(path(events_path.clone()))
            .and(query_param_is_missing("after"))
            .respond_with(ResponseTemplate::new(200).set_body_json(RunEventsPage {
                events: (1..).zip(events).collect(),
                next_after: Some(count),
                finished: true,
            }))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(events_path))
            .and(query_param("after", count.to_string()))
            .respond_with(ResponseTemplate::new(200).set_body_json(RunEventsPage {
                events: Vec::new(),
                next_after: Some(count),
                finished: true,
            }))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/api/runs/{RUN}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(RunStatusView {
                run_id: RUN.into(),
                session_id: SESSION.into(),
                loadout: "fix".into(),
                state: "finished".into(),
                phase: "finishing".into(),
                started_at_ms: 1,
                outcome: Some(outcome),
                keep: None,
            }))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/api/runs/{RUN}/junit")))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw("<testsuites/>\n", "application/xml"),
            )
            .mount(&server)
            .await;
        let kept = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        Mock::given(method("GET"))
            .and(path(format!("/api/runs/{RUN}/record")))
            .respond_with(RecordResponder {
                kept: kept.clone(),
                before_keep,
                after_keep: after_keep.clone(),
            })
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(format!("/api/sessions/{SESSION}/keep-pr")))
            .respond_with(KeepResponder {
                kept,
                status: keep_status,
            })
            .mount(&server)
            .await;
        FakeRun { server, after_keep }
    }

    /// `axocoatl run fix` against `server` with its own config path (so no
    /// person's local API token is read) and a repository in `dir`.
    fn run_args(server: &str, dir: &Path, extra: &[&str]) -> RunArgs {
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let config = dir.join("axocoatl.yaml");
        let mut argv = vec![
            "--task",
            "fix the pagination bug",
            "--repo",
            repo.to_str().unwrap(),
            "--url",
            server,
            "-c",
            config.to_str().unwrap(),
        ];
        argv.extend_from_slice(extra);
        args(&argv)
    }

    fn paths_requested(requests: &[wiremock::Request]) -> Vec<String> {
        requests
            .iter()
            .map(|request| format!("{} {}", request.method, request.url.path()))
            .collect()
    }

    #[tokio::test]
    async fn a_kept_run_writes_the_bundle_the_api_serves_after_keep_and_each_warning_once() {
        let run = fake_daemon(outcome("pass", serde_json::json!({})), Vec::new(), 200).await;
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("run.axorecord.jsonl");
        let junit = dir.path().join("junit.xml");
        let (mut console, out, err) = captured_console();
        let code = run_on(
            run_args(
                &run.server.uri(),
                dir.path(),
                &[
                    "--keep",
                    "branch",
                    "--record",
                    record.to_str().unwrap(),
                    "--junit",
                    junit.to_str().unwrap(),
                ],
            ),
            &mut console,
            ADMISSION_NOTE_AFTER,
        )
        .await;
        let (out, err) = (out.text(), err.text());
        assert_eq!(code, exit_code::PASS, "{out}\n{err}");
        // The file is the bundle the API serves once the command ended,
        // with the keep event.
        assert_eq!(std::fs::read(&record).unwrap(), run.after_keep);
        assert!(String::from_utf8_lossy(&run.after_keep).contains(r#""phase":"keep""#));
        assert_eq!(verify(&record), 0);
        assert_eq!(std::fs::read_to_string(&junit).unwrap(), "<testsuites/>\n");
        let requests = paths_requested(&run.server.received_requests().await.unwrap());
        let at = |wanted: &str| {
            requests
                .iter()
                .position(|request| request == wanted)
                .unwrap_or_else(|| panic!("no {wanted} in {requests:?}"))
        };
        let keep = at(&format!("POST /api/sessions/{SESSION}/keep-pr"));
        assert!(
            keep < at(&format!("GET /api/runs/{RUN}/record")),
            "{requests:?}"
        );
        assert!(
            keep < at(&format!("GET /api/runs/{RUN}/junit")),
            "{requests:?}"
        );
        // Admission returned the warning and recorded it as an event: it is
        // printed once.
        assert_eq!(
            err.matches("! warning same_model_reviewer: ").count(),
            1,
            "{err}"
        );
        assert!(err.contains("· starting loadout fix in "), "{err}");
        assert!(
            err.contains(&format!("· run {RUN} in Session {SESSION}")),
            "{err}"
        );
        assert!(err.contains("! keep: 1 changed path"), "{err}");
        assert!(out.starts_with("Verdict: pass (exit 0)\n"), "{out}");
        assert!(
            out.contains("  same_model_reviewer: the reviewer runs"),
            "{out}"
        );
        assert!(
            out.ends_with(
                "Kept: branch axocoatl/fix-0c1d0000 at 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n"
            ),
            "{out}"
        );
    }

    #[tokio::test]
    async fn with_json_standard_output_is_one_outcome_document_also_when_kept() {
        let run = fake_daemon(outcome("pass", serde_json::json!({})), Vec::new(), 200).await;
        let dir = tempfile::tempdir().unwrap();
        let (mut console, out, err) = captured_console();
        let code = run_on(
            run_args(&run.server.uri(), dir.path(), &["--keep", "pr", "--json"]),
            &mut console,
            ADMISSION_NOTE_AFTER,
        )
        .await;
        let (out, err) = (out.text(), err.text());
        assert_eq!(code, exit_code::PASS, "{out}\n{err}");
        let printed: RunOutcome = serde_json::from_str(&out).unwrap();
        assert_eq!(printed.run_id, RUN);
        assert!(
            err.contains("Kept: branch axocoatl/fix-0c1d0000 at "),
            "{err}"
        );
        let requests = run.server.received_requests().await.unwrap();
        let keep = requests
            .iter()
            .find(|request| request.url.path().ends_with("/keep-pr"))
            .unwrap();
        let body: serde_json::Value = keep.body_json().unwrap();
        assert_eq!(body, serde_json::json!({"run_id": RUN, "open_pr": true}));
    }

    #[tokio::test]
    async fn a_failed_keep_exits_two_after_writing_the_bundle_with_its_event() {
        let run = fake_daemon(outcome("pass", serde_json::json!({})), Vec::new(), 409).await;
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("run.axorecord.jsonl");
        let (mut console, out, err) = captured_console();
        let code = run_on(
            run_args(
                &run.server.uri(),
                dir.path(),
                &["--keep", "branch", "--record", record.to_str().unwrap()],
            ),
            &mut console,
            ADMISSION_NOTE_AFTER,
        )
        .await;
        let (out, err) = (out.text(), err.text());
        assert_eq!(code, exit_code::NEEDS_ATTENTION, "{out}\n{err}");
        assert!(
            out.ends_with(
                "Keep failed: 409 Conflict: the run path src/paginate.js changed after the run ended\n"
            ),
            "{out}"
        );
        assert_eq!(std::fs::read(&record).unwrap(), run.after_keep);
    }

    #[tokio::test]
    async fn a_run_that_needs_attention_is_not_kept_and_prints_classes_as_documented() {
        use axocoatl_session::run_outcome::{FailureClass, NotCovered, TurnState};
        let not_covered = NotCovered {
            area: "checkout".into(),
            class: FailureClass::NotReached,
            detail: "ran out of steps".into(),
            node_id: None,
            turn_id: None,
        };
        let events = vec![
            RunEvent::TurnStarted {
                at_ms: 2,
                turn_id: "turn-1".into(),
                purpose: "run".into(),
            },
            RunEvent::TurnEnded {
                at_ms: 3,
                turn_id: "turn-1".into(),
                state: TurnState::NeedsAttention,
            },
            RunEvent::NotCovered {
                at_ms: 3,
                entry: Box::new(not_covered.clone()),
            },
        ];
        let run = fake_daemon(
            outcome(
                "needs_attention",
                serde_json::json!({"not_covered": [not_covered]}),
            ),
            events,
            200,
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        let (mut console, out, err) = captured_console();
        let code = run_on(
            run_args(&run.server.uri(), dir.path(), &["--keep", "branch"]),
            &mut console,
            ADMISSION_NOTE_AFTER,
        )
        .await;
        let (out, err) = (out.text(), err.text());
        assert_eq!(code, exit_code::NEEDS_ATTENTION, "{out}\n{err}");
        assert!(
            err.contains("· turn turn-1 ended: needs_attention\n"),
            "{err}"
        );
        assert!(
            err.contains("! not covered: checkout: not_reached: ran out of steps\n"),
            "{err}"
        );
        assert!(
            !err.contains("NotReached") && !err.contains("NeedsAttention"),
            "{err}"
        );
        assert!(
            err.contains("· nothing kept: the run did not pass"),
            "{err}"
        );
        assert!(
            out.contains("  checkout: not_reached: ran out of steps\n"),
            "{out}"
        );
        let requests = run.server.received_requests().await.unwrap();
        assert!(
            !requests
                .iter()
                .any(|request| request.url.path().ends_with("/keep-pr")),
            "{:?}",
            paths_requested(&requests)
        );
    }

    #[tokio::test]
    async fn a_refused_run_never_says_it_started() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/runs"))
            .respond_with(
                ResponseTemplate::new(422)
                    .set_body_json(serde_json::json!({"error": "integrator_model: required"}))
                    .set_delay(Duration::from_millis(300)),
            )
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let (mut console, out, err) = captured_console();
        // A slow refusal: the command says it is waiting, never that the run
        // started.
        let code = run_on(
            run_args(&server.uri(), dir.path(), &[]),
            &mut console,
            Duration::from_millis(20),
        )
        .await;
        let (out, err) = (out.text(), err.text());
        assert_eq!(code, exit_code::USAGE, "{err}");
        assert_eq!(out, "");
        assert_eq!(
            err,
            "· waiting for the daemon to admit the run (it prepares the Session's environment first)\n\
             axocoatl run: integrator_model: required\n"
        );
        // A quick one prints only its reason.
        let (mut console, _, err) = captured_console();
        let code = run_on(
            run_args(&server.uri(), dir.path(), &[]),
            &mut console,
            ADMISSION_NOTE_AFTER,
        )
        .await;
        assert_eq!(code, exit_code::USAGE);
        assert_eq!(err.text(), "axocoatl run: integrator_model: required\n");
    }

    /// A Codex writer reports tokens but no cost; what its calls reserved
    /// is not shown as the run's cost.
    #[test]
    fn summary_says_when_the_cost_is_not_known() {
        let unknown = outcome(
            "pass",
            serde_json::json!({"usage": {"input_tokens": 600, "output_tokens": 18,
                "cost_microunits": 333_333, "complete": true, "cost_known": false}}),
        );
        let text = summary(&unknown);
        assert!(
            text.contains(
                "\nUsage: 600 input + 18 output tokens, cost unknown (reserved up to $0.3333)\n"
            ),
            "{text}"
        );
        let known = outcome(
            "pass",
            serde_json::json!({"usage": {"input_tokens": 240, "output_tokens": 14,
                "cost_microunits": 310, "complete": true, "cost_known": true, "retries": 1}}),
        );
        assert!(summary(&known)
            .contains("\nUsage: 240 input + 14 output tokens, $0.0003, 1 provider retries\n"));
    }

    #[test]
    fn summary_findings_show_severity_and_location() {
        let outcome = outcome(
            "pass",
            serde_json::json!({"findings": [
                {"id": "auth-F1", "source": "audit_worker", "title": "Timing-unsafe HMAC compare",
                 "severity": "high", "area": "auth", "location": "auth/hmac.go:31"},
                {"id": "notify-F1", "source": "audit_worker", "title": "Hard-coded token",
                 "severity": "critical"},
                {"id": "ingest-F1", "source": "audit_worker", "title": "Unchecked unmarshal",
                 "area": "ingest", "location": "ingest/read.go:12"},
                {"id": "B4", "source": "explorer", "title": "Coupon fails", "area": "cart",
                 "repro": {"path": "axocoatl-qa/b4.spec.ts", "classification": "fails_on_clean_build"}},
            ]}),
        );
        let text = summary(&outcome);
        for line in [
            "  auth-F1 Timing-unsafe HMAC compare [auth] (high severity, auth/hmac.go:31): audit_worker\n",
            "  notify-F1 Hard-coded token (critical severity): audit_worker\n",
            "  ingest-F1 Unchecked unmarshal [ingest] (ingest/read.go:12): audit_worker\n",
            "  B4 Coupon fails [cart]: fails on clean build\n",
        ] {
            assert!(text.contains(line), "{line:?} in\n{text}");
        }
    }
}
