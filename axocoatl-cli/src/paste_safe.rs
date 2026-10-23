//! Every command the CLI prints for a person to run can be pasted into a
//! shell as it is. A choice written as `a|b` is a pipeline there:
//! `axocoatl onboard --replace --writer claude-code|codex|ollama|openrouter`
//! would replace the configuration with a Claude Code one and pipe it into
//! `codex`, `ollama` and `openrouter`. An optional part written as
//! `[--flag]` is a glob. A choice is a placeholder in capitals instead
//! (`--writer WRITER`), with the choices in words beside the command.
//!
//! A flag's value written as choices outside a command, `--toolchain
//! node|go` or `--task-file <file|->`, is the same pipeline once a person
//! adds it to a command line: it is a placeholder too (`--toolchain
//! LANGUAGE`), with the choices in words. A placeholder is in capitals,
//! never in angle brackets, which a shell reads as a redirection.
//!
//! These checks read the printed texts (doctor's and onboarding's fixes,
//! onboarding's text without a terminal, every command's `--help`, and the
//! messages built at run time from a list of choices) and the sources of
//! the CLI and its crates: every backticked span of a string, every flag
//! written with its value, and every `|` joined into a text at run time.

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
    "dpkg-divert",
    "dpkg-statoverride",
    "setcap",
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
    if !angle_placeholders(command).is_empty() {
        found.push("a <placeholder>, which a shell reads as a redirection");
    }
    found
}

/// The `<word>` placeholders in `text`: a `<`, a letter, then letters,
/// digits, spaces, `-`, `_`, `.` or a `{format argument}` (in a source),
/// and a `>`. Pasted into a shell, `<name>` reads a file called `name>` (or
/// fails), so a printed command names a value in capitals instead (`NAME`,
/// `CONFIG`, `DISTRO`).
pub(crate) fn angle_placeholders(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find('<') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('>') else {
            break;
        };
        let inside = &after[..end];
        if let Some(inner) = inside.find('<') {
            rest = &after[inner..];
            continue;
        }
        if inside
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic())
            && inside
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || " -_.{}".contains(c))
        {
            found.push(format!("<{inside}>"));
        }
        rest = &after[end..];
    }
    found
}

/// A `|` inside `word`, with no space on either side and not part of `||`:
/// choices, which a shell runs as a pipeline.
fn has_choice_bar(word: &str) -> bool {
    let chars: Vec<char> = word.chars().collect();
    (1..chars.len().saturating_sub(1))
        .any(|index| chars[index] == '|' && chars[index - 1] != '|' && chars[index + 1] != '|')
}

/// The flags in a printed `text` whose value is written as choices, which a
/// person adds to a command line as they are: `--toolchain node|go`,
/// `--writer=a|b`, a placeholder `<file|->`. Outside backticks too.
pub(crate) fn flag_choices(text: &str) -> Vec<String> {
    let trim = |word: &str| {
        word.trim_start_matches(['`', '"', '\'', '('])
            .trim_end_matches(['`', '"', '\'', ',', '.', ';', ':', ')'])
            .to_string()
    };
    let words: Vec<String> = text.split_whitespace().map(trim).collect();
    let mut found = Vec::new();
    for (index, word) in words.iter().enumerate() {
        let flag = word.starts_with("--")
            && word[2..]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric());
        let placeholder = word
            .find('<')
            .and_then(|start| word[start..].find('>').map(|end| &word[start..start + end]))
            .is_some_and(has_choice_bar);
        if placeholder || (flag && word.contains('=') && has_choice_bar(word)) {
            found.push(word.clone());
        } else if flag && !word.contains('=') {
            // A placeholder value is found on its own, next.
            let value = words
                .get(index + 1)
                .filter(|value| !value.contains('<') && has_choice_bar(value));
            if let Some(value) = value {
                found.push(format!("{word} {value}"));
            }
        }
    }
    found
}

/// The flags in a printed `text` whose value is, or holds, an `<angle>`
/// placeholder (`--config <config>`, `--toolchain <another language>`,
/// `--setup "pip install <package>"`), outside backticks too: added to a
/// command line as written, the placeholder is a redirection, or, quoted,
/// a command nobody can run.
pub(crate) fn flag_placeholders(text: &str) -> Vec<String> {
    let words: Vec<&str> = text.split_whitespace().collect();
    let mut found = Vec::new();
    for (index, word) in words.iter().enumerate() {
        let word = word.trim_start_matches(['`', '"', '\'', '(']);
        let flag = word.starts_with("--")
            && word[2..]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric());
        if !flag {
            continue;
        }
        let value = match word.split_once('=') {
            Some((_, value)) => value.to_string(),
            None => flag_value(&words[index + 1..]),
        };
        if !angle_placeholders(&value).is_empty() {
            let value = value.trim_end_matches(['`', ',', '.', ';', ':', ')']);
            found.push(format!(
                "{} {value}",
                word.split('=').next().unwrap_or(word)
            ));
        }
    }
    found
}

