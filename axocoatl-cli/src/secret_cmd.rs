//! `axocoatl secret set|list|remove`: the secret store for route
//! credentials. A value is read from stdin, never from argv, and never
//! printed. Owner: workstream `agents`.

use clap::Subcommand;

#[derive(Debug, Subcommand)]
pub enum SecretCommands {
    /// Store a secret read from stdin (for example the output of
    /// `claude setup-token`)
    Set { name: String },
    /// List stored secret names
    List,
    /// Remove a stored secret
    Remove { name: String },
}

/// Returns the process exit code.
pub async fn cmd_secret(_command: SecretCommands) -> i32 {
    eprintln!("✗ not implemented: axocoatl secret");
    5
}
