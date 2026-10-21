//! Every command the CLI prints for a person to run can be pasted into a
//! shell as it is. A choice written as `a|b` is a pipeline there:
//! `axocoatl onboard --replace --writer claude-code|codex|ollama|openrouter`
//! would replace the configuration with a Claude Code one and pipe it into
//! `codex`, `ollama` and `openrouter`. An optional part written as
//! `[--flag]` is a glob. A choice is a placeholder in capitals instead
//! (`--writer WRITER`), with the choices in words beside the command.
//!
//! These checks read the printed texts (doctor's and onboarding's fixes,
//! onboarding's text without a terminal) and the sources of the CLI and
//! its crates, where every such command is a backticked span of a string.

use std::path::{Path, PathBuf};

/// The first words of a command a message tells a person to run.
const COMMANDS: &[&str] = &[
    "axocoatl",
    "sudo",
    "podman",
    "ollama",
    "apt-get",
    "dnf",
    "pacman",
    "zypper",
    "rpm",
    "dpkg-query",
    "usermod",
    "brew",
    "git",
    "gh",
    "wsl",
    "systemctl",
    "launchctl",
    "journalctl",
    "export",
    "unset",
    "eval",
    "curl",
    "sh",
    "cargo",
    "npm",
    "rm",
    "cd",
];

/// `command` starts with a command word, or with a variable for it
/// (`OLLAMA_HOST=… ollama serve`).
fn is_command(command: &str) -> bool {
    let Some(first) = command.split_whitespace().next() else {
        return false;
    };
    if COMMANDS.contains(&first) {
        return true;
    }
    first.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
    })
}

/// What makes `command` unsafe to paste: `a|b` (a `|` with no space on
/// either side, which a shell runs as a pipeline; `a | b` is a real one and
/// `a || b` a real or) and `[-` (an optional part). Nothing for a text that
/// is not a command.
pub(crate) fn problems(command: &str) -> Vec<&'static str> {
    if !is_command(command) {
        return Vec::new();
    }
    let chars: Vec<char> = command.chars().collect();
    let mut found = Vec::new();
    let alternative = (1..chars.len().saturating_sub(1)).any(|index| {
        chars[index] == '|'
            && !chars[index - 1].is_whitespace()
            && chars[index - 1] != '|'
            && !chars[index + 1].is_whitespace()
            && chars[index + 1] != '|'
    });
    if alternative {
        found.push("a|b alternatives, which a shell runs as a pipeline");
    }
    if command.contains("[-") {
        found.push("an optional [--flag], which a shell reads as a glob");
    }
    found
}

/// The commands in a printed `text`: its backticked spans, and its lines
/// that hold only a command, indented (as onboarding prints one to run).
pub(crate) fn commands(text: &str) -> Vec<String> {
    let mut found: Vec<String> = text
        .split('`')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect();
    found.extend(
        text.lines()
            .filter(|line| line.starts_with("  "))
            .map(|line| line.trim().to_string())
            .filter(|line| is_command(line)),
    );
    found
}

/// Panic with every command in `text` that is unsafe to paste.
pub(crate) fn assert_paste_safe(text: &str) {
    let unsafe_commands: Vec<String> = commands(text)
        .into_iter()
        .filter_map(|command| {
            let problems = problems(&command);
            (!problems.is_empty()).then(|| format!("`{command}`: {}", problems.join("; ")))
        })
        .collect();
    assert!(
        unsafe_commands.is_empty(),
        "commands unsafe to paste in {text:?}:\n{}",
        unsafe_commands.join("\n")
    );
}

/// The Rust sources under `directory`, every level down.
fn sources(directory: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            sources(&path, found);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            found.push(path);
        }
    }
}

/// The backticked spans of `source` outside comments, with each string
/// continuation (`\` at the end of a line, then the next line's
/// indentation) joined, as the program prints them; and the line each
/// starts on.
fn printed_spans(source: &str) -> Vec<(usize, String)> {
    let mut joined: Vec<(char, usize)> = Vec::new();
    let mut continuing = false;
    for (index, line) in source.lines().enumerate() {
        // Comments are not printed.
        let line = if line.trim_start().starts_with("//") {
            ""
        } else {
            line
        };
        let text = if continuing { line.trim_start() } else { line };
        continuing = text.ends_with('\\') && !text.ends_with("\\\\");
        let text = text
            .strip_suffix('\\')
            .filter(|_| continuing)
            .unwrap_or(text);
        joined.extend(text.chars().map(|c| (c, index + 1)));
        if !continuing {
            joined.push(('\n', index + 1));
        }
    }
    let mut spans = Vec::new();
    let mut open: Option<(usize, String)> = None;
    for (c, line) in joined {
        match (c, open.as_mut()) {
            ('`', None) => open = Some((line, String::new())),
            ('`', Some(_)) => spans.extend(open.take()),
            // A span never runs past the end of a line.
            ('\n', Some(_)) => open = None,
            (c, Some((_, span))) => span.push(c),
            _ => {}
        }
    }
    spans
}