/// The value written after a flag, from `words`: its first word, or, when
/// that opens a quote or an `<angle>` placeholder it does not close, the
/// words up to the one that does (at most eight).
fn flag_value(words: &[&str]) -> String {
    let Some(first) = words.first() else {
        return String::new();
    };
    let bare = first.trim_start_matches(['\\', '"', '\'']);
    let quoted = bare.len() != first.len() && !bare.contains(['"', '\'']);
    let angle = bare.starts_with('<') && !bare.contains('>');
    if !quoted && !angle {
        return first.to_string();
    }
    let mut value = vec![*first];
    for word in words.iter().skip(1).take(7) {
        value.push(word);
        if (quoted && word.contains(['"', '\''])) || (!quoted && word.contains('>')) {
            break;
        }
    }
    value.join(" ")
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

/// Panic with every command in `text` that is unsafe to paste, and every
/// flag whose value it writes as choices.
pub(crate) fn assert_paste_safe(text: &str) {
    let mut unsafe_commands: Vec<String> = commands(text)
        .into_iter()
        .filter_map(|command| {
            let problems = problems(&command);
            (!problems.is_empty()).then(|| format!("`{command}`: {}", problems.join("; ")))
        })
        .collect();
    unsafe_commands.extend(
        flag_choices(text)
            .into_iter()
            .map(|flag| format!("{flag}: a flag value written as choices")),
    );
    unsafe_commands.extend(
        flag_placeholders(text)
            .into_iter()
            .map(|flag| format!("{flag}: a flag value written as a <placeholder>")),
    );
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

/// `source` outside comments, with each string continuation (`\` at the
/// end of a line, then the next line's indentation) joined, as the program
/// prints it: each character with the line it is on.
fn joined_source(source: &str) -> Vec<(char, usize)> {
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
    joined
}

/// The lines of [`joined_source`], each with the line it starts on.
fn joined_lines(source: &str) -> Vec<(usize, String)> {
    let mut lines: Vec<(usize, String)> = Vec::new();
    let mut current: Option<(usize, String)> = None;
    for (c, line) in joined_source(source) {
        if c == '\n' {
            lines.extend(current.take());
        } else {
            current
                .get_or_insert_with(|| (line, String::new()))
                .1
                .push(c);
        }
    }
    lines.extend(current);
    lines
}

/// The backticked spans of `source` outside comments ([`joined_source`]),
/// and the line each starts on.
fn printed_spans(source: &str) -> Vec<(usize, String)> {
    let joined = joined_source(source);
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

    // A flag's value as choices, outside backticks too.
    let toolchain = format!("To fix it: pass --toolchain node{bar}python{bar}go for its image");
    assert_eq!(
        flag_choices(&toolchain),
        [format!("--toolchain node{bar}python{bar}go")]
    );
    let task_file = format!("say what to do: --task \"...\" or --task-file <file{bar}->.");
    assert_eq!(flag_choices(&task_file), [format!("<file{bar}->")]);
    let equals = format!("(`--podman=create{bar}skip`)");
    assert_eq!(flag_choices(&equals), [format!("--podman=create{bar}skip")]);
    assert!(std::panic::catch_unwind(|| assert_paste_safe(&toolchain)).is_err());
    for safe in [
        "pass --toolchain LANGUAGE, where LANGUAGE is node, python or go",
        "--task-file FILE (- reads the task from standard input)",
        "podman info || podman system migrate",
        "\"severity\": \"low|medium|high|critical\"",
        "curl -fsSL https://ollama.com/install.sh | sh",
    ] {
        assert_eq!(flag_choices(safe), Vec::<String>::new(), "{safe}");
    }
    // A `|` joined in at run time.
    let built =
        format!("let names = list.join(\"{bar}\");\nlet key = format!(\"{{a}}{bar}{{b}}\");\n");
    assert_eq!(runtime_bars(&built), [1, 2]);
    assert_eq!(
        runtime_bars("let x = a || b;\nlet y = |v| { v };\n"),
        Vec::<usize>::new()
    );
}

/// A value in angle brackets is a redirection once pasted: found in a
/// command, as a flag's value and in a literal's commands, never in a real
/// redirection (`< file`), a heredoc or an HTML tag outside a command.
#[test]
fn the_check_finds_angle_placeholders() {
    // Built here so that the source scan does not find them.
    let (open, close) = ('<', '>');
    let name = format!("{open}name{close}");
    let secret = format!("axocoatl secret set {name} --from-env VAR");
    assert_eq!(
        problems(&secret),
        ["a <placeholder>, which a shell reads as a redirection"]
    );
    assert_eq!(angle_placeholders(&secret), std::slice::from_ref(&name));
    for safe in [
        "axocoatl secret set NAME < token-file",
        "podman info 2>/dev/null",
        "cat <<EOF",
        "axocoatl secret set NAME --from-env VAR",
    ] {
        assert!(problems(safe).is_empty(), "{safe}");
    }
    let config = format!("Pass --config {open}config{close}, or --check \"{open}command{close}\".");
    assert_eq!(
        flag_placeholders(&config),
        [
            format!("--config {open}config{close}"),
            format!("--check \"{open}command{close}\""),
        ]
    );
    assert!(flag_placeholders("--toolchain LANGUAGE, where LANGUAGE is go").is_empty());
    // A placeholder of several words, or inside a quoted value (with a
    // format argument, as a source writes it), is found too.
    let words = format!(
        "pass --toolchain {open}another language{close} when it runs another one, or --setup \
         \"pip install {open}package{close}\", or --setup \\\"{open}command that installs \
         {{program}}{close}\\\"; or --check \"npm test\" < input"
    );
    assert_eq!(
        flag_placeholders(&words),
        [
            format!("--toolchain {open}another language{close}"),
            format!("--setup \"pip install {open}package{close}\""),
            format!("--setup \\\"{open}command that installs {{program}}{close}\\\""),
        ]
    );
    assert!(flag_placeholders(
        "--setup \"pip install PACKAGE\", or --setup \"COMMAND\", where COMMAND installs it"
    )
    .is_empty());
    let literal = format!("Build one with: axocoatl recipe build {open}recipe{close}. Then run it");
    assert_eq!(
        literal_commands(&literal),
        [format!("axocoatl recipe build {open}recipe{close}")]
    );
    // A command inside an HTML tag is not read; a closing tag is no
    // placeholder.
    assert!(
        literal_commands("<h1>Signed in</h1><p>run <code>axocoatl onboard</code> again</p>")
            .is_empty()
    );
    assert!(angle_placeholders("axocoatl onboard</code> again</p>").is_empty());
    assert!(std::panic::catch_unwind(|| assert_paste_safe(&format!("Run `{secret}`"))).is_err());
}

/// The string literals in `line`, without their quotes (escapes kept).
fn string_literals(line: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut current: Option<String> = None;
    let mut escaped = false;
    let mut previous = ' ';
    for c in line.chars() {
        match current.as_mut() {
            Some(text) if escaped => {
                text.push(c);
                escaped = false;
            }
            Some(text) if c == '\\' => {
                text.push(c);
                escaped = true;
            }
            Some(_) if c == '"' => found.extend(current.take()),
            Some(text) => text.push(c),
            // Not the `"` of a char literal (`'"'`).
            None if c == '"' && previous != '\'' => current = Some(String::new()),
            None => {}
        }
        previous = c;
    }
    found
}

/// The lines of `source` ([`joined_lines`]) that join a `|` into a text at
/// run time: `.join("|")`, or a format string with `|` beside a
/// placeholder (`"{a}|{b}"`).
fn runtime_bars(source: &str) -> Vec<usize> {
    joined_lines(source)
        .into_iter()
        .filter(|(_, line)| {
            line.contains(".join(\"|\")")
                || line.contains(".join('|')")
                || string_literals(line)
                    .iter()
                    .any(|literal| literal.contains("}|") || literal.contains("|{"))
        })
        .map(|(number, _)| number)
        .collect()
}

/// The sources of the CLI and of every crate, but this file.
fn all_sources() -> Vec<PathBuf> {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    sources(&manifest.join("src"), &mut files);
    if let Ok(crates) = std::fs::read_dir(manifest.join("../crates")) {
        for entry in crates.flatten() {
            sources(&entry.path().join("src"), &mut files);
        }
    }
    files.retain(|file| !file.ends_with("paste_safe.rs"));
    files.sort();
    assert!(
        files.iter().any(|file| file.ends_with("src/doctor.rs"))
            && files
                .iter()
                .any(|file| file.ends_with("axocoatl-session/src/toolchain.rs")),
        "the scan read no sources: {files:?}"
    );
    files
}

/// The texts that join a `|` in at run time and are no command or flag a
/// person pastes, each with why.
const RUNTIME_BARS: &[(&str, &str)] = &[
    (
        "axocoatl-daemon/src/egress_broker/rules.rs",
        "an egress rule's methods (`GET|HEAD /path`), as a loadout's egress rules are written",
    ),
    (
        "axocoatl-daemon/src/trajectory.rs",
        "the key that tells repeated tool calls apart, never printed",
    ),
    (
        "axocoatl-config/src/yaml/ser.rs",
        "a YAML block scalar's header (`|-`)",
    ),
];

/// No string in the CLI and its crates writes a flag's value as choices
/// (`--toolchain node|go`, `<file|->`), backticked or not, and no text
/// joins a `|` in at run time but the ones [`RUNTIME_BARS`] names: a list
/// of choices built at run time is said in words beside a placeholder.
#[test]
fn no_printed_flag_offers_its_value_as_choices() {
    let mut found = Vec::new();
    for file in all_sources() {
        let source = std::fs::read_to_string(&file).unwrap();
        for (line, text) in joined_lines(&source) {
            for flag in flag_choices(&text) {
                found.push(format!("{}:{line}: {flag}", file.display()));
            }
        }
        let allowed = RUNTIME_BARS.iter().any(|(path, _)| file.ends_with(path));
        if !allowed {
            for line in runtime_bars(&source) {
                found.push(format!(
                    "{}:{line}: a `|` joined in at run time",
                    file.display()
                ));
            }
        }
    }
    assert!(
        found.is_empty(),
        "choices that paste as a pipeline:\n{}",
        found.join("\n")
    );
}

/// The commands a string literal `text` prints, each from its first word
/// (one of [`COMMANDS`]) to the end of its clause: a backtick, a line
/// break (`\n` in the literal), `;` or the end of a sentence.
fn literal_commands(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    for (start, _) in text.char_indices() {
        let before = &text[..start];
        let boundary = before.is_empty()
            || before.ends_with("\\n")
            || before.ends_with([' ', '`', '"', '(', ':']);
        if !boundary {
            continue;
        }
        let rest = &text[start..];
        let Some(word) = rest
            .split(' ')
            .next()
            .filter(|word| COMMANDS.contains(word))
        else {
            continue;
        };
        if rest.len() == word.len() {
            continue;
        }
        let end = ["`", "\\n", ";", ". "]
            .iter()
            .filter_map(|stop| rest.find(stop))
            .min()
            .unwrap_or(rest.len());
        found.push(rest[..end].to_string());
    }
    found
}

/// The sources whose `<angle>` placeholders are no command Axocoatl tells a
/// person to run, each with why.
const ANGLE_PLACEHOLDERS: &[(&str, &str)] = &[
    (
        "axocoatl-exec/src/main.rs",
        "the internal supervisor's usage synopsis, which no person runs (a byte changed there \
         changes the embedded supervisor's hash)",
    ),
    (
        "axocoatl-service/src/launchd.rs",
        "a test's CONTAINER_CONNECTION value with shell metacharacters, checked for quoting",
    ),
    (
        "axocoatl-cli/src/connect_cmd/ink_model.rs",
        "Claude Code's own screen, which the sign-in tests model and Axocoatl never prints",
    ),
    (
        "axocoatl-cli/src/connect_cmd/redact/tests.rs",
        "Claude Code's own screen, which the redaction tests feed in",
    ),
];

/// No string in the CLI and its crates names a value in a printed command
/// as an `<angle>` placeholder, which a shell reads as a redirection: not in
/// a command (`axocoatl secret set <name>`, backticked or not) nor as a
/// flag's value written beside one (`--config <config>`). A value to fill
/// in is a placeholder in capitals (`NAME`, `CONFIG`, `DISTRO`).
#[test]
fn no_printed_command_names_a_value_in_angle_brackets() {
    let mut found = Vec::new();
    for file in all_sources() {
        if ANGLE_PLACEHOLDERS
            .iter()
            .any(|(path, _)| file.ends_with(path))
        {
            continue;
        }
        let source = std::fs::read_to_string(&file).unwrap();
        for (line, text) in joined_lines(&source) {
            for literal in string_literals(&text) {
                for flag in flag_placeholders(&literal) {
                    found.push(format!("{}:{line}: {flag}", file.display()));
                }
                for command in literal_commands(&literal) {
                    if !angle_placeholders(&command).is_empty() {
                        found.push(format!("{}:{line}: {command}", file.display()));
                    }
                }
            }
        }
    }
    found.dedup();
    assert!(
        found.is_empty(),
        "commands with <placeholders> that paste as redirections:\n{}",
        found.join("\n")
    );
}

/// The usage errors built from the repository's toolchains, and the probe
/// refusal's fixes, name the choices in words.
#[test]
fn messages_built_from_a_list_of_choices_name_them_in_words() {
    use axocoatl_daemon::loadout::toolchain::{
        plan, probe_refusal, EnvironmentRequest, PlanInput, ProbeMissing,
    };
    use axocoatl_session::toolchain::{ambiguous_message, Candidate, Toolchain};
    let candidates = [
        (Toolchain::Node, "package.json"),
        (Toolchain::Python, "pyproject.toml"),
        (Toolchain::Go, "go.mod"),
    ]
    .map(|(toolchain, file)| Candidate {
        toolchain,
        files: vec![file.to_string()],
    });
    for count in 2..=3 {
        let message = ambiguous_message(&candidates[..count], Some("make test"));
        assert_paste_safe(&message);
        assert!(!message.contains('|'), "{message}");
    }
    assert!(ambiguous_message(&candidates, None)
        .contains("Pass --toolchain LANGUAGE, where LANGUAGE is node, python or go"));

    let repository = tempfile::tempdir().unwrap();
    std::fs::write(
        repository.path().join("package.json"),
        r#"{"scripts":{"test":"jest"}}"#,
    )
    .unwrap();
    let loadout = axocoatl_config::loadout::builtin_loadouts()
        .into_iter()
        .map(|loadout| loadout.unwrap())
        .find(|loadout| loadout.file.id == "fix")
        .unwrap();
    let request = EnvironmentRequest {
        toolchain: Some("none".into()),
        ..Default::default()
    };
    let plan = plan(
        repository.path(),
        PlanInput {
            file: &loadout.file,
            request: &request,
            check_flag: Some("jest"),
            setup_flag: None,
            loadout_setup: None,
        },
    )
    .unwrap();
    let message = probe_refusal(&plan, "jest", &ProbeMissing::Program("jest".into()), None);
    assert!(
        message.contains(
            "pass --toolchain LANGUAGE for Axocoatl's image of the repository's language, \
             where LANGUAGE is node, python, rust or go"
        ),
        "{message}"
    );
    assert_paste_safe(&message);
}

/// Every command's `--help`, clap's text built from the flags' doc
/// comments, pastes as written.
#[test]
fn every_help_pastes_as_written() {
    use clap::CommandFactory;
    fn walk(command: &mut clap::Command, name: String, found: &mut Vec<String>, count: &mut usize) {
        *count += 1;
        let help = command.render_long_help().to_string();
        for flag in flag_choices(&help) {
            found.push(format!("{name}: {flag}"));
        }
        for command in commands(&help) {
            let problems = problems(&command);
            if !problems.is_empty() {
                found.push(format!("{name}: `{command}`: {}", problems.join("; ")));
            }
        }
        for sub in command.get_subcommands_mut() {
            let name = format!("{name} {}", sub.get_name());
            walk(sub, name, found, count);
        }
    }
    let mut found = Vec::new();
    let mut count = 0;
    walk(
        &mut crate::Cli::command(),
        "axocoatl".into(),
        &mut found,
        &mut count,
    );
    assert!(count > 40, "only {count} commands read");
    assert!(
        found.is_empty(),
        "help unsafe to paste:\n{}",
        found.join("\n")
    );
}

/// No command in the strings of the CLI and its crates offers `a|b`
/// alternatives or `[--optional]` parts: doctor's and onboarding's fixes,
/// the daemon's readiness and refusal hints, Podman's findings, every step
/// left. (A deliberate pipeline is written with spaces, `a | b`.)
#[test]
fn no_printed_command_offers_alternatives_or_optional_parts() {
    let files = all_sources();
    assert!(
        files
            .iter()
            .any(|file| file.ends_with("axocoatl-isolation/src/podman_report.rs")),
        "the scan read no sources: {files:?}"
    );
    let mut unsafe_commands = Vec::new();
    for file in &files {
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
