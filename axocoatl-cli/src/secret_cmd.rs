//! `axocoatl secret set|list|remove`: the secret store for route
//! credentials. A value is read from stdin or, with `--from-env VAR`, from
//! the named environment variable, never from argv, and never printed.
//! Owner: workstream `agents`. For Claude Code, `axocoatl connect
//! claude-code` (`connect_cmd`) obtains and stores the token itself.
//!
//! The store is `{data dir}/secrets/<name>` (0600 files in a 0700
//! directory), where the data dir is resolved exactly as the daemon's:
//! `AXOCOATL_DATA_DIR`, else the data directory of `--config`. A route whose
//! `credential` names a stored secret (and no `credentials` entry) gets its
//! value added upstream by the daemon; the Session's containers only ever
//! see a placeholder.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use axocoatl_config::egress_routes::is_valid_credential_name;
use axocoatl_daemon::external_agent::claude_code::CLAUDE_CODE_SECRET;
use axocoatl_daemon::secret_store;
use clap::Subcommand;
use zeroize::Zeroizing;

/// Exit codes: 0 done, 3 usage (bad name or value, a terminal on stdin), 5
/// the store could not be read or written.
const USAGE: i32 = 3;
const FAILURE: i32 = 5;

#[derive(Debug, Subcommand)]
pub enum SecretCommands {
    /// Store a secret read from stdin or from an environment variable (for
    /// Claude Code, use `axocoatl connect claude-code`)
    Set {
        name: String,
        /// Read the value from this environment variable instead of stdin
        #[arg(long, value_name = "VAR")]
        from_env: Option<String>,
        /// Path to config file (selects the data directory)
        #[arg(short, long, default_value_os_t = crate::default_config_path_for_clap())]
        config: PathBuf,
        /// A value given on the command line (`secret set NAME "$TOKEN"`).
        /// It is refused without being shown: clap's own "unexpected
        /// argument" error would print it.
        #[arg(hide = true, value_name = "VALUE")]
        on_command_line: Vec<String>,
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
///
/// Only the well-known names are repeated, as `secret set "$TOKEN"` typed in
/// a terminal would otherwise print a value that happens to look like a name.
fn pipe_hint(name: &str) -> String {
    let (name, variable) = match name {
        "codex-openai" => (name, "OPENAI_API_KEY"),
        CLAUDE_CODE_SECRET => (name, "VAR"),
        _ => ("<name>", "VAR"),
    };
    let mut hint = format!(
        "axocoatl secret set reads the value from stdin or an environment variable, not from \
         the command line or the keyboard. For example:\n  axocoatl secret set {name} \
         --from-env {variable}\n  axocoatl secret set {name} < token-file"
    );
    if name == CLAUDE_CODE_SECRET {
        hint.push_str(
            "\nFor Claude Code, run `axocoatl connect claude-code`: it runs `claude setup-token` \
             and stores the token without showing it.",
        );
    }
    hint
}

/// The data dir the daemon uses for `config`, created owner-only if absent.
pub(crate) fn data_dir(config: &Path) -> Result<PathBuf, String> {
    let data_dir = crate::configure_data_dir(config)?;
    axocoatl_daemon::AxocoatlDaemon::initialize_data_root(&data_dir)
        .map_err(|error| format!("opening the data directory: {error}"))?;
    Ok(data_dir)
}

/// What `secret set` stored: its confirmation (standard output) and its
/// warnings (standard error), none of which contains the value.
#[derive(Debug)]
pub(crate) struct Stored {
    pub(crate) message: String,
    pub(crate) warnings: Vec<String>,
}

/// `secret set` with an explicit stdin, for tests. A value that does not
/// look like one bare token is stored, with a warning that says why and how
/// to store it again.
pub(crate) fn set_from(
    data_dir: &Path,
    name: &str,
    stdin: impl std::io::Read,
    stdin_is_terminal: bool,
) -> Result<Stored, (i32, String)> {
    check_name(name)?;
    if stdin_is_terminal {
        return Err((USAGE, pipe_hint(name)));
    }
    let value =
        secret_store::read_secret_value(stdin).map_err(|error| (USAGE, error.to_string()))?;
    store(data_dir, name, &value)
}

/// A secret name, checked before anything could repeat it: an invalid one
/// (often a value passed where the name belongs) is refused unshown.
fn check_name(name: &str) -> Result<(), (i32, String)> {
    if is_valid_credential_name(name) {
        Ok(())
    } else {
        Err((USAGE, secret_store::INVALID_NAME.to_string()))
    }
}

/// The refusal of `secret set NAME VALUE`, which never repeats the value.
pub(crate) const VALUE_ON_COMMAND_LINE: &str =
    "secret set takes only a name, never a value on the command line (it is not shown here); \
     nothing was stored. Pass the value with `--from-env VAR` (the variable's name, without \
     `$`) or on stdin. If that argument was a secret, it is now in your shell history: \
     replace it.";

/// Whether `variable` looks like an environment variable's name: a letter
/// or `_`, then up to 127 letters, digits or `_`.
fn is_variable_name(variable: &str) -> bool {
    let mut bytes = variable.bytes();
    matches!(bytes.next(), Some(b'A'..=b'Z' | b'a'..=b'z' | b'_'))
        && variable.len() <= 128
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// `secret set --from-env variable`, with the environment as `lookup`, for
/// tests. An unset or empty variable is refused. Neither the value nor the
/// argument is ever echoed: `--from-env "$TOKEN"` passes the value where the
/// name belongs, and a value can look like a name (`ghp_…`).
pub(crate) fn set_from_env(
    data_dir: &Path,
    name: &str,
    variable: &str,
    lookup: impl FnOnce(&str) -> Option<std::ffi::OsString>,
) -> Result<Stored, (i32, String)> {
    use std::os::unix::ffi::OsStringExt;
    check_name(name)?;
    if !is_variable_name(variable) {
        return Err((
            USAGE,
            "--from-env takes the name of an environment variable (such as OPENAI_API_KEY, \
             without `$`), not its value; the argument is not shown and nothing was stored. If \
             it was a secret, it is now in your shell history: replace it."
                .to_string(),
        ));
    }
    let Some(value) = lookup(variable) else {
        return Err((
            USAGE,
            "the environment variable named by --from-env is not set (export it first if it \
             is a shell variable; its name is not shown, in case it is a value); nothing was \
             stored"
                .to_string(),
        ));
    };
    let value = Zeroizing::new(value.into_vec());
    if value.is_empty() {
        return Err((
            USAGE,
            "the environment variable named by --from-env is empty; nothing was stored".to_string(),
        ));
    }
    let value =
        secret_store::normalize_secret_value(&value).map_err(|error| (USAGE, error.to_string()))?;
    store(data_dir, name, &value)
}

/// Store `value` (already normalized) and describe it without its value.
fn store(data_dir: &Path, name: &str, value: &[u8]) -> Result<Stored, (i32, String)> {
    let bytes = value.len();
    secret_store::set_secret(data_dir, name, value).map_err(|error| match error {
        secret_store::SecretStoreError::Invalid(_) => (USAGE, error.to_string()),
        _ => (FAILURE, error.to_string()),
    })?;
    let reasons = secret_store::value_warnings(name, value);
    let mut warnings: Vec<String> = reasons
        .iter()
        .map(|reason| format!("! secret {name}: {reason}."))
        .collect();
    if !warnings.is_empty() {
        warnings.push(format!(
            "! It was stored as given. If it is not the bare token, {}.",
            secret_store::store_again_hint(name)
        ));
    }
    Ok(Stored {
        message: format!(
            "Stored secret {name} ({bytes} bytes) in {}. Routes that name it as their credential \
             get it from the daemon; containers only see a placeholder.",
            secret_store::secret_path(data_dir, name).display()
        ),
        warnings,
    })
}

/// Returns the process exit code.
pub async fn cmd_secret(command: SecretCommands) -> i32 {
    if let SecretCommands::Set {
        on_command_line, ..
    } = &command
    {
        if !on_command_line.is_empty() {
            eprintln!("✗ {VALUE_ON_COMMAND_LINE}");
            return USAGE;
        }
    }
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
        SecretCommands::Set { name, from_env, .. } => {
            let result = match from_env {
                Some(variable) => set_from_env(&data_dir, &name, &variable, |variable| {
                    std::env::var_os(variable)
                }),
                None => {
                    let stdin = std::io::stdin();
                    let terminal = stdin.is_terminal();
                    set_from(&data_dir, &name, stdin.lock(), terminal)
                }
            };
            match result {
                Ok(stored) => {
                    println!("✓ {}", stored.message);
                    for warning in &stored.warnings {
                        eprintln!("{warning}");
                    }
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
                        "No secrets stored. Add one with `axocoatl connect claude-code`, \
                         `axocoatl secret set <name> --from-env VAR`, or \
                         `<command that prints it> | axocoatl secret set <name>`"
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
            &["x", "set", "name", "--value", "sk-value"][..],
            &["x", "set"],
        ] {
            assert!(Cli::try_parse_from(argv).is_err(), "{argv:?}");
        }
        // A value after the name parses, so that `cmd_secret` refuses it
        // with a message that does not repeat it (clap's error would).
        let parsed = Cli::try_parse_from(["x", "set", "name", "sk-value"]).unwrap();
        assert!(
            matches!(&parsed.command, SecretCommands::Set { on_command_line, .. }
            if on_command_line == &["sk-value"])
        );
        assert!(!VALUE_ON_COMMAND_LINE.contains("sk-"));
        let parsed = Cli::try_parse_from(["x", "set", "name", "-c", "/tmp/a.yaml"]).unwrap();
        assert!(
            matches!(parsed.command, SecretCommands::Set { name, config, from_env: None, on_command_line }
            if name == "name" && config == Path::new("/tmp/a.yaml") && on_command_line.is_empty())
        );
        let parsed = Cli::try_parse_from(["x", "set", "name", "--from-env", "MY_TOKEN"]).unwrap();
        assert!(
            matches!(parsed.command, SecretCommands::Set { from_env: Some(variable), .. }
            if variable == "MY_TOKEN")
        );
        assert!(Cli::try_parse_from(["x", "set", "name", "--from-env"]).is_err());
    }

    #[test]
    fn set_reads_stdin_refuses_a_terminal_and_never_prints_the_value() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let (code, message) =
            set_from(root.path(), "claude-code-oauth", &b"sk-typed"[..], true).unwrap_err();
        assert_eq!(code, USAGE);
        assert!(message.contains("axocoatl secret set claude-code-oauth --from-env VAR"));
        assert!(
            message.contains("run `axocoatl connect claude-code`"),
            "{message}"
        );
        assert!(!message.contains("claude setup-token |"), "{message}");
        assert!(secret_store::list_secrets(root.path()).unwrap().is_empty());

        let stored = set_from(
            root.path(),
            "claude-code-oauth",
            &b"sk-ant-oat01-piped\n"[..],
            false,
        )
        .unwrap();
        assert!(stored.warnings.is_empty(), "{:?}", stored.warnings);
        let message = stored.message;
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

    /// A value that is not one bare token is still stored, with warnings on
    /// standard error that say why and how to store it again, never the
    /// value itself.
    #[test]
    fn set_warns_about_a_value_that_is_not_one_bare_token_and_still_stores_it() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let value = "Bearer sk-ant-api03-secretvalue";
        let stored = set_from(
            root.path(),
            "claude-code-oauth",
            format!("{value}\n").as_bytes(),
            false,
        )
        .unwrap();
        let path = secret_store::secret_path(root.path(), "claude-code-oauth");
        assert_eq!(std::fs::read(&path).unwrap(), value.as_bytes());
        assert_eq!(stored.warnings.len(), 3, "{:?}", stored.warnings);
        assert!(stored.warnings[0]
            .starts_with("! secret claude-code-oauth: it starts with \"Bearer \""));
        assert!(stored.warnings[1].contains("starts with \"sk-ant-oat01-\""));
        assert!(stored.warnings[2].contains(
            "If it is not the bare token, connect again with `axocoatl connect claude-code`, \
             which runs `claude setup-token`"
        ));
        for line in stored.warnings.iter().chain([&stored.message]) {
            assert!(!line.contains("secretvalue"), "{line}");
        }
        // JSON and several tokens, for any name.
        let stored = set_from(
            root.path(),
            "example-token",
            &br#"{"access_token": "abc", "refresh_token": "def"}"#[..],
            false,
        )
        .unwrap();
        let text = stored.warnings.join("\n");
        assert!(text.contains("looks like JSON"), "{text}");
        assert!(text.contains("whitespace inside"), "{text}");
        assert!(text.contains("tokens or words"), "{text}");
        assert!(!text.contains("abc") && !text.contains("def"), "{text}");
        assert_eq!(
            std::fs::read(secret_store::secret_path(root.path(), "example-token")).unwrap(),
            br#"{"access_token": "abc", "refresh_token": "def"}"#
        );
    }

    /// `--from-env VAR` reads the value from the named variable, refuses an
    /// unset or empty one, and never echoes the value.
    #[test]
    fn set_from_env_reads_the_named_variable_and_never_prints_it() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let lookup = |wanted: &'static str, value: &'static str| {
            move |variable: &str| {
                assert_eq!(variable, wanted);
                Some(std::ffi::OsString::from(value))
            }
        };
        let stored = set_from_env(
            root.path(),
            "codex-openai",
            "OPENAI_API_KEY",
            lookup("OPENAI_API_KEY", "sk-proj-envvalue\n"),
        )
        .unwrap();
        assert!(stored.warnings.is_empty(), "{:?}", stored.warnings);
        assert!(!stored.message.contains("envvalue"), "{}", stored.message);
        assert!(stored.message.contains("16 bytes"), "{}", stored.message);
        let path = secret_store::secret_path(root.path(), "codex-openai");
        assert_eq!(std::fs::read(&path).unwrap(), b"sk-proj-envvalue");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let (code, message) =
            set_from_env(root.path(), "codex-openai", "UNSET_VAR", |_| None).unwrap_err();
        assert_eq!(code, USAGE);
        assert!(
            message.starts_with("the environment variable named by --from-env is not set"),
            "{message}"
        );
        assert!(!message.contains("UNSET_VAR"), "{message}");
        let (code, message) =
            set_from_env(root.path(), "codex-openai", "EMPTY", lookup("EMPTY", "")).unwrap_err();
        assert_eq!(code, USAGE);
        assert_eq!(
            message,
            "the environment variable named by --from-env is empty; nothing was stored"
        );
        for variable in ["", "A=B", "1ABC", "A-B", "A B", "\u{e9}", &"A".repeat(129)] {
            let (code, message) =
                set_from_env(root.path(), "codex-openai", variable, |_| None).unwrap_err();
            assert_eq!(code, USAGE);
            assert!(
                message.starts_with("--from-env takes the name"),
                "{message}"
            );
        }
        assert!(is_variable_name(&"A".repeat(128)) && is_variable_name("_a1"));
        let (code, message) = set_from_env(
            root.path(),
            "codex-openai",
            "TWO_LINES",
            lookup("TWO_LINES", "first-secret\nsecond-secret"),
        )
        .unwrap_err();
        assert_eq!(code, USAGE);
        assert!(
            !message.contains("first-secret") && !message.contains("second-secret"),
            "{message}"
        );
        // The value stored before is untouched by the refusals.
        assert_eq!(std::fs::read(&path).unwrap(), b"sk-proj-envvalue");
    }

    /// `--from-env "$TOKEN"` (the value where the variable's name belongs)
    /// and `secret set "$TOKEN"` (the value where the secret's name
    /// belongs) are refused without the value appearing in any message,
    /// whether it looks like a name or not.
    #[test]
    fn a_value_passed_as_a_name_is_never_echoed() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let values = [
            "sk-ant-oat01-FAKEFAKEfakefake0123456789-abcdefghijklmnopqrstuvwxyzABCDEFGHIJ_KLMNOAA",
            "sk-proj-FAKEPROBEVALUE0123456789abcdef",
            "OPENAI_API_KEY=sk-proj-FAKEPROBEVALUE0123456789abcdef",
            "ghp_FAKEfake0123456789FAKEfake0123456789",
            "AKIAFAKEFAKEFAKEFAKE",
        ];
        let shows = |message: &str, value: &str| {
            let tail = value.rsplit(['=', '-', '_']).next().unwrap_or(value);
            message.contains(value) || (tail.len() >= 8 && message.contains(tail))
        };
        for value in values {
            let (code, message) =
                set_from_env(root.path(), "codex-openai", value, |_| None).unwrap_err();
            assert_eq!(code, USAGE);
            assert!(!shows(&message, value), "{message}");
            let (code, message) =
                set_from_env(root.path(), "codex-openai", value, |_| Some("".into())).unwrap_err();
            assert_eq!(code, USAGE);
            assert!(!shows(&message, value), "{message}");
            for terminal in [true, false] {
                if let Err((_, message)) = set_from(root.path(), value, &b"x"[..], terminal) {
                    assert!(!shows(&message, value), "{message}");
                } else {
                    assert!(
                        is_valid_credential_name(value),
                        "an invalid name was stored: {value}"
                    );
                }
            }
            if !is_valid_credential_name(value) {
                let (code, message) =
                    set_from_env(root.path(), value, "OPENAI_API_KEY", |_| None).unwrap_err();
                assert_eq!(code, USAGE);
                assert!(!shows(&message, value), "{message}");
            }
        }
    }
}
