//! `axocoatl run`, `axocoatl loadouts`, `axocoatl record`. They talk to the
//! running daemon over its HTTP API with the local API token (or
//! `AXOCOATL_URL` / `AXOCOATL_TOKEN`); `axocoatl record verify` reads only
//! the file. Owner: workstream `core`.
//! Contract: docs/design/1.3-loadouts.md ("axocoatl run"), and for
//! `--receipt` docs/design/1.4-receipts.md.
//!
//! `axocoatl run` never starts a daemon. Progress goes to stderr, the
//! summary (or the Outcome as JSON) to stdout, and the process exits with
//! the Outcome's exit code: 0 pass, 1 checks failed, 2 needs attention, 3
//! usage, 4 daemon unreachable or token refused, 5 infrastructure, 6
//! interrupted, 7 busy (another run or Session holds the Workspace).

use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use axocoatl_daemon::loadout::api::{
    LoadoutSummary, LoadoutView, RunAccepted, RunEventsPage, RunPreflight, RunStatusView,
};
use axocoatl_session::run_outcome::{
    exit_code, AdjudicationDecision, CheckState, ReproClassification, RunOutcome, RunWarning,
};
use axocoatl_session::run_record::RunEvent;
use clap::{Args, Subcommand};

#[path = "run_toolchain.rs"]
mod toolchain;

