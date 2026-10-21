//! Repository toolchains: which language a repository is written in, as the
//! host reads it, and what a loadout run needs to run the repository's own
//! check in it: the check command, the command that installs the
//! repository's dependencies, and the package registries that command may
//! reach while it runs.
//!
//! Everything here reads a few files at the repository root through a
//! [`SecureDir`] (no symlink is followed, every read is bounded) and never
//! runs repository code. Owner: workstream `toolchain`.
//!
//! A toolchain is chosen from the files the repository has at its root:
//!
//! | toolchain | files |
//! |---|---|
//! | `node` | `package.json` |
//! | `python` | `pyproject.toml`, `setup.py`, `setup.cfg`, `requirements*.txt`, `pytest.ini` |
//! | `rust` | `Cargo.toml` |
//! | `go` | `go.mod` |
//!
//! When the repository has the files of several, the one the check
//! command's program belongs to is chosen (`pytest` is python, `cargo` rust,
//! `go` go, `npm`, `npx`, `node`, `pnpm` or `yarn` node); when the check does
//! not say, the run is refused and the person names one ([`Choice`]).

use axocoatl_core::{SecureDir, SecureEntryType};
use sha2::{Digest, Sha256};

/// Most bytes read from one manifest, lockfile or `AXOCOATL.md`.
pub const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
/// Most bytes of a lockfile hashed into the setup key; a larger one is
/// hashed by its first bytes and its length.
const MAX_LOCKFILE_BYTES: usize = 16 * 1024 * 1024;
/// Most root entries scanned for `requirements*.txt`.
const MAX_ROOT_ENTRIES: usize = 16 * 1024;

/// A language toolchain Axocoatl has an image recipe for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Toolchain {
    Node,
    Python,
    Rust,
    Go,
}

impl Toolchain {
    /// Every toolchain, in the order candidates are listed.
    pub const ALL: [Toolchain; 4] = [
        Toolchain::Node,
        Toolchain::Python,
        Toolchain::Rust,
        Toolchain::Go,
    ];

    /// The toolchain's name, which is also its image recipe's name.
    pub fn name(self) -> &'static str {
        match self {
            Toolchain::Node => "node",
            Toolchain::Python => "python",
            Toolchain::Rust => "rust",
            Toolchain::Go => "go",
        }
    }

    /// The toolchain named `name`.
    pub fn parse(name: &str) -> Option<Toolchain> {
        Self::ALL
            .into_iter()
            .find(|toolchain| toolchain.name() == name)
    }

    /// The language, for messages.
    pub fn language(self) -> &'static str {
        match self {
            Toolchain::Node => "Node.js",
            Toolchain::Python => "Python",
            Toolchain::Rust => "Rust",
            Toolchain::Go => "Go",
        }
    }

    /// The egress presets (`sandbox.egress.allow` names) whose hosts the
    /// dependency install may reach while it runs, and only then.
    pub fn registry_presets(self) -> &'static [&'static str] {
        match self {
            Toolchain::Node => &["npm"],
            Toolchain::Python => &["pypi"],
            Toolchain::Rust => &["crates"],
            Toolchain::Go => &["go"],
        }
    }

    /// Whether the dependency install keeps a stamp in the Workspace's
    /// cache and is skipped when nothing it installs from has changed. Node
    /// installs into the Session's own `node_modules` volume, so it runs
    /// every time (from the cache, without the network, once the cache
    /// holds the packages).
    pub fn keeps_setup_stamp(self) -> bool {
        !matches!(self, Toolchain::Node)
    }
}

/// The toolchain a program belongs to, when it says.
pub fn toolchain_for_program(program: &str) -> Option<Toolchain> {
    let program = program.rsplit('/').next().unwrap_or(program);
    let python = program == "python"
        || program == "python3"
        || program
            .strip_prefix("python3.")
            .is_some_and(|minor| !minor.is_empty() && minor.bytes().all(|b| b.is_ascii_digit()));
    match program {
        _ if python => Some(Toolchain::Python),
        "pytest" | "py.test" | "tox" | "nox" | "pip" | "pip3" | "coverage" | "uv" | "poetry"
        | "hatch" => Some(Toolchain::Python),
        "cargo" | "rustc" | "cargo-nextest" => Some(Toolchain::Rust),
        "go" | "gotestsum" => Some(Toolchain::Go),
        "npm" | "npx" | "node" | "pnpm" | "pnpx" | "yarn" | "corepack" | "jest" | "vitest"
        | "mocha" | "tsc" | "bun" => Some(Toolchain::Node),
        _ => None,
    }
}

/// The program a check command runs first: the first word that is not an
/// environment assignment or `env`/`exec`/`time`/`command`, past a leading
/// `cd <dir> &&` (or `export`/`set`/`.`), and for `sh -c '<command>'` the
/// program of that command. `None` when the shell decides it at run time: a
/// subshell or group (`(cd web && npm test)`), a keyword (`if`, `for`), or a
/// word with a parameter or command substitution (`$PYTHON -m pytest`).
pub fn check_program(command: &str) -> Option<String> {
    let mut rest = command;
    loop {
        let (first, next) = split_first_command(rest);
        let words = shell_words(first);
        let mut words = words.iter().map(String::as_str).peekable();
        let mut program = None;
        while let Some(word) = words.next() {
            if is_assignment(word) || matches!(word, "env" | "exec" | "time" | "command" | "--") {
                continue;
            }
            if word.starts_with(['(', '{', '!'])
                || word.contains(['$', '`'])
                || matches!(
                    word,
                    "if" | "then" | "for" | "while" | "until" | "case" | "do" | "function" | "[["
                )
            {
                return None;
            }
            let name = word.rsplit('/').next().unwrap_or(word);
            if matches!(name, "sh" | "bash" | "dash") && words.peek() == Some(&"-c") {
                words.next();
                return words.next().and_then(check_program);
            }
            program = Some(name.to_string());
            break;
        }
        match (program, next) {
            // `cd web && npm test`: the program is the next command's.
            (Some(program), Some(next))
                if matches!(
                    program.as_str(),
                    "cd" | "export" | "set" | "." | "source" | "unset" | "umask"
                ) =>
            {
                rest = next;
            }
            (program, _) => return program,
        }
    }
}

/// The first simple command of a command line, and what follows it when it
/// ends with `&&` or `;` (after `|`, `||` or `&` nothing is followed).
fn split_first_command(command: &str) -> (&str, Option<&str>) {
    let mut quote: Option<char> = None;
    for (at, c) in command.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"') => quote = Some(c),
            (None, ';') => return (&command[..at], Some(&command[at + 1..])),
            (None, '&') if command[at..].starts_with("&&") => {
                return (&command[..at], Some(&command[at + 2..]))
            }
            (None, '&' | '|') => return (&command[..at], None),
            _ => {}
        }
    }
    (command, None)
}

fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && name
                .bytes()
                .enumerate()
                .all(|(i, b)| b == b'_' || b.is_ascii_alphabetic() || (i > 0 && b.is_ascii_digit()))
    })
}

/// Words of a command line, with simple single and double quotes removed.
/// Enough to find a program; never used to run anything.
fn shell_words(command: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    for c in command.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => word.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                started = true;
            }
            (None, c) if c.is_whitespace() => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            (None, ';' | '&' | '|') => {
                if started {
                    words.push(std::mem::take(&mut word));
                }
                break;
            }
            (None, c) => {
                word.push(c);
                started = true;
            }
        }
    }
    if started {
        words.push(word);
    }
    words
}

/// Quote `value` for a POSIX shell when it needs it.
pub fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./=:@,+%".contains(&b))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

/// One toolchain the repository has the files of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub toolchain: Toolchain,
    /// The root files that name it, such as `package.json`.
    pub files: Vec<String>,
}

/// What a repository root holds, read once.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RootFiles {
    files: Vec<String>,
    dirs: Vec<String>,
}

impl RootFiles {
    /// Read the names of the regular files and directories at `dir`'s root.
    pub fn read(dir: &SecureDir) -> std::io::Result<Self> {
        let mut root = RootFiles::default();
        for entry in dir.entries_limited(MAX_ROOT_ENTRIES)? {
            let Some(name) = entry.name.to_str() else {
                continue;
            };
            match entry.file_type {
                SecureEntryType::File => root.files.push(name.to_string()),
                SecureEntryType::Directory => root.dirs.push(name.to_string()),
                SecureEntryType::Other => {}
            }
        }
        root.files.sort();
        root.dirs.sort();
        Ok(root)
    }

    pub fn has(&self, name: &str) -> bool {
        self.files.iter().any(|file| file == name)
    }

    pub fn has_dir(&self, name: &str) -> bool {
        self.dirs.iter().any(|dir| dir == name)
    }

    /// `requirements*.txt` and `*-requirements.txt` / `*_requirements.txt`
    /// at the root, sorted.
    pub fn requirements_files(&self) -> Vec<String> {
        self.files
            .iter()
            .filter(|name| {
                name.ends_with(".txt")
                    && (name.starts_with("requirements")
                        || name.ends_with("-requirements.txt")
                        || name.ends_with("_requirements.txt"))
            })
            .cloned()
            .collect()
    }
}

/// The toolchains whose files the repository root holds, in [`Toolchain::ALL`]
/// order.
pub fn candidates(root: &RootFiles) -> Vec<Candidate> {
    let mut candidates = Vec::new();
    let mut add = |toolchain, files: Vec<String>| {
        if !files.is_empty() {
            candidates.push(Candidate { toolchain, files });
        }
    };
    let present = |names: &[&str]| -> Vec<String> {
        names
            .iter()
            .filter(|name| root.has(name))
            .map(|name| name.to_string())
            .collect()
    };
    add(Toolchain::Node, present(&["package.json"]));
    let mut python = present(&["pyproject.toml", "setup.py", "setup.cfg"]);
    python.extend(root.requirements_files());
    python.extend(present(&["pytest.ini"]));
    add(Toolchain::Python, python);
    add(Toolchain::Rust, present(&["Cargo.toml"]));
    add(Toolchain::Go, present(&["go.mod"]));
    candidates
}

