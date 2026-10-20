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
/// environment assignment or `env`/`exec`/`time`/`command`, and for
/// `sh -c '<command>'` the program of that command.
pub fn check_program(command: &str) -> Option<String> {
    let words = shell_words(command);
    let mut words = words.iter().map(String::as_str).peekable();
    while let Some(word) = words.next() {
        if is_assignment(word) || matches!(word, "env" | "exec" | "time" | "command" | "--") {
            continue;
        }
        let name = word.rsplit('/').next().unwrap_or(word);
        if matches!(name, "sh" | "bash" | "dash") && words.peek() == Some(&"-c") {
            words.next();
            return words.next().and_then(check_program);
        }
        return Some(name.to_string());
    }
    None
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
    let names = candidates
        .iter()
        .map(|candidate| candidate.toolchain.name())
        .collect::<Vec<_>>()
        .join("|");
    let check = match check {
        Some(check) => format!("the check `{check}` does not say which one it needs"),
        None => "no check command was found to say which one it needs".into(),
    };
    format!(
        "the repository has the files of several toolchains: {found}, and {check}. Pass \
         --toolchain {names} (or --toolchain none for the plain image), or give the check with \
         --check \"<command>\""
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

/// A Python repository's check: `pytest` when it uses pytest, else the
/// standard library's `unittest` (G84: a repository that never mentions
/// pytest is not given a check it cannot run).
pub fn python_check(dir: &SecureDir, root: &RootFiles) -> String {
    if declares_pytest(dir, root) {
        return "pytest".into();
    }
    // `python3 -m` puts the repository root on the import path. A tests
    // directory that is not a package is discovered from itself.
    let tests_dir = root.has_dir("tests") && !dir.is_file("tests/__init__.py").unwrap_or(true);
    if tests_dir {
        "python3 -m unittest discover -v -s tests".into()
    } else {
        "python3 -m unittest discover -v".into()
    }
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
        (
            "yarn install --frozen-lockfile --prefer-offline",
            Some(lockfile),
        )
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

/// The keys of the tables `[<table>]` of a TOML text, read line by line.
/// Enough to find which extras and groups a `pyproject.toml` declares.
fn toml_table_keys(text: &str, table: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let mut inside = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            inside = line
                .trim_start_matches('[')
                .trim_end_matches(']')
                .trim()
                .eq(table)
                && !line.starts_with("[[");
            continue;
        }
        if !inside || line.is_empty() || line.starts_with('#') {
            continue;
        }
        let key = if let Some(rest) = line.strip_prefix('"') {
            rest.split_once('"')
                .filter(|(_, after)| after.trim_start().starts_with('='))
                .map(|(key, _)| key.to_string())
        } else {
            line.split_once('=')
                .map(|(key, _)| key.trim().to_string())
                .filter(|key| {
                    !key.is_empty()
                        && key.bytes().all(|b| {
                            b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.'
                        })
                })
        };
        keys.extend(key);
    }
    keys
}

/// Whether a TOML text has the table `[<table>]`.
fn toml_has_table(text: &str, table: &str) -> bool {
    text.lines().any(|line| {
        let line = line.trim();
        line.starts_with('[')
            && !line.starts_with("[[")
            && line.trim_start_matches('[').trim_end_matches(']').trim() == table
    })
}

/// The extras of a `setup.cfg` (`[options.extras_require]`).
fn setup_cfg_extras(text: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let mut inside = false;
    for line in text.lines() {
        if line.trim_start().starts_with('[') {
            inside = line.trim() == "[options.extras_require]";
            continue;
        }
        if inside && !line.starts_with([' ', '\t']) {
            if let Some((key, _)) = line.split_once(['=', ':']) {
                let key = key.trim();
                if !key.is_empty() && !key.starts_with(['#', ';']) {
                    keys.push(key.to_string());
                }
            }
        }
    }
    keys
}

const TEST_NAMES: [&str; 4] = ["test", "tests", "testing", "dev"];

fn first_test_name(names: &[String]) -> Option<String> {
    TEST_NAMES
        .iter()
        .find(|wanted| names.iter().any(|name| name == *wanted))
        .map(|name| name.to_string())
}

fn python_install(dir: &SecureDir, root: &RootFiles, check: Option<&str>) -> Option<Install> {
    let pyproject = read_text(dir, "pyproject.toml");
    let setup_cfg = read_text(dir, "setup.cfg");
    let mut inputs = Vec::new();
    let mut args = Vec::new();
    let installable = pyproject
        .as_deref()
        .is_some_and(|text| toml_has_table(text, "project") || toml_has_table(text, "tool.poetry"))
        || root.has("setup.py")
        || setup_cfg
            .as_deref()
            .is_some_and(|text| text.contains("[metadata]"));
    if installable {
        let mut extras = pyproject
            .as_deref()
            .map(|text| toml_table_keys(text, "project.optional-dependencies"))
            .unwrap_or_default();
        extras.extend(
            setup_cfg
                .as_deref()
                .map(setup_cfg_extras)
                .unwrap_or_default(),
        );
        args.push(match first_test_name(&extras) {
            Some(extra) => format!("-e '.[{extra}]'"),
            None => "-e .".to_string(),
        });
    }
    if let Some(group) = pyproject
        .as_deref()
        .map(|text| toml_table_keys(text, "dependency-groups"))
        .and_then(|groups| first_test_name(&groups))
    {
        args.push(format!("--group {}", shell_quote(&group)));
    }
    for file in root.requirements_files() {
        args.push(format!("-r {}", shell_quote(&file)));
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
    if runs_pytest {
        let listed = inputs
            .iter()
            .filter_map(|name| read_text(dir, name))
            .any(|text| lists_pytest(&text));
        if !listed {
            args.push("pytest".into());
        }
    }
    if args.is_empty() {
        return None;
    }
    Some(Install {
        command: format!("pip install {}", args.join(" ")),
        inputs,
    })
}

/// Whether a manifest or requirements file lists pytest as a requirement
/// (`pytest`, `pytest>=8`, `"pytest-cov"`), as opposed to configuring it
/// (`[tool.pytest.ini_options]`).
fn lists_pytest(text: &str) -> bool {
    text.lines().any(|line| {
        let line = line.trim().to_ascii_lowercase();
        let starts = |text: &str| {
            text.strip_prefix("pytest").is_some_and(|rest| {
                rest.is_empty()
                    || !rest.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_')
            })
        };
        starts(&line)
            || line
                .match_indices(['"', '\''])
                .any(|(at, _)| starts(&line[at + 1..]))
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

/// The shell script a toolchain's setup runs (as the Session's writer, in
/// the repository): `command`, after what the toolchain needs first (a
/// Python virtual environment in the cache). With a `key`, the script skips
/// `command` when the Workspace's cache holds that key from an earlier
/// setup, and records it after `command` succeeds.
pub fn setup_script(toolchain: Toolchain, command: &str, key: Option<&str>) -> String {
    let prepare = match toolchain {
        Toolchain::Python => {
            Some("{ [ -x \"$VIRTUAL_ENV/bin/python\" ] || python3 -m venv \"$VIRTUAL_ENV\"; }")
        }
        _ => None,
    };
    let body = match prepare {
        Some(prepare) => format!("{prepare} && {command}"),
        None => command.to_string(),
    };
    let Some(key) = key else {
        return body;
    };
    let name = toolchain.name();
    let reset = match toolchain {
        // A different image or dependency set starts from a new environment.
        Toolchain::Python => "rm -rf \"$VIRTUAL_ENV\" && ",
        _ => "",
    };
    format!(
        "stamp=\"$AXOCOATL_CACHE/{name}/setup.stamp\"; key={key}; \
         if [ -f \"$stamp\" ] && [ \"$(cat \"$stamp\")\" = \"$key\" ]; then \
         echo \"axocoatl: nothing to install: the dependency files and the image are unchanged \
         since this Workspace's last {name} setup\"; \
         else rm -f \"$stamp\" && {reset}{body} && mkdir -p \"$AXOCOATL_CACHE/{name}\" && \
         printf '%s\\n' \"$key\" > \"$stamp\"; fi"
    )
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
        assert!(message.contains("--toolchain node|python|go"), "{message}");
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
        assert_eq!(detect_check_in(&secure, &root), Some("pytest".into()));
        let (_d, secure, root) = repo(&[("setup.py", ""), ("tests/conftest.py", "")]);
        assert_eq!(detect_check_in(&secure, &root), Some("pytest".into()));
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
        assert!(lists_pytest("pytest>=8\n"));
        assert!(lists_pytest("dev = [\"ruff\", 'pytest-cov']"));
        assert!(!lists_pytest("[tool.pytest.ini_options]\naddopts = '-q'"));
        assert!(!lists_pytest("pytest_plugins = []"));
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
    fn setup_scripts_prepare_the_toolchain_and_skip_unchanged_installs() {
        assert_eq!(setup_script(Toolchain::Node, "npm ci", None), "npm ci");
        let python = setup_script(Toolchain::Python, "pip install -e .", None);
        assert!(
            python.starts_with("{ [ -x \"$VIRTUAL_ENV/bin/python\" ]"),
            "{python}"
        );
        assert!(python.ends_with("&& pip install -e ."), "{python}");
        let stamped = setup_script(Toolchain::Python, "pip install -e .", Some("abc123"));
        assert!(stamped.contains("key=abc123"), "{stamped}");
        assert!(
            stamped.contains("$AXOCOATL_CACHE/python/setup.stamp"),
            "{stamped}"
        );
        assert!(
            stamped.contains("rm -rf \"$VIRTUAL_ENV\" && {"),
            "{stamped}"
        );
        assert!(stamped.contains("nothing to install"), "{stamped}");
        let cargo = setup_script(Toolchain::Rust, "cargo fetch", Some("k"));
        assert!(
            cargo.contains("else rm -f \"$stamp\" && cargo fetch && mkdir -p"),
            "{cargo}"
        );
    }

    /// The setup scripts run under a POSIX shell: write the stamp once,
    /// then skip.
    #[test]
    fn a_stamped_setup_runs_once_per_key() {
        let cache = tempfile::tempdir().unwrap();
        let marker = cache.path().join("ran");
        let command = format!("echo x >> {}", marker.display());
        let script = setup_script(Toolchain::Go, &command, Some("k1"));
        let run = |script: &str| {
            let output = std::process::Command::new("sh")
                .arg("-c")
                .arg(script)
                .env("AXOCOATL_CACHE", cache.path())
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            String::from_utf8_lossy(&output.stdout).into_owned()
        };
        run(&script);
        let second = run(&script);
        assert!(second.contains("nothing to install"), "{second}");
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "x\n");
        run(&setup_script(Toolchain::Go, &command, Some("k2")));
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "x\nx\n");
    }
}
