//! `axocoatl secret set|list|remove`: the secret store for route
//! credentials. A value is read from stdin, never from argv, and never
//! printed. Owner: workstream `agents`.
//!
//! The store is `{data dir}/secrets/<name>` (0600 files in a 0700
//! directory), where the data dir is resolved exactly as the daemon's:
//! `AXOCOATL_DATA_DIR`, else the data directory of `--config`. A route whose
//! `credential` names a stored secret (and no `credentials` entry) gets its
//! value added upstream by the daemon; the Session's containers only ever
//! see a placeholder.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use axocoatl_daemon::secret_store;
use clap::Subcommand;

/// Exit codes: 0 done, 3 usage (bad name or value, a terminal on stdin), 5
/// the store could not be read or written.
const USAGE: i32 = 3;
const FAILURE: i32 = 5;

#[derive(Debug, Subcommand)]
pub enum SecretCommands {
    /// Store a secret read from stdin (for example the output of
    /// `claude setup-token`)
    Set {
        name: String,
        /// Path to config file (selects the data directory)
        #[arg(short, long, default_value_os_t = crate::default_config_path_for_clap())]
        config: PathBuf,
    },
    /// List stored secret names
    List {
        /// Path to config file (selects the data directory)
        #[arg(short, long, default_value_os_t = crate::default_config_path_for_clap())]
        config: PathBuf,
    },
    /// Remove a stored secret
    Remove {
        name: String,
        /// Path to config file (selects the data directory)
        #[arg(short, long, default_value_os_t = crate::default_config_path_for_clap())]
        config: PathBuf,
    },
}

/// The hint printed when stdin is a terminal: values never come from
/// argv or from typing them where they would echo.
fn pipe_hint(name: &str) -> String {
    let source = match name {
        "claude-code-oauth" => "claude setup-token",
        "codex-openai" => "printenv OPENAI_API_KEY",
        _ => "<command that prints it>",
    };
    format!(
        "axocoatl secret set reads the value from stdin, not from the command line or the \
         keyboard. Pipe it in, for example:\n  {source} | axocoatl secret set {name}\n  \
         axocoatl secret set {name} < token-file"
    )
}

/// The data dir the daemon uses for `config`, created owner-only if absent.
fn data_dir(config: &Path) -> Result<PathBuf, String> {
    let data_dir = crate::configure_data_dir(config)?;
    axocoatl_daemon::AxocoatlDaemon::initialize_data_root(&data_dir)
        .map_err(|error| format!("opening the data directory: {error}"))?;
    Ok(data_dir)
}

/// `secret set` with an explicit stdin, for tests.
pub(crate) fn set_from(
    data_dir: &Path,
    name: &str,
    stdin: impl std::io::Read,
    stdin_is_terminal: bool,
) -> Result<String, (i32, String)> {
    if stdin_is_terminal {
        return Err((USAGE, pipe_hint(name)));
    }
    let value =
        secret_store::read_secret_value(stdin).map_err(|error| (USAGE, error.to_string()))?;
    let bytes = value.len();
    secret_store::set_secret(data_dir, name, &value).map_err(|error| match error {
        secret_store::SecretStoreError::Invalid(_) => (USAGE, error.to_string()),
        _ => (FAILURE, error.to_string()),
    })?;
    Ok(format!(
        "Stored secret {name} ({bytes} bytes) in {}. Routes that name it as their credential \
         get it from the daemon; containers only see a placeholder.",
        secret_store::secret_path(data_dir, name).display()
    ))
}

/// Returns the process exit code.
pub async fn cmd_secret(command: SecretCommands) -> i32 {
    let config = match &command {
        SecretCommands::Set { config, .. }
        | SecretCommands::List { config }
        | SecretCommands::Remove { config, .. } => config.clone(),
    };
    let data_dir = match data_dir(&config) {
        Ok(data_dir) => data_dir,
        Err(error) => {
            eprintln!("✗ {error}");
            return FAILURE;
        }
    };
    match command {
        SecretCommands::Set { name, .. } => {
            let stdin = std::io::stdin();
            let terminal = stdin.is_terminal();
            match set_from(&data_dir, &name, stdin.lock(), terminal) {
                Ok(message) => {
                    println!("✓ {message}");
                    0
                }
                Err((code, message)) => {
                    eprintln!("✗ {message}");
                    code
                }
            }
        }
        SecretCommands::List { .. } => match secret_store::list_secrets(&data_dir) {
            Ok(names) => {
                if names.is_empty() {
                    eprintln!(
                        "No secrets stored. Add one with: <command that prints it> | axocoatl secret set <name>"
                    );
                }
                for name in names {
                    println!("{name}");
                }
                0
            }
            Err(error) => {
                eprintln!("✗ {error}");
                FAILURE
            }
        },
        SecretCommands::Remove { name, .. } => {
            match secret_store::remove_secret(&data_dir, &name) {
                Ok(true) => {
                    println!("✓ Removed secret {name}");
                    0
                }
                Ok(false) => {
                    eprintln!("✗ No secret named {name}");
                    USAGE
                }
                Err(error @ secret_store::SecretStoreError::Invalid(_)) => {
                    eprintln!("✗ {error}");
                    USAGE
                }
                Err(error) => {
                    eprintln!("✗ {error}");
                    FAILURE
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::os::unix::fs::PermissionsExt;

    #[derive(Parser)]
    struct Cli {
        #[command(subcommand)]
        command: SecretCommands,
    }

    #[test]
    fn set_takes_no_value_on_the_command_line() {
        assert!(Cli::try_parse_from(["x", "set", "name"]).is_ok());
        for argv in [
            &["x", "set", "name", "sk-value"][..],
            &["x", "set", "name", "--value", "sk-value"],
            &["x", "set"],
        ] {
            assert!(Cli::try_parse_from(argv).is_err(), "{argv:?}");
        }
        let parsed = Cli::try_parse_from(["x", "set", "name", "-c", "/tmp/a.yaml"]).unwrap();
        assert!(
            matches!(parsed.command, SecretCommands::Set { name, config }
            if name == "name" && config == Path::new("/tmp/a.yaml"))
        );
    }

    #[test]
    fn set_reads_stdin_refuses_a_terminal_and_never_prints_the_value() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let (code, message) =
            set_from(root.path(), "claude-code-oauth", &b"sk-typed"[..], true).unwrap_err();
        assert_eq!(code, USAGE);
        assert!(message.contains("claude setup-token | axocoatl secret set claude-code-oauth"));
        assert!(secret_store::list_secrets(root.path()).unwrap().is_empty());

        let message = set_from(
            root.path(),
            "claude-code-oauth",
            &b"sk-ant-oat01-piped\n"[..],
            false,
        )
        .unwrap();
        assert!(!message.contains("sk-ant"), "{message}");
        assert!(message.contains("18 bytes"), "{message}");
        let path = secret_store::secret_path(root.path(), "claude-code-oauth");
        assert_eq!(std::fs::read(&path).unwrap(), b"sk-ant-oat01-piped");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let (code, message) =
            set_from(root.path(), "bad name", &b"sk-ant-x"[..], false).unwrap_err();
        assert_eq!(code, USAGE);
        assert!(!message.contains("sk-ant"));
        let (code, message) =
            set_from(root.path(), "name", &b"line one\nline two"[..], false).unwrap_err();
        assert_eq!(code, USAGE);
        assert!(!message.contains("line one"), "{message}");
    }
}