/// The toolchain a run uses, or why it cannot choose one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Choice {
    /// One toolchain, and why.
    Chosen {
        toolchain: Toolchain,
        reason: String,
    },
    /// No toolchain's files at the root.
    Nothing { reason: String },
    /// The files of several, and the check does not say which it needs.
    Ambiguous { candidates: Vec<Candidate> },
}

impl Choice {
    /// The chosen toolchain, if one was.
    pub fn toolchain(&self) -> Option<Toolchain> {
        match self {
            Choice::Chosen { toolchain, .. } => Some(*toolchain),
            _ => None,
        }
    }
}

/// The module `python -m <module>` (or `python3 -m`) runs in a check:
/// `pytest` for `python3 -m pytest -q`.
pub fn python_module_of(check: &str) -> Option<String> {
    let words = shell_words(split_first_command(check).0);
    let program = check_program(check)?;
    if toolchain_for_program(&program) != Some(Toolchain::Python) || !program.starts_with("python")
    {
        return None;
    }
    let at = words.iter().position(|word| word == "-m")?;
    words.get(at + 1).cloned()
}

fn files_text(files: &[String]) -> String {
    files.join(", ")
}

/// Choose among `candidates` with the check command the run will use.
pub fn choose(candidates: &[Candidate], check: Option<&str>) -> Choice {
    match candidates {
        [] => Choice::Nothing {
            reason: "the repository root has no package.json, pyproject.toml, setup.py, \
                     setup.cfg, requirements*.txt, Cargo.toml or go.mod"
                .into(),
        },
        [one] => Choice::Chosen {
            toolchain: one.toolchain,
            reason: format!("{} at the repository root", files_text(&one.files)),
        },
        several => {
            let program = check.and_then(check_program);
            let wanted = program.as_deref().and_then(toolchain_for_program);
            match several
                .iter()
                .find(|candidate| Some(candidate.toolchain) == wanted)
            {
                Some(candidate) => Choice::Chosen {
                    toolchain: candidate.toolchain,
                    reason: format!(
                        "{} at the repository root, and the check runs {} ({})",
                        files_text(&candidate.files),
                        program.as_deref().unwrap_or_default(),
                        candidate.toolchain.language()
                    ),
                },
                None => Choice::Ambiguous {
                    candidates: several.to_vec(),
                },
            }
        }
    }
}