#[test]
fn the_check_finds_alternatives_and_optional_parts() {
    // Built here so that the source scan below does not find them.
    let bar = '|';
    let pipeline = format!("axocoatl onboard --replace --writer claude-code{bar}codex");
    assert_eq!(
        problems(&pipeline),
        ["a|b alternatives, which a shell runs as a pipeline"]
    );
    let optional = format!("axocoatl onboard --yes [--pull{bar}--no-pull]");
    assert_eq!(problems(&optional).len(), 2);
    for safe in [
        "axocoatl onboard --replace --writer WRITER",
        "curl -fsSL https://ollama.com/install.sh | sh",
        "podman info || podman system migrate",
        "sudo apt-get install -t bookworm-backports linux-image-$(dpkg --print-architecture)",
        // Not a command: an example of a value, a field, a pattern.
        "low|medium|high|critical",
    ] {
        assert!(problems(safe).is_empty(), "{safe}");
    }
    let text = format!("Run `{pipeline}` or:\n  {optional}\n");
    assert_eq!(commands(&text), [pipeline.clone(), optional]);
    assert!(std::panic::catch_unwind(|| assert_paste_safe(&text)).is_err());
    assert_paste_safe("Write a new one with `axocoatl onboard --replace --writer WRITER`");
    // The scan joins string continuations and skips comments.
    let source = format!(
        "// `{pipeline}`\nlet text = \"or write a new one with `axocoatl \\\n    onboard \
         --writer a{bar}b` (it backs\";\n"
    );
    assert_eq!(
        printed_spans(&source),
        [(2, format!("axocoatl onboard --writer a{bar}b"))]
    );
}

/// No command in the strings of the CLI and its crates offers `a|b`
/// alternatives or `[--optional]` parts: doctor's and onboarding's fixes,
/// the daemon's readiness and refusal hints, Podman's findings, every step
/// left. (A deliberate pipeline is written with spaces, `a | b`.)
#[test]
fn no_printed_command_offers_alternatives_or_optional_parts() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    sources(&manifest.join("src"), &mut files);
    if let Ok(crates) = std::fs::read_dir(manifest.join("../crates")) {
        for entry in crates.flatten() {
            sources(&entry.path().join("src"), &mut files);
        }
    }
    files.sort();
    assert!(
        files.iter().any(|file| file.ends_with("src/doctor.rs"))
            && files
                .iter()
                .any(|file| file.ends_with("axocoatl-isolation/src/podman_report.rs")),
        "the scan read no sources: {files:?}"
    );
    let mut unsafe_commands = Vec::new();
    for file in files.iter().filter(|file| !file.ends_with("paste_safe.rs")) {
        let source = std::fs::read_to_string(file).unwrap();
        for (line, span) in printed_spans(&source) {
            let problems = problems(&span);
            if !problems.is_empty() {
                unsafe_commands.push(format!(
                    "{}:{line}: `{span}`: {}",
                    file.display(),
                    problems.join("; ")
                ));
            }
        }
    }
    assert!(
        unsafe_commands.is_empty(),
        "commands unsafe to paste:\n{}",
        unsafe_commands.join("\n")
    );
}

/// The text onboarding prints without a terminal shows commands on lines
/// of their own; each pastes as it is.
#[test]
fn the_text_without_a_terminal_pastes_as_written() {
    let text = crate::onboard::NO_TERMINAL;
    let commands = commands(text);
    assert_eq!(
        commands,
        [
            "axocoatl onboard --help",
            "axocoatl onboard --yes --writer WRITER",
            "axocoatl onboard --plan"
        ]
    );
    assert_paste_safe(text);
    assert!(text.contains("WRITER is claude-code, codex, ollama or openrouter"));
}