/// `axocoatl run <loadout> --task "..."`.
#[derive(Debug, Args)]
pub struct RunArgs {
    /// Loadout id: `fix`, `fix-claude-code`, `fix-codex`, `qa`, `audit`
    /// (opt-in), or one of yours (`axocoatl loadouts list`)
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
    /// Write the run's signed receipt (a DSSE envelope) here
    #[arg(long)]
    pub receipt: Option<PathBuf>,
    /// A model parameter as ROLE=provider:model (sets the `ROLE_model`
    /// parameter). Every built-in needs a reviewer, a different model on
    /// ollama: or openrouter: (--model reviewer=ollama:gpt-oss:20b); `fix`
    /// also needs --model writer=…. run.models.writer and run.models.reviewer
    /// in the configuration (`axocoatl onboard` writes them) are used when
    /// the run names none
    #[arg(long = "model", value_name = "ROLE=PROVIDER:MODEL")]
    pub models: Vec<String>,
    /// A loadout parameter as NAME=VALUE
    #[arg(long = "param", value_name = "NAME=VALUE")]
    pub params: Vec<String>,
    /// The command a `detected` check runs, in place of the repository's
    /// check command (a `Check command:` line of AXOCOATL.md, else detected
    /// from its files; needed when neither gives one)
    #[arg(long)]
    pub check: Option<String>,
    /// The exact setup command this run approves. With a toolchain it
    /// replaces the detected dependency install and may reach that
    /// toolchain's package registries while it runs
    #[arg(long, conflicts_with = "no_setup")]
    pub setup: Option<String>,
    /// The repository's language toolchain: detected (from its files), node,
    /// python, rust, go, or none (the image as the loadout names it). The
    /// default is detected for a loadout that runs the repository's check
    #[arg(long, value_name = "TOOLCHAIN")]
    pub toolchain: Option<String>,
    /// Run this image as it is, without a toolchain (a curated or trusted
    /// image; give its installs with --setup)
    #[arg(long, value_name = "REF")]
    pub image: Option<String>,
    /// Run no setup, not even the toolchain's detected dependency install
    #[arg(long)]
    pub no_setup: bool,
    /// Do not build a missing toolchain image: refuse the run, naming the
    /// `axocoatl recipe build` command
    #[arg(long)]
    pub no_build: bool,
    /// Do not probe the check before the first model call (the probe
    /// refuses a run whose check cannot run in its image; use this when it
    /// is wrong about yours)
    #[arg(long)]
    pub no_probe: bool,
    /// What to do with a passing run's changes: none, branch or pr
    #[arg(long, default_value = "none")]
    pub keep: String,
    /// Print the Outcome as JSON on stdout instead of the summary
    #[arg(long)]
    pub json: bool,
    /// Only check the run as admission would and list every problem: no run,
    /// Workspace or Session is created, Podman is asked but a stopped
    /// machine is not started, and each Ollama model is loaded once without
    /// a prompt and probed, as a run's admission does. Exits 0 when the run
    /// would start, else 3 or 5 (4: the daemon cannot be reached or is too
    /// old). `--task` is optional here
    #[arg(
        long,
        visible_alias = "preflight",
        conflicts_with_all = ["junit", "record", "receipt"]
    )]
    pub dry_run: bool,
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
    /// `--toolchain`, `--image`, `--no-setup`, `--no-build` and
    /// `--no-probe`.
    pub environment: axocoatl_daemon::loadout::toolchain::EnvironmentRequest,
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
        // A dry run without a task checks everything else, and sends none.
        (None, None) if args.dry_run => String::new(),
        (None, None) => {
            return Err(Failure::usage(
                "say what to do: --task \"...\" or --task-file <file> (- reads the task from \
                 standard input)",
            ))
        }
        (Some(_), Some(_)) => return Err(Failure::usage("give --task or --task-file, not both")),
    };
    let dry_run_without_task = args.dry_run && args.task.is_none() && args.task_file.is_none();
    if task.trim().is_empty() && !dry_run_without_task {
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
        environment: toolchain::request(
            args.toolchain.as_deref(),
            args.image.as_deref(),
            args.no_setup,
            args.no_build,
            args.no_probe,
        ),
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

    /// A daemon at `base` with `token` (tests).
    #[cfg(test)]
    pub(crate) fn for_test(base: &str, token: Option<&str>) -> Self {
        Self {
            base: base.trim_end_matches('/').to_string(),
            token: token.map(str::to_string),
            client: reqwest::Client::new(),
        }
    }

    /// The daemon's URL, without a trailing `/`.
    pub(crate) fn base(&self) -> &str {
        &self.base
    }

    pub(crate) fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let builder = self.client.request(method, format!("{}{path}", self.base));
        match &self.token {
            Some(token) => builder.bearer_auth(token),
            None => builder,
        }
    }

    async fn send(&self, builder: reqwest::RequestBuilder) -> Result<reqwest::Response, Failure> {
        let response = self.transport(builder).await?;
        self.checked(response).await
    }

    /// Send `builder`; only a daemon that cannot be reached is a failure.
    async fn transport(
        &self,
        builder: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, Failure> {
        builder.send().await.map_err(|error| {
            Failure::unreachable(format!(
                "the daemon at {} cannot be reached: {error}. Start it with `axocoatl dev` (or \
                 `axocoatl service start` for the service)",
                self.base
            ))
        })
    }

    /// `response` when it succeeded, else the failure its status and body
    /// say.
    async fn checked(&self, response: reqwest::Response) -> Result<reqwest::Response, Failure> {
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let text = response.text().await.unwrap_or_default();
        let body = serde_json::from_str::<serde_json::Value>(&text).ok();
        let code = body
            .as_ref()
            .and_then(|value| value.get("code"))
            .and_then(|code| code.as_str());
        let message = body
            .as_ref()
            .and_then(|value| {
                value
                    .get("error")
                    .and_then(|e| e.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| text.clone());
        Err(match status.as_u16() {
            401 | 403 => Failure::unreachable(format!(
                "the daemon refused the API token ({status}): {message}"
            )),
            404 | 422 => Failure::usage(message),
            409 if code == Some(axocoatl_daemon::WORKSPACE_BUSY_CODE) => Failure {
                code: exit_code::BUSY,
                message,
            },
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
    if !outcome.unreadable_findings.is_empty() {
        let _ = writeln!(out, "Findings unreadable:");
        for entry in &outcome.unreadable_findings {
            let _ = writeln!(out, "  {}: {}", entry.area, entry.detail);
        }
    }
    if !outcome.notes.is_empty() {
        let _ = writeln!(out, "Notes:");
        for note in &outcome.notes {
            let _ = writeln!(out, "  {note}");
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
    if args.dry_run {
        return match dry_run(&args, console, note_after).await {
            Ok(code) => code,
            Err(failure) => {
                console.note(&format!("axocoatl run: {}", failure.message));
                failure.code
            }
        };
    }
    let mut admitted = false;
    match run(&args, console, note_after, &mut admitted).await {
        Ok(code) => code,
        Err(failure) => {
            console.note(&format!("axocoatl run: {}", failure.message));
            if !admitted {
                write_unadmitted_files(&args, &failure, console);
            }
            failure.code
        }
    }
}

/// No run was admitted (the daemon refused it, could not be reached, or the
/// flags were refused): `--junit` still gets a file whose verdict is the
/// refusal, an `<error>` of type `busy`, `usage` or `error` with the exit
/// code, so a CI job that reads only JUnit sees it. `--record` has no run to
/// record, so nothing is written there.
fn write_unadmitted_files(args: &RunArgs, failure: &Failure, console: &mut Console) {
    if let Some(path) = &args.junit {
        let xml = axocoatl_session::run_junit::render_refused_run_junit(
            &args.loadout,
            failure.code,
            &failure.message,
        );
        match write_atomically(path, xml.as_bytes()) {
            Ok(()) => console.note(&format!("· JUnit written to {}", path.display())),
            Err(error) => console.note(&format!(
                "axocoatl run: JUnit: writing {}: {error}",
                path.display()
            )),
        }
    }
    if let Some(path) = &args.record {
        console.note(&format!(
            "· no record written to {}: no run was admitted, so there is nothing to record",
            path.display()
        ));
    }
    if let Some(path) = &args.receipt {
        console.note(&format!(
            "· no receipt written to {}: no run was admitted, so there is nothing to sign",
            path.display()
        ));
    }
}

/// Write the run's receipt to `path` atomically and say what it covers. A
/// run without one (it could not be signed; its Outcome says why) is said,
/// not a failure: a receipt never changes a run's exit code.
async fn write_receipt(
    daemon: &Daemon,
    run_id: &str,
    path: &Path,
    console: &mut Console,
) -> Result<(), Failure> {
    let response = daemon
        .send(daemon.request(reqwest::Method::GET, &format!("/api/runs/{run_id}/receipt")))
        .await;
    let response = match response {
        Ok(response) => response,
        Err(failure) if failure.code == exit_code::USAGE => {
            console.note(&format!(
                "· no receipt written to {}: {}",
                path.display(),
                failure.message
            ));
            return Ok(());
        }
        Err(failure) => return Err(failure),
    };
    let bytes = response
        .bytes()
        .await
        .map_err(|error| Failure::infrastructure(format!("reading the receipt: {error}")))?;
    write_atomically(path, &bytes)
        .map_err(|error| Failure::infrastructure(format!("writing {}: {error}", path.display())))?;
    let covered = axocoatl_session::receipt::open_receipt(&bytes)
        .map(|opened| {
            let files = &opened.statement.predicate.files;
            let covered = opened.statement.subject.len();
            format!(
                " (key {}, {covered} file{} covered, {} removed, {}{})",
                axocoatl_session::receipt::short_key_id(&opened.statement.predicate.signer.key_id),
                if covered == 1 { "" } else { "s" },
                files.removed.len(),
                files.source,
                if files.complete {
                    ""
                } else {
                    "; it does not cover every path the run changed"
                }
            )
        })
        .unwrap_or_default();
    console.note(&format!("· receipt written to {}{covered}", path.display()));
    Ok(())
}

async fn run(
    args: &RunArgs,
    console: &mut Console,
    note_after: Duration,
    admitted: &mut bool,
) -> Result<i32, Failure> {
    let task_from_file = match &args.task_file {
        Some(path) => Some(read_task_file(path)?),
        None => None,
    };
    let plan = plan(args, task_from_file)?;
    let daemon = Daemon::locate(args.url.as_deref(), &args.config).await?;
    let mut request = run_request(&plan)?;
    request["task"] = serde_json::Value::String(plan.task.clone());
    request["request_id"] = serde_json::Value::String(format!("cli-{}", uuid::Uuid::new_v4()));
    // A missing toolchain image is built here, its progress on stderr, once
    // the daemon found nothing else that stops the run.
    toolchain::prepare_image(&daemon, &plan, &request, &args.config, console).await?;
    let accepted = admit(&daemon, &request, console, note_after, ctrl_c_pressed()).await?;
    *admitted = true;
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
    if let Some(path) = &args.receipt {
        if let Err(failure) = write_receipt(&daemon, &accepted.run_id, path, console).await {
            console.note(&format!("axocoatl run: receipt: {}", failure.message));
            file_failure = Some(failure);
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
/// without saying that the run started. `interrupt` (Ctrl-C) ends the wait
/// at once as an interrupted run: the request goes, and the daemon, which
/// finishes the admission on its own, stops a run it admits that way
/// before its first turn.
async fn admit(
    daemon: &Daemon,
    request: &serde_json::Value,
    console: &mut Console,
    note_after: Duration,
    interrupt: impl std::future::Future<Output = ()>,
) -> Result<RunAccepted, Failure> {
    let admission = daemon.post_json::<RunAccepted>("/api/runs", request);
    tokio::pin!(admission);
    tokio::pin!(interrupt);
    tokio::select! {
        accepted = &mut admission => return accepted.map_err(plain_refusal),
        () = &mut interrupt => return Err(interrupted_admission()),
        () = tokio::time::sleep(note_after) => {}
    }
    console.note(ADMISSION_NOTE);
    tokio::select! {
        accepted = admission => accepted.map_err(plain_refusal),
        () = interrupt => Err(interrupted_admission()),
    }
}

/// Ctrl-C, once it is pressed. Never, when its handler cannot be installed:
/// Ctrl-C then ends the process as it would without one.
async fn ctrl_c_pressed() {
    if tokio::signal::ctrl_c().await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// Ctrl-C while the daemon was still admitting the run (exit code 6).
fn interrupted_admission() -> Failure {
    Failure {
        code: exit_code::INTERRUPTED,
        message: "interrupted (Ctrl-C) while the daemon was admitting the run. The daemon \
                  finishes the admission on its own: a run it admits stops before its first \
                  turn, as interrupted, and its Session's containers stop with it; `GET \
                  /api/runs` lists it"
            .into(),
    }
}

/// An admission refusal in the daemon's own words: an infrastructure
/// refusal (exit code 5), such as Podman that is not ready, without the
/// HTTP status and error class in front of it.
fn plain_refusal(mut failure: Failure) -> Failure {
    if failure.code == exit_code::INFRASTRUCTURE {
        if let Some(rest) = failure.message.strip_prefix("400 Bad Request: ") {
            failure.message = rest
                .strip_prefix("Session error: ")
                .unwrap_or(rest)
                .to_string();
        }
    }
    failure
}

/// What `axocoatl run` says while admission takes longer than
/// [`ADMISSION_NOTE_AFTER`]: it may load Ollama models and start a stopped
/// Podman machine before it prepares the Session's environment.
const ADMISSION_NOTE: &str = "· checking the run (each Ollama model is loaded once and probed, \
                              then Podman is asked, and a stopped Podman machine is started when \
                              nothing else stops the run), then preparing the Session's \
                              environment";

/// What `axocoatl run --dry-run` says while its check takes longer than
/// [`ADMISSION_NOTE_AFTER`].
const DRY_RUN_NOTE: &str = "· checking the run as admission would: each Ollama model is loaded \
                            once without a prompt and probed, and Podman is asked (a stopped \
                            machine stays stopped)";

/// The body of `POST /api/runs` for `plan`, without its task and request
/// id; the dry run's too.
fn run_request(plan: &RunPlan) -> Result<serde_json::Value, Failure> {
    let mut request = serde_json::json!({
        "loadout": plan.loadout,
        "repo": plan.repo.display().to_string(),
        "params": plan.params,
        "keep": match plan.keep { Keep::None => "none", Keep::Branch => "branch", Keep::Pr => "pr" },
        "check_command": plan.check_command,
        "setup_command": plan.setup_command,
    });
    if !plan.environment.is_default() {
        request["environment"] = serde_json::to_value(&plan.environment)
            .map_err(|error| Failure::infrastructure(error.to_string()))?;
    }
    Ok(request)
}

/// The dry run's route. It is not a flag on `POST /api/runs`: a daemon
/// too old to know it answers 404 or 405, never starts a run.
const PREFLIGHT_PATH: &str = "/api/runs/preflight";

/// `POST /api/runs/preflight` with `request`: the [`RunPreflight`], or
/// why there is none. A daemon older than this command has no such route
/// (404 or 405): it is told to restart, and nothing was started. An answer
/// that is not a [`RunPreflight`] is never taken for one; if it names a
/// run, that run is named.
async fn ask_preflight(
    daemon: &Daemon,
    request: &serde_json::Value,
) -> Result<RunPreflight, Failure> {
    match try_preflight(daemon, request).await? {
        Preflight::Checked(preflight) => Ok(preflight),
        Preflight::Missing(status) => Err(Failure::unreachable(format!(
            "the daemon at {} has no dry run (POST {PREFLIGHT_PATH} answered {status}), so it is \
             older than this axocoatl {}; nothing was started. Restart it with this version \
             (`axocoatl dev`, or `axocoatl service stop` and `axocoatl service start`), then run \
             again",
            daemon.base(),
            env!("CARGO_PKG_VERSION")
        ))),
    }
}

/// What `POST /api/runs/preflight` answered.
enum Preflight {
    Checked(RunPreflight),
    /// A daemon older than the dry run: its 404 or 405.
    Missing(reqwest::StatusCode),
}

/// [`ask_preflight`], saying when the daemon has no dry run.
async fn try_preflight(daemon: &Daemon, request: &serde_json::Value) -> Result<Preflight, Failure> {
    let response = daemon
        .transport(
            daemon
                .request(reqwest::Method::POST, PREFLIGHT_PATH)
                .json(request),
        )
        .await?;
    let status = response.status();
    if matches!(status.as_u16(), 404 | 405) {
        return Ok(Preflight::Missing(status));
    }
    let text = daemon
        .checked(response)
        .await?
        .text()
        .await
        .map_err(|error| Failure::infrastructure(format!("reading {PREFLIGHT_PATH}: {error}")))?;
    serde_json::from_str::<RunPreflight>(&text)
        .map(Preflight::Checked)
        .map_err(|error| {
            let run_id = serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|value| value.get("run_id")?.as_str().map(str::to_string));
            match run_id {
                Some(run_id) => Failure::infrastructure(format!(
                    "the daemon at {} answered the dry run with run {run_id}, which it may have \
                     started: it does not support --dry-run. Stop that run (in the workbench, \
                     or POST /api/runs/{run_id}/stop), then restart the daemon with this \
                     axocoatl {}",
                    daemon.base(),
                    env!("CARGO_PKG_VERSION")
                )),
                None => Failure::infrastructure(format!(
                    "the daemon at {} answered the dry run with something that is not one \
                     ({error}); restart it with this axocoatl {}",
                    daemon.base(),
                    env!("CARGO_PKG_VERSION")
                )),
            }
        })
}

/// `axocoatl run --dry-run`: `POST /api/runs/preflight`, then what
/// admission found, as a summary (or the [`RunPreflight`] as JSON with
/// `--json`). Returns 0 when the run would start, else the exit code of its
/// first problem (3 usage, 5 infrastructure); 4 when the daemon cannot be
/// reached or is too old to have a dry run.
async fn dry_run(
    args: &RunArgs,
    console: &mut Console,
    note_after: Duration,
) -> Result<i32, Failure> {
    let task_from_file = match &args.task_file {
        Some(path) => Some(read_task_file(path)?),
        None => None,
    };
    let plan = plan(args, task_from_file)?;
    let daemon = Daemon::locate(args.url.as_deref(), &args.config).await?;
    let mut request = run_request(&plan)?;
    if !plan.task.is_empty() {
        request["task"] = serde_json::Value::String(plan.task.clone());
    }
    let asked = ask_preflight(&daemon, &request);
    tokio::pin!(asked);
    let preflight = tokio::select! {
        preflight = &mut asked => preflight?,
        () = tokio::time::sleep(note_after) => {
            console.note(DRY_RUN_NOTE);
            asked.await?
        }
    };
    if args.json {
        console.say(&serde_json::to_string_pretty(&preflight).unwrap_or_default());
    } else {
        console.print(&preflight_summary(&preflight, &plan.repo));
    }
    Ok(preflight.exit_code)
}

/// The summary `axocoatl run --dry-run` prints.
pub fn preflight_summary(preflight: &RunPreflight, repo: &Path) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Dry run of loadout {} in {}: nothing was created or started.",
        preflight.loadout,
        repo.display()
    );
    if !preflight.ollama_models.is_empty() {
        let loaded = preflight.ollama_models.iter().any(|model| model.loaded);
        let _ = writeln!(
            out,
            "Ollama models{}:",
            if loaded {
                " (each loaded once without a prompt and probed, as a run's admission does)"
            } else {
                ""
            }
        );
        for model in &preflight.ollama_models {
            let context = match model.context_tokens {
                Some(tokens) => format!("{tokens}-token context"),
                None => "context not known".into(),
            };
            let _ = writeln!(out, "  {}: {context}", model.model);
        }
    }
    if !preflight.params.is_empty() {
        let _ = writeln!(out, "Parameters:");
        for (name, value) in &preflight.params {
            let _ = writeln!(out, "  {name} = {value}");
        }
    }
    if !preflight.notes.is_empty() {
        let _ = writeln!(out, "Notes:");
        for note in &preflight.notes {
            let _ = writeln!(out, "  {note}");
        }
    }
    if !preflight.warnings.is_empty() {
        let _ = writeln!(out, "Warnings:");
        for warning in &preflight.warnings {
            let _ = writeln!(out, "  {}: {}", warning.code, warning.message);
        }
    }
    if let Some(sandbox) = &preflight.sandbox {
        let _ = writeln!(out, "Sandbox: {sandbox}");
    }
    if preflight.ready {
        let _ = writeln!(out, "Ready: yes, this run would start.");
        return out;
    }
    let count = preflight.problems.len();
    let _ = writeln!(
        out,
        "Ready: no, {count} problem{} (exit {}):",
        if count == 1 { "" } else { "s" },
        preflight.exit_code
    );
    for (index, problem) in preflight.problems.iter().enumerate() {
        let class = if problem.exit_code == exit_code::USAGE {
            "usage"
        } else {
            "infrastructure"
        };
        let _ = writeln!(out, "  {}. [{class}] {}", index + 1, problem.message);
    }
    out
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
            if let Some(receipt) = kept["receipt"].as_str() {
                report(
                    console,
                    &format!("Receipt: {receipt} (the commit's Axocoatl-Receipt trailer)"),
                );
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
    println!("# run: {}", run_command(&view.summary));
    Ok(0)
}

/// The `axocoatl run` command for a loadout with its required parameters,
/// as Settings shows it: a `ROLE_model` model parameter as `--model
/// ROLE=provider:model`, any other as `--param NAME=...`. A parameter with
/// a default is left out.
pub fn run_command(summary: &LoadoutSummary) -> String {
    let mut command = format!("axocoatl run {} --task \"...\"", summary.id);
    for param in summary.params.iter().filter(|param| param.required) {
        match param.name.strip_suffix("_model") {
            Some(role) if param.kind == "model" => {
                command.push_str(&format!(" --model {role}=provider:model"))
            }
            _ => command.push_str(&format!(" --param {}=...", param.name)),
        }
    }
    command
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

    /// `axocoatl loadouts show` ends with the command a loadout runs with,
    /// as Settings shows it: the built-in external fixes need only the
    /// reviewer, since their writer's model has a default.
    #[test]
    fn the_run_command_names_the_required_parameters() {
        let summary = |id: &str| {
            let loadout = axocoatl_config::loadout::builtin_loadouts()
                .into_iter()
                .map(Result::unwrap)
                .find(|loadout| loadout.file.id == id)
                .unwrap();
            axocoatl_daemon::bootstrap::loadout_runs::loadout_summary(&loadout)
        };
        for (id, command) in [
            (
                "fix",
                "axocoatl run fix --task \"...\" --model reviewer=provider:model --model \
                 writer=provider:model",
            ),
            (
                "fix-claude-code",
                "axocoatl run fix-claude-code --task \"...\" --model reviewer=provider:model",
            ),
            (
                "fix-codex",
                "axocoatl run fix-codex --task \"...\" --model reviewer=provider:model",
            ),
            (
                "qa",
                "axocoatl run qa --task \"...\" --model explorer=provider:model",
            ),
        ] {
            assert_eq!(run_command(&summary(id)), command);
        }
        let mut custom = summary("fix");
        custom.params[0].kind = "text".into();
        assert!(run_command(&custom).contains(" --param reviewer_model=..."));
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

    /// The daemon's 409 with the code `workspace_busy` (another run or
    /// Session holds the Workspace) exits 7 with the daemon's reason; a
    /// conflict without that code stays exit 5.
    #[tokio::test]
    async fn a_busy_workspace_exits_seven_and_another_conflict_five() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        let busy = "Workspace busy: the Workspace /repo is held by Session ses-1 (loadout run \
                    run-1), whose turn turn-1 is running";
        Mock::given(method("POST"))
            .and(path("/api/runs"))
            .respond_with(ResponseTemplate::new(409).set_body_json(
                serde_json::json!({"error": busy, "code": axocoatl_daemon::WORKSPACE_BUSY_CODE}),
            ))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/other"))
            .respond_with(
                ResponseTemplate::new(409)
                    .set_body_json(serde_json::json!({"error": "Session conflict: closing"})),
            )
            .mount(&server)
            .await;
        let daemon = Daemon {
            base: server.uri(),
            token: None,
            client: reqwest::Client::new(),
        };
        let failure = daemon
            .post_json::<serde_json::Value>("/api/runs", &serde_json::json!({}))
            .await
            .unwrap_err();
        assert_eq!(failure.code, exit_code::BUSY);
        assert_eq!(failure.message, busy);
        let failure = daemon
            .post_json::<serde_json::Value>("/api/other", &serde_json::json!({}))
            .await
            .unwrap_err();
        assert_eq!(
            failure.code,
            exit_code::INFRASTRUCTURE,
            "{}",
            failure.message
        );
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
        // A tokens budget that no model call fits is invalid (exit 3); the
        // daemon checks the model's real context when the run is admitted.
        std::fs::write(
            &file,
            fix.text
                .replace("id: fix", "id: my-fix")
                .replace("tokens: 4000000", "tokens: 8000"),
        )
        .unwrap();
        assert_eq!(validate(&file).unwrap(), exit_code::USAGE);
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

    /// The audit re-smoke's notes were only progress lines on standard
    /// error: the summary of a run that passed said nothing of them.
    #[test]
    fn the_summary_lists_the_notes() {
        let run = outcome(
            "pass",
            serde_json::json!({"notes": [
                "worker-ingest listed as not reached: ingest/legacy.py; a note: the host decides \
                 coverage from the files its workers read"
            ]}),
        );
        let text = summary(&run);
        assert!(
            text.contains(
                "Notes:\n  worker-ingest listed as not reached: ingest/legacy.py; a note: the \
                 host decides coverage from the files its workers read\nWarnings:"
            ),
            "{text}"
        );
        assert!(!summary(&outcome("pass", serde_json::json!({}))).contains("Notes:"));
    }

    /// An audit area whose worker's findings could not be read is listed
    /// apart from what was not covered; its answers stay in the record.
    #[test]
    fn the_summary_lists_unreadable_findings_apart_from_coverage() {
        let run = outcome(
            "needs_attention",
            serde_json::json!({
                "attention": ["Findings unreadable for ingest"],
                "unreadable_findings": [{
                    "area": "ingest",
                    "detail": "the area worker's FINDINGS block could not be read (the answer \
                               has no FINDINGS block), nor after 2 re-asks",
                    "answers": ["I found a defect in feed.go."]
                }]
            }),
        );
        let text = summary(&run);
        assert!(
            text.contains("Needs attention: Findings unreadable for ingest\n"),
            "{text}"
        );
        assert!(
            text.contains(
                "Findings unreadable:\n  ingest: the area worker's FINDINGS block could not be \
                 read (the answer has no FINDINGS block), nor after 2 re-asks\n"
            ),
            "{text}"
        );
        assert!(!text.contains("Not covered:"), "{text}");
        assert!(!text.contains("I found a defect"), "{text}");
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
                receipt: None,
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

    /// `--receipt` writes the envelope the daemon serves, also for a run
    /// that did not pass; a run the daemon could not sign is said and keeps
    /// its exit code.
    #[tokio::test]
    async fn the_receipt_is_written_whatever_the_verdict_and_its_absence_is_said() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        let envelope = b"{\"payloadType\":\"application/vnd.in-toto+json\",\"payload\":\"e30=\",\"signatures\":[]}\n";
        let run = fake_daemon(
            outcome("checks_failed", serde_json::json!({})),
            Vec::new(),
            200,
        )
        .await;
        Mock::given(method("GET"))
            .and(path(format!("/api/runs/{RUN}/receipt")))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                envelope.to_vec(),
                axocoatl_session::receipt::ENVELOPE_MEDIA_TYPE,
            ))
            .mount(&run.server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let receipt = dir.path().join("r.json");
        let (mut console, out, err) = captured_console();
        let code = run_on(
            run_args(
                &run.server.uri(),
                dir.path(),
                &["--receipt", receipt.to_str().unwrap()],
            ),
            &mut console,
            ADMISSION_NOTE_AFTER,
        )
        .await;
        let err = err.text();
        assert_eq!(code, exit_code::CHECKS_FAILED, "{}\n{err}", out.text());
        assert_eq!(std::fs::read(&receipt).unwrap(), envelope);
        assert!(
            err.contains(&format!("· receipt written to {}", receipt.display())),
            "{err}"
        );

        let run = fake_daemon(outcome("pass", serde_json::json!({})), Vec::new(), 200).await;
        Mock::given(method("GET"))
            .and(path(format!("/api/runs/{RUN}/receipt")))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "error": format!("run {RUN} has no receipt: the key cannot be read")
            })))
            .mount(&run.server)
            .await;
        let receipt = dir.path().join("none.json");
        let (mut console, _out, err) = captured_console();
        let code = run_on(
            run_args(
                &run.server.uri(),
                dir.path(),
                &["--receipt", receipt.to_str().unwrap()],
            ),
            &mut console,
            ADMISSION_NOTE_AFTER,
        )
        .await;
        let err = err.text();
        assert_eq!(code, exit_code::PASS, "{err}");
        assert!(!receipt.exists());
        assert!(
            err.contains(&format!(
                "· no receipt written to {}: run {RUN} has no receipt: the key cannot be read",
                receipt.display()
            )),
            "{err}"
        );
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
            format!("{ADMISSION_NOTE}\naxocoatl run: integrator_model: required\n")
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

    /// Ctrl-C while the daemon admits the run ends the command at once with
    /// exit code 6 and says what the daemon does with the run; the request
    /// was sent, and its answer is not waited for.
    #[tokio::test]
    async fn ctrl_c_during_admission_ends_the_command_as_interrupted() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/runs"))
            .respond_with(
                ResponseTemplate::new(202)
                    .set_body_json(serde_json::json!({"run_id": "run-1", "session_id": "ses-1",
                        "workspace_id": "wsp-1", "warnings": []}))
                    .set_delay(Duration::from_secs(60)),
            )
            .mount(&server)
            .await;
        let daemon = Daemon {
            base: server.uri(),
            token: None,
            client: reqwest::Client::new(),
        };
        let started = std::time::Instant::now();
        let (mut console, out, err) = captured_console();
        let failure = admit(
            &daemon,
            &serde_json::json!({"loadout": "fix"}),
            &mut console,
            Duration::from_millis(20),
            tokio::time::sleep(Duration::from_millis(300)),
        )
        .await
        .unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(30));
        assert_eq!(failure.code, exit_code::INTERRUPTED, "{}", failure.message);
        assert!(
            failure
                .message
                .contains("a run it admits stops before its first turn, as interrupted"),
            "{}",
            failure.message
        );
        assert_eq!(out.text(), "");
        assert_eq!(err.text(), format!("{ADMISSION_NOTE}\n"));
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
        // Pressed before the waiting note is due, the note is never printed.
        let (mut console, _, err) = captured_console();
        let failure = admit(
            &daemon,
            &serde_json::json!({"loadout": "fix"}),
            &mut console,
            Duration::from_secs(60),
            std::future::ready(()),
        )
        .await
        .unwrap_err();
        assert_eq!(failure.code, exit_code::INTERRUPTED);
        assert_eq!(err.text(), "");
    }

    /// A run the daemon refused at admission (or could not admit) has no
    /// run id: `--junit` still gets a valid file whose verdict is the
    /// refusal (`busy`, `usage` or `error`) with the command's exit code,
    /// and `--record` has nothing to write.
    #[tokio::test]
    async fn a_refused_run_writes_its_verdict_to_junit_and_no_record() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let busy = "Workspace busy: the Workspace /repo is held by Session ses-1 (loadout run \
                    run-1), whose turn turn-1 is running";
        let unreachable = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            drop(listener);
            format!("http://127.0.0.1:{port}")
        };
        for (status, body, code, kind) in [
            (
                Some(409),
                serde_json::json!({"error": busy, "code": axocoatl_daemon::WORKSPACE_BUSY_CODE}),
                exit_code::BUSY,
                "busy",
            ),
            (
                Some(422),
                serde_json::json!({"error": "integrator_model: required"}),
                exit_code::USAGE,
                "usage",
            ),
            (
                Some(500),
                serde_json::json!({"error": "the Session environment failed"}),
                exit_code::INFRASTRUCTURE,
                "error",
            ),
            (
                None,
                serde_json::Value::Null,
                exit_code::DAEMON_UNAVAILABLE,
                "error",
            ),
        ] {
            let server = MockServer::start().await;
            if let Some(status) = status {
                Mock::given(method("POST"))
                    .and(path("/api/runs"))
                    .respond_with(ResponseTemplate::new(status).set_body_json(body))
                    .mount(&server)
                    .await;
            }
            let url = match status {
                Some(_) => server.uri(),
                None => unreachable.clone(),
            };
            let dir = tempfile::tempdir().unwrap();
            let junit = dir.path().join("out").join("junit.xml");
            std::fs::create_dir(dir.path().join("out")).unwrap();
            let record = dir.path().join("out").join("run.axorecord.jsonl");
            let receipt = dir.path().join("out").join("r.json");
            let (mut console, out, err) = captured_console();
            let exit = run_on(
                run_args(
                    &url,
                    dir.path(),
                    &[
                        "--junit",
                        junit.to_str().unwrap(),
                        "--record",
                        record.to_str().unwrap(),
                        "--receipt",
                        receipt.to_str().unwrap(),
                    ],
                ),
                &mut console,
                ADMISSION_NOTE_AFTER,
            )
            .await;
            let (out, err) = (out.text(), err.text());
            assert_eq!(exit, code, "{err}");
            assert_eq!(out, "");
            let xml = std::fs::read_to_string(&junit).unwrap_or_else(|error| {
                panic!("{kind}: no JUnit file ({error})\n{err}");
            });
            assert!(
                xml.contains(&format!(
                    "<property name=\"axocoatl.exit_code\" value=\"{code}\"/>"
                )),
                "{xml}"
            );
            assert!(
                xml.contains(&format!("<error type=\"{kind}\" message=\"")),
                "{xml}"
            );
            assert!(
                xml.contains("<property name=\"axocoatl.loadout\" value=\"fix\"/>"),
                "{xml}"
            );
            if code == exit_code::BUSY {
                assert!(xml.contains("held by Session ses-1"), "{xml}");
            }
            assert!(!record.exists(), "a record was written for no run");
            assert!(!receipt.exists(), "a receipt was written for no run");
            // Only the files' notes after the reason; nothing says the run
            // started.
            assert!(!err.contains("· run "), "{err}");
            assert!(
                err.ends_with(&format!(
                    "· JUnit written to {}\n· no record written to {}: no run was admitted, so \
                     there is nothing to record\n· no receipt written to {}: no run was \
                     admitted, so there is nothing to sign\n",
                    junit.display(),
                    record.display(),
                    receipt.display()
                )),
                "{err}"
            );
            // Nothing else is left beside them.
            assert_eq!(
                std::fs::read_dir(dir.path().join("out")).unwrap().count(),
                1
            );
        }
    }

    /// A call whose cost is not known (a Codex writer on a model without a
    /// price): what its calls reserved is not shown as the run's cost.
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
        // A Codex writer's cost computed from its reported tokens says so.
        let computed = outcome(
            "pass",
            serde_json::json!({"usage": {"input_tokens": 4107, "output_tokens": 60,
                "cost_microunits": 17_835, "complete": true, "cost_known": true,
                "cost_computed": true}}),
        );
        assert!(summary(&computed).contains(
            "\nUsage: 4107 input + 60 output tokens, $0.0178 (includes cost computed from \
             reported tokens at list prices)\n"
        ));
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

    /// `--dry-run` asks `POST /api/runs/preflight`, needs no task (and sends
    /// none) and sends the toolchain flags, prints every problem with its
    /// class and the Ollama models it loaded, and exits with the first
    /// problem's code; nothing is admitted, so no file is written.
    #[tokio::test]
    async fn a_dry_run_lists_every_problem_and_exits_with_the_first_ones_code() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/runs/preflight"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "loadout": "fix-claude-code",
                "ready": false,
                "exit_code": 3,
                "problems": [
                    {"code": "credential", "exit_code": 3,
                     "message": "claude-code-oauth is not stored: run `axocoatl connect claude-code`"},
                    {"code": "recipe_image", "exit_code": 5,
                     "message": "no image is built from the recipes claude-code"}
                ],
                "params": {"reviewer_model": "ollama:gpt-oss:20b", "writer_model": "anthropic:sonnet"},
                "defaults": [{"param": "reviewer_model", "value": "ollama:gpt-oss:20b",
                              "source": "run.models.reviewer"}],
                "notes": ["reviewer_model = ollama:gpt-oss:20b (from your setup: run.models.reviewer in the configuration)"],
                "sandbox": "the Podman machine is stopped; a run starts it first",
                "ollama_models": [{"model": "ollama:gpt-oss:20b", "loaded": true, "context_tokens": 32768}]
            })))
            .expect(3)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let mut argv = run_args(&server.uri(), dir.path(), &["--dry-run"]);
        argv.task = None;
        argv.loadout = "fix-claude-code".into();
        let (mut console, out, err) = captured_console();
        let code = run_on(argv, &mut console, ADMISSION_NOTE_AFTER).await;
        assert_eq!(code, exit_code::USAGE, "{}", err.text());
        let repo = std::fs::canonicalize(dir.path().join("repo")).unwrap();
        assert_eq!(
            out.text(),
            format!(
                "Dry run of loadout fix-claude-code in {}: nothing was created or started.\n\
                 Ollama models (each loaded once without a prompt and probed, as a run's \
                 admission does):\n  ollama:gpt-oss:20b: 32768-token context\n\
                 Parameters:\n  reviewer_model = ollama:gpt-oss:20b\n  writer_model = \
                 anthropic:sonnet\nNotes:\n  reviewer_model = ollama:gpt-oss:20b (from your \
                 setup: run.models.reviewer in the configuration)\nSandbox: the Podman machine \
                 is stopped; a run starts it first\nReady: no, 2 problems (exit 3):\n  1. \
                 [usage] claude-code-oauth is not stored: run `axocoatl connect claude-code`\n  \
                 2. [infrastructure] no image is built from the recipes claude-code\n",
                repo.display()
            )
        );
        assert_eq!(err.text(), "");
        // With --json, standard output is the preflight document.
        let (mut console, out, _) = captured_console();
        let code = run_on(
            run_args(&server.uri(), dir.path(), &["--preflight", "--json"]),
            &mut console,
            ADMISSION_NOTE_AFTER,
        )
        .await;
        assert_eq!(code, exit_code::USAGE);
        let preflight: RunPreflight = serde_json::from_str(&out.text()).unwrap();
        assert_eq!(preflight.problems.len(), 2);
        let requests = server.received_requests().await.unwrap();
        let body: serde_json::Value = requests[0].body_json().unwrap();
        assert!(body.get("task").is_none(), "{body}");
        assert!(body.get("request_id").is_none(), "{body}");
        assert_eq!(body["loadout"], "fix-claude-code");
        let body: serde_json::Value = requests[1].body_json().unwrap();
        assert_eq!(body["task"], "fix the pagination bug");
        assert!(body.get("environment").is_none(), "{body}");
        assert!(requests.iter().all(|request| request.url.query().is_none()));
        // The toolchain flags reach the dry run, as they reach a run.
        let (mut console, _, _) = captured_console();
        run_on(
            run_args(
                &server.uri(),
                dir.path(),
                &["--dry-run", "--toolchain", "go", "--no-build", "--no-probe"],
            ),
            &mut console,
            ADMISSION_NOTE_AFTER,
        )
        .await;
        let requests = server.received_requests().await.unwrap();
        let body: serde_json::Value = requests[2].body_json().unwrap();
        assert_eq!(
            body["environment"],
            serde_json::json!({"toolchain": "go", "no_probe": true, "build_images": false}),
            "{body}"
        );
        // The run's files have nothing to hold.
        let parsed = Cli::try_parse_from(["axocoatl", "fix", "--dry-run", "--junit", "x.xml"]);
        assert!(parsed.is_err());
    }

    /// Set in the child process of
    /// [`a_run_that_cannot_start_is_refused_before_its_image_is_built`]: the
    /// file its stub `podman` appends each call to.
    const STUB_PODMAN_LOG: &str = "AXOCOATL_RUN_CMD_STUB_PODMAN_LOG";

    /// A run whose toolchain image is not built asks the daemon's dry run
    /// first, loading no model: a run that cannot start (here a credential
    /// that is not stored) is refused in admission's words and with its
    /// exit code, before anything is built or admitted. The body runs in a
    /// child process whose only `podman` is a stub that fails and logs its
    /// call, so a regression can never build an image on this computer.
    #[tokio::test]
    async fn a_run_that_cannot_start_is_refused_before_its_image_is_built() {
        if std::env::var_os(STUB_PODMAN_LOG).is_some() {
            cannot_start_child_body().await;
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let podman = bin.join("podman");
        std::fs::write(
            &podman,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"${STUB_PODMAN_LOG}\"\n\
                 echo 'this test runs no Podman' >&2\nexit 1\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&podman, std::fs::Permissions::from_mode(0o700)).unwrap();
        let log = root.path().join("podman.log");
        let result = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "run_cmd::tests::a_run_that_cannot_start_is_refused_before_its_image_is_built",
                "--nocapture",
            ])
            .env(STUB_PODMAN_LOG, &log)
            .env_remove("AXOCOATL_DATA_DIR")
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .kill_on_drop(true)
            .output()
            .await
            .unwrap();
        assert!(
            result.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        // The child ran its body, and asked Podman nothing.
        assert!(
            String::from_utf8_lossy(&result.stdout).contains("1 passed"),
            "{}",
            String::from_utf8_lossy(&result.stdout)
        );
        assert_eq!(std::fs::read_to_string(&log).unwrap_or_default(), "");
    }

    async fn cannot_start_child_body() {
        use axocoatl_daemon::loadout::api::{LoadoutGraph, LoadoutSummary};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let fix = axocoatl_config::loadout::builtin_loadouts()
            .into_iter()
            .map(Result::unwrap)
            .find(|loadout| loadout.file.id == "fix")
            .unwrap();
        let view = LoadoutView {
            summary: LoadoutSummary {
                id: "fix".into(),
                version: fix.file.version,
                name: fix.file.name.clone(),
                description: String::new(),
                kind: "fix".into(),
                builtin: true,
                opt_in: false,
                digest: fix.digest.clone(),
                params: Vec::new(),
                warnings: Vec::new(),
                error: None,
                path: None,
            },
            file: fix.file.clone(),
            text: fix.text.clone(),
            graph: LoadoutGraph {
                nodes: Vec::new(),
                edges: Vec::new(),
            },
        };
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/loadouts/fix"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&view))
            .mount(&server)
            .await;
        let problems = [
            "Agent writer (claude-code) needs the credential claude-code-oauth, and it is not \
             stored yet: run `axocoatl connect claude-code`, then run again",
            "the reviewer runs anthropic:opus, on the provider anthropic, and a native Agent or \
             reviewer runs only on Ollama or OpenRouter",
        ];
        Mock::given(method("POST"))
            .and(path("/api/runs/preflight"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "loadout": "fix",
                "ready": false,
                "exit_code": 3,
                "problems": problems.iter().map(|message| serde_json::json!({
                    "code": "credential", "exit_code": 3, "message": message
                })).collect::<Vec<_>>(),
                "params": {},
                "notes": ["the run's image localhost/axocoatl-recipe-python:x is not built on \
                           this computer; a run builds it first"]
            })))
            .expect(1)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let argv = run_args(&server.uri(), dir.path(), &[]);
        for (name, contents) in [
            ("pyproject.toml", "[project]\nname = 'pages'\n"),
            ("tests/conftest.py", ""),
            ("pages.py", ""),
        ] {
            let file = dir.path().join("repo").join(name);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, contents).unwrap();
        }
        let (mut console, out, err) = captured_console();
        let code = run_on(argv, &mut console, ADMISSION_NOTE_AFTER).await;
        assert_eq!(code, exit_code::USAGE, "{}", err.text());
        assert_eq!(out.text(), "");
        assert_eq!(
            err.text(),
            format!(
                "axocoatl run: 2 problems stop this run, and nothing was started:\n  1. {}\n  \
                 2. {}\n",
                problems[0], problems[1]
            )
        );
        let requests = server.received_requests().await.unwrap();
        assert_eq!(
            paths_requested(&requests),
            ["GET /api/loadouts/fix", "POST /api/runs/preflight"]
        );
        let body: serde_json::Value = requests[1].body_json().unwrap();
        assert_eq!(body["load_models"], false, "{body}");
        assert_eq!(body["task"], "fix the pagination bug");
        assert!(
            body["request_id"]
                .as_str()
                .is_some_and(|id| id.starts_with("cli-")),
            "{body}"
        );
    }

    /// A daemon older than this command has no dry run: its 405 (or 404)
    /// says to restart it, exit code 4, and nothing else is asked. An
    /// answer that names a run is never taken for a dry run.
    #[tokio::test]
    async fn a_dry_run_against_an_older_daemon_never_counts_as_one() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let dir = tempfile::tempdir().unwrap();
        for status in [404, 405] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/api/runs/preflight"))
                .respond_with(ResponseTemplate::new(status))
                .expect(1)
                .mount(&server)
                .await;
            let (mut console, out, err) = captured_console();
            let code = run_on(
                run_args(&server.uri(), dir.path(), &["--dry-run"]),
                &mut console,
                ADMISSION_NOTE_AFTER,
            )
            .await;
            assert_eq!(code, exit_code::DAEMON_UNAVAILABLE, "{}", err.text());
            assert_eq!(out.text(), "");
            assert!(
                err.text().contains(&format!(
                    "has no dry run (POST /api/runs/preflight answered {status}"
                )) && err.text().contains("nothing was started")
                    && err.text().contains("axocoatl dev"),
                "{}",
                err.text()
            );
            assert_eq!(
                paths_requested(&server.received_requests().await.unwrap()),
                ["POST /api/runs/preflight"]
            );
        }
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/runs/preflight"))
            .respond_with(ResponseTemplate::new(202).set_body_json(serde_json::json!({
                "run_id": "run-1", "session_id": "ses-1", "workspace_id": "wsp-1", "warnings": []
            })))
            .mount(&server)
            .await;
        let (mut console, _, err) = captured_console();
        let code = run_on(
            run_args(&server.uri(), dir.path(), &["--dry-run"]),
            &mut console,
            ADMISSION_NOTE_AFTER,
        )
        .await;
        assert_eq!(code, exit_code::INFRASTRUCTURE);
        assert!(
            err.text().contains("answered the dry run with run run-1")
                && err.text().contains("POST /api/runs/run-1/stop"),
            "{}",
            err.text()
        );
    }

    /// While a dry run checks for longer than the note delay, it says what
    /// it is doing: loading the Ollama models and asking Podman.
    #[tokio::test]
    async fn a_slow_dry_run_says_it_loads_the_models() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/runs/preflight"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({
                        "loadout": "fix", "ready": true, "exit_code": 0, "problems": [],
                        "params": {}
                    }))
                    .set_delay(Duration::from_millis(300)),
            )
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let (mut console, out, err) = captured_console();
        let code = run_on(
            run_args(&server.uri(), dir.path(), &["--dry-run"]),
            &mut console,
            Duration::from_millis(10),
        )
        .await;
        assert_eq!(code, 0, "{}", err.text());
        assert_eq!(err.text(), format!("{DRY_RUN_NOTE}\n"));
        assert!(out.text().contains("Ready: yes"), "{}", out.text());
    }

    /// An admission refusal the daemon classes as infrastructure (exit code
    /// 5) prints in its own words, without the HTTP status and error class.
    #[tokio::test]
    async fn an_infrastructure_refusal_prints_without_the_http_status() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/runs"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": "Session error: a loadout Session needs rootless Podman, and Podman is \
                          not ready: podman not installed"
            })))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let (mut console, _, err) = captured_console();
        let code = run_on(
            run_args(&server.uri(), dir.path(), &[]),
            &mut console,
            ADMISSION_NOTE_AFTER,
        )
        .await;
        assert_eq!(code, exit_code::INFRASTRUCTURE);
        assert_eq!(
            err.text(),
            "axocoatl run: a loadout Session needs rootless Podman, and Podman is not ready: \
             podman not installed\n"
        );
    }

    /// `axocoatl run --help` names every built-in loadout.
    #[test]
    fn the_help_names_every_builtin_loadout() {
        use clap::CommandFactory;
        let help = Cli::command().render_long_help().to_string();
        for id in axocoatl_config::loadout::builtin_loadouts()
            .into_iter()
            .map(|loadout| loadout.unwrap().file.id)
        {
            assert!(help.contains(&format!("`{id}`")), "{id} in\n{help}");
        }
        assert!(help.contains("--dry-run"), "{help}");
    }
}