/// The usage error of an [`Choice::Ambiguous`] repository.
pub fn ambiguous_message(candidates: &[Candidate], check: Option<&str>) -> String {
    let found = candidates
        .iter()
        .map(|candidate| {
            format!(
                "{} ({})",
                files_text(&candidate.files),
                candidate.toolchain.name()
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    // The choices in words beside a placeholder: `node|go` pasted into a
    // shell is a pipeline.
    let names: Vec<&str> = candidates
        .iter()
        .map(|candidate| candidate.toolchain.name())
        .collect();
    let names = match names.split_last() {
        Some((last, [])) => last.to_string(),
        Some((last, rest)) => format!("{} or {last}", rest.join(", ")),
        None => String::new(),
    };
    let check = match check {
        Some(check) => format!("the check `{check}` does not say which one it needs"),
        None => "no check command was found to say which one it needs".into(),
    };
    format!(
        "the repository has the files of several toolchains: {found}, and {check}. Pass \
         --toolchain <language>, where <language> is {names} (or --toolchain none for the plain \
         image), or give the check with --check \"<command>\""
    )
}

/// The `Check command:` line of the repository root's `AXOCOATL.md`, when
/// it has one: `Check command: pytest -q`, also as a list item or in bold,
/// with the command in backticks, or on the next line (in a code fence).
pub fn axocoatl_md_check(dir: &SecureDir) -> Option<String> {
    if !dir.is_file("AXOCOATL.md").unwrap_or(false) {
        return None;
    }
    let bytes = dir.read_limited("AXOCOATL.md", MAX_MANIFEST_BYTES).ok()?;
    check_line(&String::from_utf8_lossy(&bytes))
}

/// The command of the first `Check command:` line of `text`.
pub fn check_line(text: &str) -> Option<String> {
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        let trimmed = line
            .trim()
            .trim_start_matches(['-', '*', '#', '>', ' ', '\t'])
            .trim_start_matches("**")
            .trim_start();
        let Some(prefix) = trimmed.get(..13) else {
            continue;
        };
        if !prefix.eq_ignore_ascii_case("check command") {
            continue;
        }
        let rest = trimmed[13..]
            .trim_start()
            .trim_start_matches("**")
            .trim_start();
        let Some(rest) = rest.strip_prefix(':') else {
            continue;
        };
        let rest = rest.trim().trim_start_matches("**").trim();
        let command = if rest.is_empty() {
            lines
                .by_ref()
                .map(str::trim)
                .find(|line| {
                    !line.is_empty() && !line.starts_with("```") && !line.starts_with("~~~")
                })
                .map(str::to_string)
        } else {
            Some(rest.to_string())
        };
        let command = command?;
        let command = command.trim().trim_matches('`').trim().to_string();
        if command.is_empty() || command.starts_with('#') {
            return None;
        }
        return Some(command);
    }
    None
}

fn read_text(dir: &SecureDir, name: &str) -> Option<String> {
    if !dir.is_file(name).unwrap_or(false) {
        return None;
    }
    dir.read_limited(name, MAX_MANIFEST_BYTES)
        .ok()
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

/// Whether the repository says it uses pytest: a pytest config file or a
/// `conftest.py`, or `pytest` named in a manifest or requirements file.
pub fn declares_pytest(dir: &SecureDir, root: &RootFiles) -> bool {
    if root.has("pytest.ini") || root.has("conftest.py") {
        return true;
    }
    if dir.is_file("tests/conftest.py").unwrap_or(false)
        || dir.is_file("test/conftest.py").unwrap_or(false)
    {
        return true;
    }
    let mut sources = vec![
        "pyproject.toml".to_string(),
        "setup.cfg".into(),
        "setup.py".into(),
        "tox.ini".into(),
    ];
    sources.extend(root.requirements_files());
    sources
        .iter()
        .filter_map(|name| read_text(dir, name))
        .any(|text| text.to_ascii_lowercase().contains("pytest"))
}

/// The repository's check command, as a run detects it, and where it came
/// from: a `Check command:` line in `AXOCOATL.md`, else a `check`, `test`,
/// `ci` or `build` script of `package.json` (`npm run <script>`), else
/// `cargo test`, `go test ./...`, for Python `pytest` when the repository
/// uses pytest and `python3 -m unittest discover -v` when it does not, else
/// `make test`.
pub fn detect_check(dir: &SecureDir) -> Option<(String, CheckSource)> {
    if let Some(command) = axocoatl_md_check(dir) {
        return Some((command, CheckSource::AxocoatlMd));
    }
    let root = RootFiles::read(dir).ok()?;
    detect_check_in(dir, &root).map(|command| (command, CheckSource::Detected))
}

/// Where a run's check command came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckSource {
    /// `--check`.
    Flag,
    /// A `Check command:` line of the root `AXOCOATL.md`.
    AxocoatlMd,
    /// The repository's files.
    Detected,
}

impl CheckSource {
    pub fn describe(self) -> &'static str {
        match self {
            CheckSource::Flag => "--check",
            CheckSource::AxocoatlMd => "the Check command: line of AXOCOATL.md",
            CheckSource::Detected => "detected from the repository",
        }
    }
}

const MAX_PACKAGE_JSON_BYTES: usize = 1024 * 1024;

fn package_json(dir: &SecureDir) -> Option<serde_json::Value> {
    if !dir.is_file("package.json").unwrap_or(false) {
        return None;
    }
    let bytes = dir
        .read_limited("package.json", MAX_PACKAGE_JSON_BYTES)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// [`detect_check`] without `AXOCOATL.md`.
pub fn detect_check_in(dir: &SecureDir, root: &RootFiles) -> Option<String> {
    if let Some(package) = package_json(dir) {
        if let Some(scripts) = package.get("scripts").and_then(|s| s.as_object()) {
            for name in ["check", "test", "ci", "build"] {
                if scripts.contains_key(name) {
                    return Some(format!("npm run {name}"));
                }
            }
        }
    }
    if root.has("Cargo.toml") {
        return Some("cargo test".into());
    }
    if root.has("go.mod") {
        return Some("go test ./...".into());
    }
    if has_python_files(root) {
        return Some(python_check(dir, root));
    }
    if root.has("Makefile") || root.has("makefile") {
        return Some("make test".into());
    }
    None
}

/// Whether the repository root has a Python project's files.
pub fn has_python_files(root: &RootFiles) -> bool {
    ["pyproject.toml", "setup.py", "setup.cfg", "pytest.ini"]
        .iter()
        .any(|name| root.has(name))
        || !root.requirements_files().is_empty()
}

/// A Python repository's check: `pytest` when it uses pytest (it says so,
/// or its test files are plain `def test_...` functions, which only pytest
/// runs), else the standard library's `unittest` (G84: a repository whose
/// tests are `unittest.TestCase`s and that never mentions pytest is not
/// given a check it cannot run).
pub fn python_check(dir: &SecureDir, root: &RootFiles) -> String {
    // `python3 -m` puts the repository root on the import path, so tests
    // import a module at the root (`from pages import ...`) whether or not
    // the project is installed; the bare `pytest` script does not.
    if declares_pytest(dir, root)
        || python_test_style(dir, &python_test_files(dir, root)) == PythonTests::Pytest
    {
        return "python3 -m pytest".into();
    }
    // A tests directory that is not a package is discovered from itself.
    let tests_dir = root.has_dir("tests") && !dir.is_file("tests/__init__.py").unwrap_or(true);
    if tests_dir {
        "python3 -m unittest discover -v -s tests".into()
    } else {
        "python3 -m unittest discover -v".into()
    }
}

/// Most test files listed, and read to tell pytest-style tests from
/// unittest ones.
const MAX_TEST_FILES: usize = 64;
/// Most bytes read of one test file.
const MAX_TEST_FILE_BYTES: usize = 256 * 1024;
/// Most entries of one directory looked at while finding test files.
const MAX_TEST_SCAN_ENTRIES: usize = 4096;
/// Most directories opened while finding test files.
const MAX_TEST_SCAN_DIRS: usize = 256;
/// Deepest directory level (below the root) searched for test files.
const MAX_TEST_SCAN_DEPTH: usize = 4;

/// Directories never searched for test files: dependencies, build output,
/// virtual environments and version control.
const SKIPPED_DIRS: [&str; 11] = [
    "node_modules",
    "vendor",
    "target",
    "dist",
    "build",
    "venv",
    "site-packages",
    "__pycache__",
    "testdata",
    "coverage",
    "out",
];

/// The repository's test files for `toolchain` (relative paths, sorted, at
/// most [`MAX_TEST_FILES`]): Python's `test_*.py` and `*_test.py`, Node's
/// `*.test.*` and `*.spec.*` scripts and the scripts of a `test/`, `tests/`
/// or `__tests__/` directory, Go's `*_test.go`. Rust keeps tests in its
/// sources, so none are listed for it. Hidden directories and dependency
/// or build directories are not searched.
pub fn test_files(dir: &SecureDir, toolchain: Toolchain) -> Vec<String> {
    let matches: fn(&str, &str) -> bool = match toolchain {
        Toolchain::Python => |_, name| {
            name.ends_with(".py") && (name.starts_with("test_") || name.ends_with("_test.py"))
        },
        Toolchain::Node => |path, name| {
            let script = [".js", ".cjs", ".mjs", ".ts", ".cts", ".mts", ".jsx", ".tsx"]
                .iter()
                .any(|extension| name.ends_with(extension));
            script
                && (name.contains(".test.")
                    || name.contains(".spec.")
                    || path
                        .split('/')
                        .any(|part| matches!(part, "test" | "tests" | "__tests__")))
        },
        Toolchain::Go => |_, name| name.ends_with("_test.go"),
        Toolchain::Rust => return Vec::new(),
    };
    let mut found = Vec::new();
    let mut queue: std::collections::VecDeque<(String, usize)> =
        std::collections::VecDeque::from([(String::new(), 0)]);
    let mut opened = 0;
    while let Some((relative, depth)) = queue.pop_front() {
        if opened >= MAX_TEST_SCAN_DIRS || found.len() >= MAX_TEST_FILES {
            break;
        }
        opened += 1;
        let child;
        let directory = if relative.is_empty() {
            dir
        } else {
            match dir.existing_child(&relative) {
                Ok(opened) => {
                    child = opened;
                    &child
                }
                Err(_) => continue,
            }
        };
        let Ok(mut entries) = directory.entries_limited(MAX_TEST_SCAN_ENTRIES) else {
            continue;
        };
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        for entry in entries {
            let Some(name) = entry.name.to_str() else {
                continue;
            };
            let path = if relative.is_empty() {
                name.to_string()
            } else {
                format!("{relative}/{name}")
            };
            match entry.file_type {
                SecureEntryType::File if matches(&path, name) => {
                    found.push(path);
                    if found.len() >= MAX_TEST_FILES {
                        break;
                    }
                }
                SecureEntryType::Directory
                    if depth < MAX_TEST_SCAN_DEPTH
                        && !name.starts_with('.')
                        && !SKIPPED_DIRS.contains(&name) =>
                {
                    queue.push_back((path, depth + 1));
                }
                _ => {}
            }
        }
    }
    found.sort();
    found
}

/// [`test_files`] of a Python repository.
pub fn python_test_files(dir: &SecureDir, _root: &RootFiles) -> Vec<String> {
    test_files(dir, Toolchain::Python)
}

/// Which runner a Python repository's tests are written for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PythonTests {
    /// Plain `def test_...` functions (or `import pytest`): only pytest
    /// runs them.
    Pytest,
    /// Every test file uses `unittest`: the standard library runs them.
    Unittest,
    /// No test file, or none that says.
    Unknown,
}

/// Which runner the test files `files` (from [`test_files`]) are written
/// for.
pub fn python_test_style(dir: &SecureDir, files: &[String]) -> PythonTests {
    let mut unittest = false;
    let mut plain = false;
    for file in files.iter().take(MAX_TEST_FILES) {
        let Ok(bytes) = dir.read_limited(file, MAX_TEST_FILE_BYTES) else {
            continue;
        };
        let text = String::from_utf8_lossy(&bytes);
        if text.contains("import pytest") || text.contains("from pytest") {
            return PythonTests::Pytest;
        }
        if text.contains("unittest") || text.contains("TestCase") {
            unittest = true;
        } else if text.contains("def test") {
            plain = true;
        }
    }
    match (plain, unittest) {
        (true, _) => PythonTests::Pytest,
        (false, true) => PythonTests::Unittest,
        (false, false) => PythonTests::Unknown,
    }
}

/// The program the `package.json` script a check runs starts: for
/// `npm test`, `npm run check`, `yarn test` or `pnpm run ci`, the first
/// program of that script (`mocha` for `"test": "mocha --exit"`).
pub fn package_script_program(dir: &SecureDir, check: &str) -> Option<String> {
    let words = shell_words(split_first_command(check).0);
    let mut words = words.iter().map(String::as_str);
    let manager = words.next()?;
    let manager = manager.rsplit('/').next().unwrap_or(manager);
    if !matches!(manager, "npm" | "yarn" | "pnpm") {
        return None;
    }
    let script = match words.next()? {
        "run" | "run-script" => words.next()?,
        "t" | "tst" if manager == "npm" => "test",
        word => word,
    };
    let package = package_json(dir)?;
    let command = package.get("scripts")?.get(script)?.as_str()?;
    check_program(command)
}

/// The names (lower case, `_` and `.` as `-`) of the Python packages the
/// repository declares anywhere: its dependencies, extras and groups in
/// `pyproject.toml` (PEP 621, PEP 735, Poetry, uv, PDM), `setup.cfg`'s, and
/// its requirements files'.
pub fn declared_python_packages(dir: &SecureDir, root: &RootFiles) -> Vec<String> {
    let mut requirements = Vec::new();
    if let Some(text) = read_text(dir, "pyproject.toml") {
        let pyproject = parse_pyproject(&text);
        requirements.extend(pyproject.dependencies);
        requirements.extend(pyproject.poetry_dependencies);
        for (_, list) in pyproject
            .extras
            .into_iter()
            .chain(pyproject.groups)
            .chain(pyproject.poetry_groups)
            .chain(pyproject.tool_groups)
        {
            requirements.extend(list);
        }
    }
    if let Some(text) = read_text(dir, "setup.cfg") {
        for (_, list) in setup_cfg_extras(&text) {
            requirements.extend(list);
        }
        requirements.extend(setup_cfg_list(&text, "options", "install_requires"));
    }
    for file in root.requirements_files() {
        if let Some(text) = read_text(dir, &file) {
            requirements.extend(requirements_lines(&text));
        }
    }
    let mut names: Vec<String> = requirements
        .iter()
        .filter_map(|requirement| requirement_name(requirement))
        .collect();
    names.sort();
    names.dedup();
    names
}

/// The normalized name of a PEP 508 requirement: `PyYAML>=6` is `pyyaml`,
/// `zope.interface` is `zope-interface`.
pub fn requirement_name(requirement: &str) -> Option<String> {
    let name: String = requirement
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .collect();
    (!name.is_empty()).then(|| normalize_package(&name))
}

/// A package or module name compared the way pip does: lower case, with
/// `_` and `.` read as `-`.
pub fn normalize_package(name: &str) -> String {
    name.to_ascii_lowercase().replace(['_', '.'], "-")
}

/// The dependency install a run's setup runs for a toolchain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Install {
    /// The command as a person reads and approves it, such as `npm ci`.
    pub command: String,
    /// The root files the command installs from; their content goes into
    /// the setup key.
    pub inputs: Vec<String>,
}

/// The command that installs the repository's dependencies for
/// `toolchain`, or `None` when it declares none. `check` is the run's check
/// command: a Python repository whose check runs pytest without declaring
/// it gets `pytest` installed too.
pub fn install_for(
    dir: &SecureDir,
    root: &RootFiles,
    toolchain: Toolchain,
    check: Option<&str>,
) -> Option<Install> {
    match toolchain {
        Toolchain::Node => node_install(dir, root),
        Toolchain::Python => python_install(dir, root, check),
        Toolchain::Rust => rust_install(dir, root),
        Toolchain::Go => go_install(dir, root),
    }
}

/// Whether a repository with a `yarn.lock` uses Yarn 2 or later ("Berry"):
/// its `packageManager` names `yarn@2` or later, or its lockfile is Berry's
/// (it starts with a `__metadata:` entry).
fn yarn_berry(dir: &SecureDir, package: &serde_json::Value) -> bool {
    if let Some(manager) = package
        .get("packageManager")
        .and_then(|value| value.as_str())
    {
        if let Some(version) = manager.strip_prefix("yarn@") {
            return version
                .split('.')
                .next()
                .and_then(|major| major.parse::<u32>().ok())
                .is_some_and(|major| major >= 2);
        }
    }
    dir.read_limited("yarn.lock", 4096)
        .map(|head| {
            String::from_utf8_lossy(&head)
                .lines()
                .any(|line| line.starts_with("__metadata:"))
        })
        .unwrap_or(false)
}

