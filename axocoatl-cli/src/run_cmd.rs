//! `axocoatl run`, `axocoatl loadouts`, `axocoatl record`. They talk to the
//! running daemon over its HTTP API with the local API token (or
//! `AXOCOATL_URL` / `AXOCOATL_TOKEN`). Owner: workstream `core`.
//! Contract: docs/design/1.3-loadouts.md ("axocoatl run").

use std::path::PathBuf;

use axocoatl_session::run_outcome::exit_code;
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
    /// The command a `detected` check runs when the repository has none
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
    },
    /// Show one loadout's file and graph
    Show {
        id: String,
        #[arg(long)]
        url: Option<String>,
    },
    /// Validate a loadout file without the daemon
    Validate { file: PathBuf },
}

#[derive(Debug, Subcommand)]
pub enum RecordCommands {
    /// Verify a record bundle's order, line count and digest
    Verify { file: PathBuf },
}

/// Run `axocoatl run`; returns the process exit code.
pub async fn cmd_run(_args: RunArgs) -> i32 {
    eprintln!("✗ not implemented: axocoatl run");
    exit_code::INFRASTRUCTURE
}

/// Run `axocoatl loadouts ...`; returns the process exit code.
pub async fn cmd_loadouts(_command: LoadoutCommands) -> i32 {
    eprintln!("✗ not implemented: axocoatl loadouts");
    exit_code::INFRASTRUCTURE
}

/// Run `axocoatl record ...`; returns the process exit code.
pub async fn cmd_record(_command: RecordCommands) -> i32 {
    eprintln!("✗ not implemented: axocoatl record");
    exit_code::INFRASTRUCTURE
}