fn node_install(dir: &SecureDir, root: &RootFiles) -> Option<Install> {
    let package = package_json(dir)?;
    let has_dependencies = [
        "dependencies",
        "devDependencies",
        "optionalDependencies",
        "peerDependencies",
    ]
    .iter()
    .any(|key| {
        package
            .get(*key)
            .and_then(|value| value.as_object())
            .is_some_and(|map| !map.is_empty())
    });
    if !has_dependencies {
        return None;
    }
    let lock = |name: &str| root.has(name).then(|| name.to_string());
    let (command, lockfile) = if let Some(lockfile) =
        lock("package-lock.json").or_else(|| lock("npm-shrinkwrap.json"))
    {
        (
            "npm ci --prefer-offline --no-audit --no-fund",
            Some(lockfile),
        )
    } else if let Some(lockfile) = lock("pnpm-lock.yaml") {
        (
            "pnpm install --frozen-lockfile --prefer-offline --store-dir \"$AXOCOATL_CACHE/node/pnpm-store\"",
            Some(lockfile),
        )
    } else if let Some(lockfile) = lock("yarn.lock") {
        if yarn_berry(dir, &package) {
            // Yarn 2 and later refuse Yarn 1's flags (`--frozen-lockfile`,
            // `--prefer-offline`); `--immutable` is their frozen lockfile.
            // They read no proxy from HTTPS_PROXY, only from their own
            // settings.
            (
                "YARN_HTTPS_PROXY=\"$HTTPS_PROXY\" YARN_HTTP_PROXY=\"$HTTP_PROXY\" yarn install --immutable",
                Some(lockfile),
            )
        } else {
            (
                "yarn install --frozen-lockfile --prefer-offline",
                Some(lockfile),
            )
        }
    } else {
        (
            "npm install --no-package-lock --prefer-offline --no-audit --no-fund",
            None,
        )
    };
    let mut inputs = vec!["package.json".to_string()];
    inputs.extend(lockfile);
    Some(Install {
        command: command.into(),
        inputs,
    })
}

/// Whether a TOML text has the table `[<table>]` (read line by line, for a
/// `pyproject.toml` that is not valid TOML).
fn toml_has_table(text: &str, table: &str) -> bool {
    text.lines().any(|line| {
        let line = line.trim();
        line.starts_with('[')
            && !line.starts_with("[[")
            && line.trim_start_matches('[').trim_end_matches(']').trim() == table
    })
}

/// The extras of a `setup.cfg` (`[options.extras_require]`), each with its
/// requirements.
fn setup_cfg_extras(text: &str) -> Vec<(String, Vec<String>)> {
    let mut extras: Vec<(String, Vec<String>)> = Vec::new();
    let mut inside = false;
    for line in text.lines() {
        if line.trim_start().starts_with('[') {
            inside = line.trim() == "[options.extras_require]";
            continue;
        }
        if !inside || line.trim().is_empty() || line.trim_start().starts_with(['#', ';']) {
            continue;
        }
        if line.starts_with([' ', '\t']) {
            if let Some((_, list)) = extras.last_mut() {
                list.extend(cfg_values(line));
            }
        } else if let Some((key, value)) = line.split_once(['=', ':']) {
            let key = key.trim();
            if !key.is_empty() {
                extras.push((key.to_string(), cfg_values(value)));
            }
        }
    }
    extras
}

/// The list value `key` of the section `[section]` of a `setup.cfg`.
fn setup_cfg_list(text: &str, section: &str, key: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut in_section = false;
    let mut in_key = false;
    for line in text.lines() {
        if line.trim_start().starts_with('[') {
            in_section = line.trim() == format!("[{section}]");
            in_key = false;
            continue;
        }
        if !in_section || line.trim().is_empty() {
            continue;
        }
        if line.starts_with([' ', '\t']) {
            if in_key {
                values.extend(cfg_values(line));
            }
        } else {
            in_key = false;
            if let Some((name, value)) = line.split_once(['=', ':']) {
                if name.trim() == key {
                    in_key = true;
                    values.extend(cfg_values(value));
                }
            }
        }
    }
    values
}

/// The requirements of one `setup.cfg` value line (`;`-separated or one).
fn cfg_values(text: &str) -> Vec<String> {
    let text = text.trim();
    if text.is_empty() || text.starts_with(['#', ';']) {
        return Vec::new();
    }
    vec![text.to_string()]
}

/// The requirement lines of a requirements file (no options, includes or
/// comments).
fn requirements_lines(text: &str) -> Vec<String> {
    text.lines()
        .map(|line| line.split(" #").next().unwrap_or(line).trim())
        .filter(|line| !line.is_empty() && !line.starts_with(['#', '-']))
        .map(str::to_string)
        .collect()
}

/// What a `pyproject.toml` declares that a run's install needs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Pyproject {
    /// A `[project]` table (PEP 621): `pip install -e .` installs it.
    project: bool,
    dependencies: Vec<String>,
    /// `[project.optional-dependencies]`, in either TOML form.
    extras: Vec<(String, Vec<String>)>,
    /// `[dependency-groups]` (PEP 735), with `include-group`s expanded.
    groups: Vec<(String, Vec<String>)>,
    /// A `[tool.poetry]` table.
    poetry: bool,
    /// Poetry builds the project as a package (`package-mode` is not
    /// false), so `pip install -e .` installs it and its dependencies.
    poetry_package: bool,
    /// `[tool.poetry.dependencies]` as PEP 508 requirements, without
    /// `python`.
    poetry_dependencies: Vec<String>,
    /// `[tool.poetry.group.<name>.dependencies]`, and the older
    /// `[tool.poetry.dev-dependencies]` as `dev`, as PEP 508 requirements.
    /// pip installs none of them.
    poetry_groups: Vec<(String, Vec<String>)>,
    /// uv's `[tool.uv] dev-dependencies` (as `dev`), PDM's
    /// `[tool.pdm.dev-dependencies]` groups and Hatch's environments'
    /// `dependencies`: tool-specific lists pip does not install.
    tool_groups: Vec<(String, Vec<String>)>,
}

fn toml_strings(item: Option<&toml_edit::Item>) -> Vec<String> {
    item.and_then(|item| item.as_array())
        .map(|array| {
            array
                .iter()
                .filter_map(|value| value.as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn toml_lists(item: Option<&toml_edit::Item>) -> Vec<(String, Vec<String>)> {
    item.and_then(|item| item.as_table_like())
        .map(|table| {
            table
                .iter()
                .map(|(key, value)| (key.to_string(), toml_strings(Some(value))))
                .collect()
        })
        .unwrap_or_default()
}

fn parse_pyproject(text: &str) -> Pyproject {
    let Ok(document) = toml_edit::Document::parse(text) else {
        // pip cannot read it either; say only what its tables are.
        return Pyproject {
            project: toml_has_table(text, "project"),
            poetry: toml_has_table(text, "tool.poetry"),
            poetry_package: true,
            ..Pyproject::default()
        };
    };
    let root = document.as_item();
    let project = root.get("project");
    let poetry = root.get("tool").and_then(|tool| tool.get("poetry"));
    let mut pyproject = Pyproject {
        project: project.is_some_and(|project| project.is_table_like()),
        dependencies: toml_strings(project.and_then(|project| project.get("dependencies"))),
        extras: toml_lists(project.and_then(|project| project.get("optional-dependencies"))),
        poetry: poetry.is_some_and(|poetry| poetry.is_table_like()),
        poetry_package: poetry
            .and_then(|poetry| poetry.get("package-mode"))
            .and_then(|mode| mode.as_bool())
            != Some(false),
        ..Pyproject::default()
    };
    // PEP 735 groups: strings, and `{include-group = "name"}` tables.
    if let Some(groups) = root
        .get("dependency-groups")
        .and_then(|groups| groups.as_table_like())
    {
        let raw: Vec<(String, Vec<toml_edit::Value>)> = groups
            .iter()
            .map(|(name, value)| {
                (
                    name.to_string(),
                    value
                        .as_array()
                        .map(|array| array.iter().cloned().collect())
                        .unwrap_or_default(),
                )
            })
            .collect();
        fn expand(
            raw: &[(String, Vec<toml_edit::Value>)],
            name: &str,
            depth: usize,
            out: &mut Vec<String>,
        ) {
            let Some((_, values)) = raw.iter().find(|(group, _)| group == name) else {
                return;
            };
            for value in values {
                if let Some(requirement) = value.as_str() {
                    out.push(requirement.to_string());
                } else if let Some(include) = value
                    .as_inline_table()
                    .and_then(|table| table.get("include-group"))
                    .and_then(|group| group.as_str())
                {
                    if depth < 8 {
                        expand(raw, include, depth + 1, out);
                    }
                }
            }
        }
        for (name, _) in &raw {
            let mut list = Vec::new();
            expand(&raw, name, 0, &mut list);
            pyproject.groups.push((name.clone(), list));
        }
    }
    if let Some(poetry) = poetry {
        pyproject.poetry_dependencies = poetry_requirements(poetry.get("dependencies"));
        if let Some(groups) = poetry.get("group").and_then(|group| group.as_table_like()) {
            for (name, group) in groups.iter() {
                pyproject.poetry_groups.push((
                    name.to_string(),
                    poetry_requirements(group.get("dependencies")),
                ));
            }
        }
        if let Some(dev) = poetry.get("dev-dependencies") {
            if !pyproject
                .poetry_groups
                .iter()
                .any(|(name, _)| name == "dev")
            {
                pyproject
                    .poetry_groups
                    .push(("dev".into(), poetry_requirements(Some(dev))));
            }
        }
    }
    let tool = root.get("tool");
    let uv_dev = toml_strings(
        tool.and_then(|tool| tool.get("uv"))
            .and_then(|uv| uv.get("dev-dependencies")),
    );
    if !uv_dev.is_empty() {
        pyproject.tool_groups.push(("dev".into(), uv_dev));
    }
    pyproject.tool_groups.extend(
        toml_lists(
            tool.and_then(|tool| tool.get("pdm"))
                .and_then(|pdm| pdm.get("dev-dependencies")),
        )
        .into_iter()
        .map(|(name, list)| {
            (
                name,
                list.into_iter()
                    .filter(|requirement| !requirement.trim_start().starts_with('-'))
                    .collect(),
            )
        }),
    );
    if let Some(envs) = tool
        .and_then(|tool| tool.get("hatch"))
        .and_then(|hatch| hatch.get("envs"))
        .and_then(|envs| envs.as_table_like())
    {
        for (name, env) in envs.iter() {
            let list = toml_strings(env.get("dependencies"));
            if !list.is_empty() {
                // `hatch-test` is Hatch's test environment.
                let name = if name == "hatch-test" { "test" } else { name };
                pyproject.tool_groups.push((name.to_string(), list));
            }
        }
    }
    pyproject
}

/// Poetry's dependency table as PEP 508 requirements: `pytest = "^8.0"` is
/// `pytest>=8.0,<9`, `{version = "~1.2", extras = ["x"]}` is
/// `name[x]>=1.2,<1.3`. `python`, optional dependencies and those from git,
/// a path or a URL (which the registries alone cannot install) are left
/// out; a constraint it cannot read leaves the name alone.
fn poetry_requirements(item: Option<&toml_edit::Item>) -> Vec<String> {
    let Some(table) = item.and_then(|item| item.as_table_like()) else {
        return Vec::new();
    };
    let mut requirements = Vec::new();
    for (name, value) in table.iter() {
        if name.eq_ignore_ascii_case("python") || requirement_name(name).is_none() {
            continue;
        }
        let (constraint, extras) = if let Some(constraint) = value.as_str() {
            (Some(constraint.to_string()), Vec::new())
        } else if let Some(spec) = value.as_table_like() {
            let optional = spec
                .get("optional")
                .and_then(|optional| optional.as_bool())
                .unwrap_or(false);
            if optional
                || ["git", "path", "url"]
                    .iter()
                    .any(|key| spec.get(key).is_some())
            {
                continue;
            }
            (
                spec.get("version")
                    .and_then(|version| version.as_str())
                    .map(str::to_string),
                toml_strings(spec.get("extras")),
            )
        } else {
            // A list of constraints by Python version: the name alone.
            (None, Vec::new())
        };
        let mut requirement = name.to_string();
        if !extras.is_empty() {
            requirement.push_str(&format!("[{}]", extras.join(",")));
        }
        if let Some(specifier) = constraint.as_deref().and_then(poetry_constraint) {
            requirement.push_str(&specifier);
        }
        requirements.push(requirement);
    }
    requirements
}

/// A Poetry version constraint as a PEP 440 specifier, or `None` when it
/// says any version or cannot be read (`^1.2.3`, `~1.2`, `1.2.*`, `>=1,<2`,
/// `1.2.3`).
fn poetry_constraint(constraint: &str) -> Option<String> {
    let constraint = constraint.trim();
    if constraint.is_empty() || constraint == "*" || constraint.contains("||") {
        return None;
    }
    let numbers = |text: &str| -> Option<Vec<u64>> {
        text.trim()
            .split('.')
            .map(|part| part.parse::<u64>().ok())
            .collect()
    };
    let join = |parts: &[u64]| {
        parts
            .iter()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join(".")
    };
    if let Some(version) = constraint.strip_prefix('^') {
        let parts = numbers(version)?;
        // The first non-zero part may not grow.
        let at = parts
            .iter()
            .position(|part| *part != 0)
            .unwrap_or(parts.len().saturating_sub(1));
        let mut upper = parts[..=at].to_vec();
        upper[at] += 1;
        return Some(format!(">={},<{}", join(&parts), join(&upper)));
    }
    if let Some(version) = constraint
        .strip_prefix('~')
        .filter(|rest| !rest.starts_with('='))
    {
        let parts = numbers(version)?;
        let mut upper = if parts.len() >= 2 {
            parts[..2].to_vec()
        } else {
            parts.clone()
        };
        let last = upper.len() - 1;
        upper[last] += 1;
        return Some(format!(">={},<{}", join(&parts), join(&upper)));
    }
    if constraint.starts_with(['<', '>', '=', '!', '~']) {
        let ok = constraint.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(c, '.' | ',' | '<' | '>' | '=' | '!' | '~' | '*' | ' ')
        });
        return ok.then(|| constraint.replace(' ', ""));
    }
    if numbers(constraint.trim_end_matches(".*")).is_some() {
        return Some(format!("=={constraint}"));
    }
    None
}

/// The extras, groups and their kinds a run looks for, in order.
const TEST_NAMES: [&str; 4] = ["test", "tests", "testing", "dev"];

fn first_test(lists: &[(String, Vec<String>)]) -> Option<&(String, Vec<String>)> {
    TEST_NAMES
        .iter()
        .find_map(|wanted| lists.iter().find(|(name, _)| name == wanted))
}

/// Whether one of `requirements` installs pytest (`pytest` itself, or a
/// `pytest-*` plugin, which requires it).
fn installs_pytest(requirements: &[String]) -> bool {
    requirements.iter().any(|requirement| {
        requirement_name(requirement)
            .is_some_and(|name| name == "pytest" || name.starts_with("pytest-"))
    })
}

fn python_install(dir: &SecureDir, root: &RootFiles, check: Option<&str>) -> Option<Install> {
    let pyproject = read_text(dir, "pyproject.toml")
        .map(|text| parse_pyproject(&text))
        .unwrap_or_default();
    let setup_cfg = read_text(dir, "setup.cfg");
    let cfg_extras = setup_cfg
        .as_deref()
        .map(setup_cfg_extras)
        .unwrap_or_default();
    let mut args = Vec::new();
    // What the command installs, as far as the files say.
    let mut installed: Vec<String> = Vec::new();
    let installable = pyproject.project
        || (pyproject.poetry && pyproject.poetry_package)
        || root.has("setup.py")
        || setup_cfg
            .as_deref()
            .is_some_and(|text| text.contains("[metadata]"));
    if installable {
        installed.extend(pyproject.dependencies.iter().cloned());
        installed.extend(pyproject.poetry_dependencies.iter().cloned());
        let mut extras = pyproject.extras.clone();
        extras.extend(cfg_extras);
        match first_test(&extras) {
            Some((extra, list)) => {
                args.push(format!("-e '.[{extra}]'"));
                installed.extend(list.iter().cloned());
            }
            None => args.push("-e .".to_string()),
        }
    } else if pyproject.poetry {
        // `package-mode = false`: nothing to build, only dependencies.
        for requirement in &pyproject.poetry_dependencies {
            args.push(shell_quote(requirement));
        }
        installed.extend(pyproject.poetry_dependencies.iter().cloned());
    }
    // The test group: PEP 735 (`pip install --group`), else the
    // tool-specific lists pip does not read, given as requirements.
    if let Some((group, list)) = first_test(&pyproject.groups) {
        args.push(format!("--group {}", shell_quote(group)));
        installed.extend(list.iter().cloned());
    } else if let Some((_, list)) =
        first_test(&pyproject.poetry_groups).or_else(|| first_test(&pyproject.tool_groups))
    {
        for requirement in list {
            args.push(shell_quote(requirement));
        }
        installed.extend(list.iter().cloned());
    }
    let mut inputs = Vec::new();
    for file in root.requirements_files() {
        args.push(format!("-r {}", shell_quote(&file)));
        if let Some(text) = read_text(dir, &file) {
            installed.extend(requirements_lines(&text));
        }
    }
    for name in ["pyproject.toml", "setup.py", "setup.cfg"] {
        if root.has(name) {
            inputs.push(name.to_string());
        }
    }
    inputs.extend(root.requirements_files());
    let runs_pytest = check
        .and_then(check_program)
        .is_some_and(|program| program == "pytest" || program == "py.test")
        || check.is_some_and(|check| check.contains("-m pytest"));
    // pytest is installed when the check runs it and nothing this command
    // installs brings it, whatever the files mention elsewhere.
    if runs_pytest && !installs_pytest(&installed) {
        args.push("pytest".into());
    }
    if args.is_empty() {
        return None;
    }
    Some(Install {
        command: format!("pip install {}", args.join(" ")),
        inputs,
    })
}

fn rust_install(dir: &SecureDir, root: &RootFiles) -> Option<Install> {
    let manifest = read_text(dir, "Cargo.toml")?;
    let needs = toml_has_table(&manifest, "workspace")
        || manifest.lines().any(|line| {
            let line = line.trim();
            line.starts_with('[') && line.contains("dependencies")
        });
    if !needs {
        return None;
    }
    let mut inputs = vec!["Cargo.toml".to_string()];
    if root.has("Cargo.lock") {
        inputs.push("Cargo.lock".into());
    }
    Some(Install {
        command: "cargo fetch".into(),
        inputs,
    })
}

fn go_install(dir: &SecureDir, root: &RootFiles) -> Option<Install> {
    let manifest = read_text(dir, "go.mod")?;
    let requires = manifest
        .lines()
        .any(|line| line.trim_start().starts_with("require"));
    if !requires {
        return None;
    }
    let mut inputs = vec!["go.mod".to_string()];
    if root.has("go.sum") {
        inputs.push("go.sum".into());
    }
    Some(Install {
        command: "GOFLAGS=-modcacherw GOPROXY=https://proxy.golang.org go mod download".into(),
        inputs,
    })
}

/// The key of one setup: the toolchain, the image it runs in, the command,
/// and the content of every file it installs from. A setup whose key the
/// Workspace's cache already holds installed exactly this.
pub fn setup_key(
    dir: &SecureDir,
    toolchain: Toolchain,
    image: &str,
    command: &str,
    inputs: &[String],
) -> String {
    let mut hash = Sha256::new();
    for part in [toolchain.name(), image, command] {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    for name in inputs {
        hash.update((name.len() as u64).to_be_bytes());
        hash.update(name.as_bytes());
        match dir.read_limited(name, MAX_LOCKFILE_BYTES) {
            Ok(bytes) => {
                hash.update((bytes.len() as u64).to_be_bytes());
                hash.update(&bytes);
            }
            Err(_) => hash.update(u64::MAX.to_be_bytes()),
        }
    }
    let digest = hash.finalize();
    digest.iter().take(16).map(|b| format!("{b:02x}")).collect()
}

/// The name of the file in a toolchain's directory of the Workspace's cache
/// that holds the [`setup_key`] of the last setup that installed into it.
/// Only root writes it, after the setup, once the directory is root's again.
pub const SETUP_STAMP: &str = "setup.stamp";

impl Toolchain {
    /// The entries of the toolchain's cache directory a setup starts again
    /// from when its key changed (a different image or dependency set): the
    /// Python virtual environment, so nothing of the old set stays.
    pub fn setup_reset_entries(self) -> &'static [&'static str] {
        match self {
            Toolchain::Python => &["venv"],
            _ => &[],
        }
    }
}

/// The shell script a toolchain's setup runs (as the Session's writer, in
/// the repository): `command`, after what the toolchain needs first (a
/// Python virtual environment in the cache). Whether it runs at all, and
/// who owns the cache before and after, is root's to decide around it
/// ([`SETUP_STAMP`]).
pub fn setup_script(toolchain: Toolchain, command: &str) -> String {
    match toolchain {
        Toolchain::Python => format!(
            "{{ [ -x \"$VIRTUAL_ENV/bin/python\" ] || python3 -m venv \"$VIRTUAL_ENV\"; }} && {command}"
        ),
        _ => command.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(files: &[(&str, &str)]) -> (tempfile::TempDir, SecureDir, RootFiles) {
        let dir = tempfile::tempdir().unwrap();
        for (name, contents) in files {
            let path = dir.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }
        let secure = SecureDir::open(dir.path()).unwrap();
        let root = RootFiles::read(&secure).unwrap();
        (dir, secure, root)
    }

    #[test]
    fn programs_name_their_toolchains() {
        assert_eq!(check_program("pytest -q"), Some("pytest".into()));
        assert_eq!(check_program("CI=1 FOO=bar npm test"), Some("npm".into()));
        assert_eq!(
            check_program("env RUST_LOG=1 cargo test"),
            Some("cargo".into())
        );
        assert_eq!(check_program("sh -c 'go test ./...'"), Some("go".into()));
        assert_eq!(
            check_program("/usr/bin/python3 -m pytest"),
            Some("python3".into())
        );
        assert_eq!(check_program("npm ci && npm test"), Some("npm".into()));
        assert_eq!(check_program("   "), None);
        for (program, toolchain) in [
            ("pytest", Some(Toolchain::Python)),
            ("python3.13", Some(Toolchain::Python)),
            ("python", Some(Toolchain::Python)),
            ("cargo", Some(Toolchain::Rust)),
            ("go", Some(Toolchain::Go)),
            ("npx", Some(Toolchain::Node)),
            ("pnpm", Some(Toolchain::Node)),
            ("yarn", Some(Toolchain::Node)),
            ("make", None),
            ("python3.x", None),
        ] {
            assert_eq!(toolchain_for_program(program), toolchain, "{program}");
        }
        assert_eq!(shell_quote("requirements-dev.txt"), "requirements-dev.txt");
        assert_eq!(shell_quote("it's here.txt"), "'it'\\''s here.txt'");
    }

    #[test]
    fn one_toolchain_is_chosen_from_its_files() {
        let (_d, _s, root) = repo(&[("package.json", "{}")]);
        let choice = choose(&candidates(&root), None);
        assert_eq!(
            choice,
            Choice::Chosen {
                toolchain: Toolchain::Node,
                reason: "package.json at the repository root".into()
            }
        );
        let (_d, _s, root) = repo(&[("requirements-dev.txt", "pytest\n"), ("app.py", "")]);
        assert!(matches!(
            choose(&candidates(&root), None),
            Choice::Chosen {
                toolchain: Toolchain::Python,
                ..
            }
        ));
        let (_d, _s, root) = repo(&[("README.md", "")]);
        assert!(matches!(
            choose(&candidates(&root), None),
            Choice::Nothing { .. }
        ));
    }

    #[test]
    fn several_toolchains_follow_the_check_or_are_refused() {
        let (_d, _s, root) = repo(&[
            ("package.json", "{}"),
            ("pyproject.toml", "[project]\nname='x'\n"),
            ("go.mod", "module x\n"),
        ]);
        let found = candidates(&root);
        assert_eq!(found.len(), 3);
        match choose(&found, Some("pytest -q")) {
            Choice::Chosen { toolchain, reason } => {
                assert_eq!(toolchain, Toolchain::Python);
                assert!(reason.contains("pyproject.toml"), "{reason}");
                assert!(reason.contains("pytest"), "{reason}");
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            choose(&found, Some("go test ./...")),
            Choice::Chosen {
                toolchain: Toolchain::Go,
                ..
            }
        ));
        let Choice::Ambiguous { candidates: listed } = choose(&found, Some("make test")) else {
            panic!("a make check does not say");
        };
        let message = ambiguous_message(&listed, Some("make test"));
        assert!(message.contains("package.json (node)"), "{message}");
        assert!(message.contains("go.mod (go)"), "{message}");
        assert!(
            message.contains("Pass --toolchain <language>, where <language> is node, python or go"),
            "{message}"
        );
        assert!(!message.contains('|'), "{message}");
        assert!(message.contains("`make test`"), "{message}");
        // A check for a toolchain the repository lacks does not choose.
        assert!(matches!(
            choose(&found, Some("cargo test")),
            Choice::Ambiguous { .. }
        ));
        assert!(matches!(choose(&found, None), Choice::Ambiguous { .. }));
    }

    #[test]
    fn axocoatl_md_names_the_check() {
        assert_eq!(
            check_line("# Project\n\nCheck command: pytest -q\n"),
            Some("pytest -q".into())
        );
        assert_eq!(
            check_line("- **Check command:** `npm test`\n"),
            Some("npm test".into())
        );
        assert_eq!(
            check_line("check command : `cargo test --all`"),
            Some("cargo test --all".into())
        );
        assert_eq!(
            check_line("Check command:\n\n```sh\ngo test ./...\n```\n"),
            Some("go test ./...".into())
        );
        assert_eq!(check_line("The check command is pytest"), None);
        assert_eq!(check_line("Check commands: x"), None);
        assert_eq!(check_line("Check command:"), None);
        let (_d, secure, _root) = repo(&[
            (
                "AXOCOATL.md",
                "Notes\nCheck command: python3 -m unittest -v\n",
            ),
            (
                "pyproject.toml",
                "[project]\nname = 'x'\n[tool.pytest.ini_options]\n",
            ),
        ]);
        assert_eq!(
            detect_check(&secure),
            Some(("python3 -m unittest -v".into(), CheckSource::AxocoatlMd))
        );
    }

    #[test]
    fn python_runs_pytest_only_when_the_repository_uses_it() {
        let (_d, secure, root) = repo(&[
            ("pyproject.toml", "[project]\nname = 'calc'\n"),
            ("tests/test_calc.py", "import unittest\n"),
        ]);
        assert_eq!(
            detect_check_in(&secure, &root),
            Some("python3 -m unittest discover -v -s tests".into())
        );
        let (_d, secure, root) = repo(&[
            ("pyproject.toml", "[project]\nname = 'calc'\n"),
            ("tests/__init__.py", ""),
        ]);
        assert_eq!(
            detect_check_in(&secure, &root),
            Some("python3 -m unittest discover -v".into())
        );
        let (_d, secure, root) = repo(&[(
            "pyproject.toml",
            "[project]\nname = 'pages'\n[tool.pytest.ini_options]\ntestpaths = ['tests']\n",
        )]);
        assert_eq!(
            detect_check_in(&secure, &root),
            Some("python3 -m pytest".into())
        );
        let (_d, secure, root) = repo(&[("setup.py", ""), ("tests/conftest.py", "")]);
        assert_eq!(
            detect_check_in(&secure, &root),
            Some("python3 -m pytest".into())
        );
        let (_d, secure, root) = repo(&[
            ("package.json", r#"{"scripts":{"test":"mocha"}}"#),
            ("Cargo.toml", ""),
        ]);
        assert_eq!(detect_check_in(&secure, &root), Some("npm run test".into()));
        let (_d, secure, root) = repo(&[("Cargo.toml", "")]);
        assert_eq!(detect_check_in(&secure, &root), Some("cargo test".into()));
        let (_d, secure, root) = repo(&[("go.mod", "module x\n")]);
        assert_eq!(
            detect_check_in(&secure, &root),
            Some("go test ./...".into())
        );
        let (_d, secure, root) = repo(&[("Makefile", "test:\n")]);
        assert_eq!(detect_check_in(&secure, &root), Some("make test".into()));
    }

    #[test]
    fn node_installs_follow_the_lockfile() {
        let deps = r#"{"devDependencies":{"mocha":"10.7.3"}}"#;
        let (_d, secure, root) = repo(&[("package.json", deps), ("package-lock.json", "{}")]);
        let install = install_for(&secure, &root, Toolchain::Node, None).unwrap();
        assert_eq!(
            install.command,
            "npm ci --prefer-offline --no-audit --no-fund"
        );
        assert_eq!(install.inputs, ["package.json", "package-lock.json"]);
        let (_d, secure, root) = repo(&[("package.json", deps), ("pnpm-lock.yaml", "")]);
        assert!(install_for(&secure, &root, Toolchain::Node, None)
            .unwrap()
            .command
            .starts_with("pnpm install --frozen-lockfile"));
        let (_d, secure, root) = repo(&[("package.json", deps), ("yarn.lock", "")]);
        assert!(install_for(&secure, &root, Toolchain::Node, None)
            .unwrap()
            .command
            .starts_with("yarn install --frozen-lockfile"));
        let (_d, secure, root) = repo(&[("package.json", deps)]);
        assert!(install_for(&secure, &root, Toolchain::Node, None)
            .unwrap()
            .command
            .starts_with("npm install --no-package-lock"));
        // Nothing to install: no setup at all.
        let (_d, secure, root) = repo(&[("package.json", r#"{"scripts":{"test":"node --test"}}"#)]);
        assert_eq!(install_for(&secure, &root, Toolchain::Node, None), None);
        assert_eq!(Toolchain::Node.registry_presets(), ["npm"]);
    }

    #[test]
    fn python_installs_the_project_with_its_test_extra_and_requirements() {
        let (_d, secure, root) = repo(&[
            (
                "pyproject.toml",
                "[project]\nname = 'pages'\n\n[project.optional-dependencies]\ndocs = ['x']\n\"test\" = [\n  \"pytest>=8\",\n]\n\n[dependency-groups]\ndev = ['ruff']\n",
            ),
            ("requirements-dev.txt", "hypothesis\n"),
        ]);
        let install = install_for(&secure, &root, Toolchain::Python, Some("pytest")).unwrap();
        assert_eq!(
            install.command,
            "pip install -e '.[test]' --group dev -r requirements-dev.txt"
        );
        assert_eq!(install.inputs, ["pyproject.toml", "requirements-dev.txt"]);
        // pytest configured but not listed as a requirement: the run
        // installs it (the README example's repository).
        let (_d, secure, root) = repo(&[(
            "pyproject.toml",
            "[project]\nname = 'pages'\n[tool.pytest.ini_options]\ntestpaths = [\"tests\"]\n",
        )]);
        let install = install_for(&secure, &root, Toolchain::Python, Some("pytest")).unwrap();
        assert_eq!(install.command, "pip install -e . pytest");
        assert!(installs_pytest(&["pytest>=8".into()]));
        assert!(installs_pytest(&["ruff".into(), "pytest-cov".into()]));
        assert!(!installs_pytest(&["pytestx".into(), "ruff".into()]));
        let (_d, secure, root) = repo(&[
            ("pyproject.toml", "[project]\nname = 'pages'\n"),
            ("tests/conftest.py", ""),
        ]);
        let install = install_for(&secure, &root, Toolchain::Python, Some("pytest")).unwrap();
        assert_eq!(install.command, "pip install -e . pytest");
        let (_d, secure, root) = repo(&[("requirements.txt", "requests\n")]);
        let install = install_for(
            &secure,
            &root,
            Toolchain::Python,
            Some("python3 -m unittest"),
        )
        .unwrap();
        assert_eq!(install.command, "pip install -r requirements.txt");
        let (_d, secure, root) = repo(&[(
            "setup.cfg",
            "[metadata]\nname = x\n[options.extras_require]\ntesting =\n    pytest\n",
        )]);
        assert_eq!(
            install_for(&secure, &root, Toolchain::Python, None)
                .unwrap()
                .command,
            "pip install -e '.[testing]'"
        );
        let (_d, secure, root) = repo(&[("pytest.ini", "")]);
        assert_eq!(
            install_for(&secure, &root, Toolchain::Python, Some("pytest"))
                .unwrap()
                .command,
            "pip install pytest"
        );
    }

    #[test]
    fn rust_and_go_fetch_only_when_they_have_dependencies() {
        let (_d, secure, root) = repo(&[
            (
                "Cargo.toml",
                "[package]\nname = 'x'\n\n[dev-dependencies]\nproptest = '1'\n",
            ),
            ("Cargo.lock", ""),
        ]);
        let install = install_for(&secure, &root, Toolchain::Rust, None).unwrap();
        assert_eq!(install.command, "cargo fetch");
        assert_eq!(install.inputs, ["Cargo.toml", "Cargo.lock"]);
        let (_d, secure, root) = repo(&[("Cargo.toml", "[package]\nname = 'x'\n")]);
        assert_eq!(install_for(&secure, &root, Toolchain::Rust, None), None);
        let (_d, secure, root) = repo(&[
            (
                "go.mod",
                "module x\n\ngo 1.27\n\nrequire github.com/google/go-cmp v0.7.0\n",
            ),
            ("go.sum", ""),
        ]);
        let install = install_for(&secure, &root, Toolchain::Go, None).unwrap();
        assert!(
            install.command.ends_with("go mod download"),
            "{}",
            install.command
        );
        assert!(install.command.contains("GOPROXY=https://proxy.golang.org"));
        let (_d, secure, root) = repo(&[("go.mod", "module x\n\ngo 1.27\n")]);
        assert_eq!(install_for(&secure, &root, Toolchain::Go, None), None);
    }

    #[test]
    fn the_setup_key_changes_with_the_image_the_command_and_the_files() {
        let (dir, secure, _root) = repo(&[("go.mod", "module x\n"), ("go.sum", "a")]);
        let inputs = vec!["go.mod".to_string(), "go.sum".to_string()];
        let key = setup_key(&secure, Toolchain::Go, "img:1", "go mod download", &inputs);
        assert_eq!(key.len(), 32);
        assert_eq!(
            key,
            setup_key(&secure, Toolchain::Go, "img:1", "go mod download", &inputs)
        );
        assert_ne!(
            key,
            setup_key(&secure, Toolchain::Go, "img:2", "go mod download", &inputs)
        );
        assert_ne!(
            key,
            setup_key(&secure, Toolchain::Go, "img:1", "go mod tidy", &inputs)
        );
        std::fs::write(dir.path().join("go.sum"), "b").unwrap();
        assert_ne!(
            key,
            setup_key(&secure, Toolchain::Go, "img:1", "go mod download", &inputs)
        );
    }

    #[test]
    fn setup_scripts_prepare_the_toolchain_first() {
        assert_eq!(setup_script(Toolchain::Node, "npm ci"), "npm ci");
        let python = setup_script(Toolchain::Python, "pip install -e .");
        assert!(
            python.starts_with("{ [ -x \"$VIRTUAL_ENV/bin/python\" ]"),
            "{python}"
        );
        assert!(python.ends_with("&& pip install -e ."), "{python}");
        // The stamp is root's: the writer's script never reads or writes it.
        assert!(!python.contains(SETUP_STAMP), "{python}");
        assert_eq!(setup_script(Toolchain::Rust, "cargo fetch"), "cargo fetch");
        assert_eq!(Toolchain::Python.setup_reset_entries(), ["venv"]);
        assert!(Toolchain::Go.setup_reset_entries().is_empty());
    }

    /// TC-COR-5: a check the shell decides at run time names no program,
    /// so the probe does not look one up; a leading `cd dir &&` is skipped.
    #[test]
    fn checks_the_shell_decides_name_no_program() {
        assert_eq!(check_program("(cd web && npm test)"), None);
        assert_eq!(check_program("{ npm test; }"), None);
        assert_eq!(check_program("$PYTHON -m pytest"), None);
        assert_eq!(check_program("`which pytest` -q"), None);
        assert_eq!(check_program("if true; then pytest; fi"), None);
        assert_eq!(check_program("cd web && npm test"), Some("npm".into()));
        assert_eq!(check_program("cd web; pytest -q"), Some("pytest".into()));
        assert_eq!(check_program("cd web || exit 1"), Some("cd".into()));
        assert_eq!(
            check_program("export CI=1 && cargo test"),
            Some("cargo".into())
        );
        assert_eq!(
            check_program("FOO='a b' pytest -k 'x and y'"),
            Some("pytest".into())
        );
    }

    /// TC-NU-1: plain pytest-style test functions get `pytest`, which the
    /// setup then installs; unittest-only test files keep `unittest`.
    #[test]
    fn python_tests_written_for_pytest_get_pytest() {
        let (_d, secure, root) = repo(&[
            ("requirements.txt", "six\n"),
            (
                "tests/test_pages.py",
                "from pages import count\n\ndef test_count():\n    assert count(3) == 1\n",
            ),
            ("pages.py", "def count(n):\n    return 0\n"),
        ]);
        assert_eq!(python_test_files(&secure, &root), ["tests/test_pages.py"]);
        assert_eq!(
            detect_check_in(&secure, &root),
            Some("python3 -m pytest".into())
        );
        assert_eq!(
            install_for(&secure, &root, Toolchain::Python, Some("pytest"))
                .unwrap()
                .command,
            "pip install -r requirements.txt pytest"
        );
        let (_d, secure, root) = repo(&[
            ("pyproject.toml", "[project]\nname = 'calc'\n"),
            (
                "tests/test_calc.py",
                "import unittest\n\nclass T(unittest.TestCase):\n    def test_add(self):\n        pass\n",
            ),
            ("tests/helpers/test_util.py", "from unittest import TestCase\n"),
        ]);
        assert_eq!(
            python_test_style(&secure, &python_test_files(&secure, &root)),
            PythonTests::Unittest
        );
        assert_eq!(
            detect_check_in(&secure, &root),
            Some("python3 -m unittest discover -v -s tests".into())
        );
        // Dependencies and hidden directories are not searched.
        let (_d, secure, root) = repo(&[
            ("setup.py", ""),
            (".venv/lib/test_x.py", "def test_x(): pass\n"),
            ("node_modules/x/test_y.py", "def test_y(): pass\n"),
            ("src/app/app_test.py", "import pytest\n"),
        ]);
        assert_eq!(python_test_files(&secure, &root), ["src/app/app_test.py"]);
        assert_eq!(
            detect_check_in(&secure, &root),
            Some("python3 -m pytest".into())
        );
        let (_d, secure, _root) = repo(&[
            ("test/a.spec.ts", ""),
            ("lib/b.test.js", ""),
            ("test/helper.js", ""),
            ("lib/c.js", ""),
            ("pkg/x_test.go", ""),
        ]);
        assert_eq!(
            test_files(&secure, Toolchain::Node),
            ["lib/b.test.js", "test/a.spec.ts", "test/helper.js"]
        );
        assert_eq!(test_files(&secure, Toolchain::Go), ["pkg/x_test.go"]);
        assert!(test_files(&secure, Toolchain::Rust).is_empty());
    }

    /// TC-COR-3 and TC-NU-2: pytest is installed whenever the check runs it
    /// and what the command installs does not bring it, whatever else the
    /// files mention.
    #[test]
    fn pytest_is_installed_unless_the_selected_install_brings_it() {
        // Poetry: pip installs neither group; the dev group's packages are
        // given as requirements, with Poetry's constraints translated.
        let (_d, secure, root) = repo(&[(
            "pyproject.toml",
            "[tool.poetry]\nname = 'pages'\nversion = '0.1.0'\n\n[tool.poetry.dependencies]\npython = '^3.10'\nrequests = '^2.31'\n\n[tool.poetry.group.dev.dependencies]\npytest = \"^8.0\"\npytest-mock = { version = \"~3.14\" }\n\n[build-system]\nrequires = ['poetry-core']\nbuild-backend = 'poetry.core.masonry.api'\n",
        )]);
        assert_eq!(
            install_for(&secure, &root, Toolchain::Python, Some("pytest"))
                .unwrap()
                .command,
            "pip install -e . 'pytest>=8.0,<9' 'pytest-mock>=3.14,<3.15'"
        );
        // Poetry's older dev-dependencies table, and a project that is not
        // a package.
        let (_d, secure, root) = repo(&[(
            "pyproject.toml",
            "[tool.poetry]\nname = 'x'\npackage-mode = false\n[tool.poetry.dependencies]\npython = '*'\nflask = '2.3.*'\n[tool.poetry.dev-dependencies]\nnose = '*'\n",
        )]);
        assert_eq!(
            install_for(&secure, &root, Toolchain::Python, Some("pytest -q"))
                .unwrap()
                .command,
            "pip install 'flask==2.3.*' nose pytest"
        );
        // PEP 621 inline optional-dependencies.
        let (_d, secure, root) = repo(&[(
            "pyproject.toml",
            "[project]\nname = 'x'\noptional-dependencies = { test = [\"pytest>=8\"], docs = [\"sphinx\"] }\n",
        )]);
        assert_eq!(
            install_for(&secure, &root, Toolchain::Python, Some("pytest"))
                .unwrap()
                .command,
            "pip install -e '.[test]'"
        );
        // An extra that does not hold pytest: it is added.
        let (_d, secure, root) = repo(&[(
            "pyproject.toml",
            "[project]\nname = 'x'\n[project.optional-dependencies]\ndev = ['ruff']\nlint = ['pytest']\n",
        )]);
        assert_eq!(
            install_for(&secure, &root, Toolchain::Python, Some("python3 -m pytest"))
                .unwrap()
                .command,
            "pip install -e '.[dev]' pytest"
        );
        // PEP 735 groups, with include-group.
        let (_d, secure, root) = repo(&[(
            "pyproject.toml",
            "[project]\nname = 'x'\n[dependency-groups]\nlint = ['pytest-cov']\ntest = [{include-group = 'lint'}, 'hypothesis']\n",
        )]);
        assert_eq!(
            install_for(&secure, &root, Toolchain::Python, Some("pytest"))
                .unwrap()
                .command,
            "pip install -e . --group test"
        );
        // uv's and PDM's dev lists.
        let (_d, secure, root) = repo(&[(
            "pyproject.toml",
            "[project]\nname = 'x'\n[tool.uv]\ndev-dependencies = ['pytest>=8', 'ruff']\n",
        )]);
        assert_eq!(
            install_for(&secure, &root, Toolchain::Python, Some("pytest"))
                .unwrap()
                .command,
            "pip install -e . 'pytest>=8' ruff"
        );
        let (_d, secure, root) = repo(&[(
            "pyproject.toml",
            "[project]\nname = 'x'\n[tool.pdm.dev-dependencies]\ntest = ['-e file:///${PROJECT_ROOT}/sub', 'pytest']\n",
        )]);
        assert_eq!(
            install_for(&secure, &root, Toolchain::Python, Some("pytest"))
                .unwrap()
                .command,
            "pip install -e . pytest"
        );
        // Mentioning pytest outside what is installed does not count.
        let (_d, secure, root) = repo(&[
            (
                "pyproject.toml",
                "[project]\nname = 'x'\n[tool.pytest.ini_options]\naddopts = '-q'\n",
            ),
            ("requirements-docs.txt", "# pytest is not here\nsphinx\n"),
        ]);
        assert_eq!(
            install_for(&secure, &root, Toolchain::Python, Some("pytest"))
                .unwrap()
                .command,
            "pip install -e . -r requirements-docs.txt pytest"
        );
        // A setup.cfg extra's own list counts.
        let (_d, secure, root) = repo(&[(
            "setup.cfg",
            "[metadata]\nname = x\n[options.extras_require]\ntesting =\n    pytest>=7\n    mock\n",
        )]);
        assert_eq!(
            install_for(&secure, &root, Toolchain::Python, Some("pytest"))
                .unwrap()
                .command,
            "pip install -e '.[testing]'"
        );
        // An unreadable pyproject.toml is still installed, so the setup
        // fails with pip's own words about it.
        let (_d, secure, root) = repo(&[("pyproject.toml", "[project\nname = \n")]);
        assert_eq!(
            install_for(&secure, &root, Toolchain::Python, Some("pytest"))
                .unwrap()
                .command,
            "pip install -e . pytest"
        );
    }

    #[test]
    fn poetry_constraints_become_pep_440_specifiers() {
        for (poetry, pep440) in [
            ("^1.2.3", Some(">=1.2.3,<2")),
            ("^0.2.3", Some(">=0.2.3,<0.3")),
            ("^0.0.3", Some(">=0.0.3,<0.0.4")),
            ("^2", Some(">=2,<3")),
            ("~1.2.3", Some(">=1.2.3,<1.3")),
            ("~1", Some(">=1,<2")),
            ("~=1.4", Some("~=1.4")),
            (">= 1.0, < 2.0", Some(">=1.0,<2.0")),
            ("1.2.3", Some("==1.2.3")),
            ("1.2.*", Some("==1.2.*")),
            ("*", None),
            ("^1.0 || ^2.0", None),
            ("latest", None),
        ] {
            assert_eq!(poetry_constraint(poetry).as_deref(), pep440, "{poetry}");
        }
    }

    #[test]
    fn declared_packages_and_script_programs_are_read_from_the_files() {
        let (_d, secure, root) = repo(&[
            (
                "pyproject.toml",
                "[project]\nname = 'x'\ndependencies = ['PyYAML>=6', 'zope.interface']\n[tool.poetry.group.dev.dependencies]\nRequests_Mock = '*'\n",
            ),
            ("requirements-dev.txt", "-r requirements.txt\nhypothesis==6.1 # fuzz\n"),
            (
                "package.json",
                r#"{"scripts":{"test":"NODE_ENV=test mocha --exit","check":"tsc && jest"}}"#,
            ),
        ]);
        assert_eq!(
            declared_python_packages(&secure, &root),
            ["hypothesis", "pyyaml", "requests-mock", "zope-interface"]
        );
        assert_eq!(normalize_package("requests_mock"), "requests-mock");
        assert_eq!(
            package_script_program(&secure, "npm test"),
            Some("mocha".into())
        );
        assert_eq!(
            package_script_program(&secure, "npm run check"),
            Some("tsc".into())
        );
        assert_eq!(
            package_script_program(&secure, "yarn test --watch=false"),
            Some("mocha".into())
        );
        assert_eq!(package_script_program(&secure, "npm run nope"), None);
        assert_eq!(
            python_module_of("python3 -m pytest -q"),
            Some("pytest".into())
        );
        assert_eq!(
            python_module_of("python -m unittest discover"),
            Some("unittest".into())
        );
        assert_eq!(python_module_of("pytest -m slow"), None);
        assert_eq!(package_script_program(&secure, "pytest"), None);
    }

    /// TC-COR-4: Yarn 2+ refuses Yarn 1's install flags.
    #[test]
    fn yarn_berry_installs_with_immutable() {
        let deps = r#"{"packageManager":"yarn@4.5.0","devDependencies":{"mocha":"10.7.3"}}"#;
        let (_d, secure, root) = repo(&[("package.json", deps), ("yarn.lock", "")]);
        assert_eq!(
            install_for(&secure, &root, Toolchain::Node, None)
                .unwrap()
                .command,
            "YARN_HTTPS_PROXY=\"$HTTPS_PROXY\" YARN_HTTP_PROXY=\"$HTTP_PROXY\" yarn install --immutable"
        );
        let deps = r#"{"devDependencies":{"mocha":"10.7.3"}}"#;
        let berry_lock =
            "# This file is generated by running \"yarn install\"\n\n__metadata:\n  version: 8\n";
        let (_d, secure, root) = repo(&[("package.json", deps), ("yarn.lock", berry_lock)]);
        assert_eq!(
            install_for(&secure, &root, Toolchain::Node, None)
                .unwrap()
                .command,
            "YARN_HTTPS_PROXY=\"$HTTPS_PROXY\" YARN_HTTP_PROXY=\"$HTTP_PROXY\" yarn install --immutable"
        );
        let classic = r#"{"packageManager":"yarn@1.22.22","devDependencies":{"mocha":"1"}}"#;
        let (_d, secure, root) = repo(&[
            ("package.json", classic),
            ("yarn.lock", "# yarn lockfile v1\n"),
        ]);
        assert_eq!(
            install_for(&secure, &root, Toolchain::Node, None)
                .unwrap()
                .command,
            "yarn install --frozen-lockfile --prefer-offline"
        );
    }
}
