//! A loadout run's toolchain: the image its Session runs, the dependency
//! install its setup runs, the registries that setup alone may reach, and
//! the check its `detected` checks run. Owner: workstream `toolchain`
//! (docs: sites/docs/src/content/docs/configure/toolchains.mdx).
//!
//! [`plan`] is pure and host-side: it reads a few files at the repository
//! root (never running repository code) and the run's flags and loadout,
//! and says what admission will do, or why the run is refused (a usage
//! error, exit 3). The daemon then builds or finds the image, runs the setup
//! with the registries open, closes them, and probes the check before the
//! first model call (`bootstrap_loadout_toolchain.rs`).
//!
//! Precedence:
//!
//! - toolchain: `--toolchain`, else the loadout's `environment.toolchain`
//!   or a toolchain among its `environment.recipes`, else none when an image
//!   is named (`--image` or `environment.image`), else detected from the
//!   repository when the loadout has a `detected` check, else none;
//! - image: `--image`, else the loadout's `environment.image`, else the
//!   recipe image of the loadout's recipes plus the toolchain;
//! - setup: `--no-setup` (none), else `--setup`, else the loadout's
//!   `environment.setup`, else the toolchain's detected install;
//! - check: `--check`, else a `Check command:` line of `AXOCOATL.md`, else
//!   detected from the repository.
//!
//! Without a toolchain (`--toolchain none`) a run is what it was before
//! toolchains: the setup runs through the Session's environment with only
//! the configured egress lists.
//!
//! Node, Python, Rust and Go are planned alike. A loadout Session is always
//! hardened; from Landlock ABI 2 the hardened supervisor's domain allows the
//! cross-directory renames cargo and rustc make for every build. A kernel
//! with only ABI 1 still refuses them with EXDEV ("Invalid cross-device
//! link"), which the check probe names ([`ProbeMissing::Sandbox`]).

use std::path::Path;

use axocoatl_config::loadout::LoadoutFile;
use axocoatl_core::SecureDir;
use axocoatl_session::toolchain::{self as detect, Choice, RootFiles, Toolchain};
use serde::{Deserialize, Serialize};

/// The run's own environment choices (`axocoatl run --toolchain --image
/// --no-setup --no-build`), part of `POST /api/runs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentRequest {
    /// `detected`, `none`, `node`, `python`, `rust` or `go`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub toolchain: Option<String>,
    /// An image reference to run as it is (no toolchain unless named).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Run no setup, not even the detected install.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_setup: bool,
    /// Do not probe the check before the first model call: a check the
    /// probe takes for one that cannot run still gets its run.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_probe: bool,
    /// Build a missing toolchain image during admission. `false` refuses
    /// the run instead, naming the `axocoatl recipe build` command.
    #[serde(default = "build_images_default", skip_serializing_if = "is_true")]
    pub build_images: bool,
}

fn build_images_default() -> bool {
    true
}

fn is_true(value: &bool) -> bool {
    *value
}

impl Default for EnvironmentRequest {
    fn default() -> Self {
        Self {
            toolchain: None,
            image: None,
            no_setup: false,
            no_probe: false,
            build_images: true,
        }
    }
}

impl EnvironmentRequest {
    /// Whether the request says nothing (the field is then left out).
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Where a setup command came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SetupSource {
    /// The toolchain's install, detected from the repository.
    Detected,
    /// `--setup`.
    Flag,
    /// The loadout's `environment.setup`.
    Loadout,
}

impl SetupSource {
    pub fn as_str(self) -> &'static str {
        match self {
            SetupSource::Detected => "detected",
            SetupSource::Flag => "--setup",
            SetupSource::Loadout => "loadout",
        }
    }
}

/// The toolchain setup a run runs before its first model call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupPlan {
    /// As approved and shown.
    pub command: String,
    pub source: SetupSource,
    /// What runs (`sh -c`, as the writer, in the repository).
    pub script: String,
    /// The registry hosts and ports the setup alone may reach.
    pub registries: Vec<(String, Vec<u16>)>,
    /// The detected install's [`detect::setup_key`]: when the Workspace's
    /// cache holds it from an earlier setup, the setup does not run.
    pub key: Option<String>,
}

impl SetupPlan {
    pub fn registry_hosts(&self) -> Vec<String> {
        self.registries
            .iter()
            .map(|(host, _)| host.clone())
            .collect()
    }
}

/// What admission decided about a run's environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentPlan {
    pub toolchain: Option<Toolchain>,
    /// Why this toolchain (or none).
    pub reason: String,
    /// The recipes of the run's image; empty when it runs `image` or the
    /// default image.
    pub recipes: Vec<String>,
    /// An image reference to run as it is.
    pub image: Option<String>,
    /// The check command for `detected` checks, when there is one, and
    /// where it came from.
    pub check: Option<(String, detect::CheckSource)>,
    /// The toolchain's setup (only with a toolchain).
    pub setup: Option<SetupPlan>,
    /// A setup for the Session's environment as before toolchains (only
    /// without one): `--setup` or the loadout's, with the configured egress
    /// lists only.
    pub plain_setup: Option<String>,
    /// The loadout's `sandbox.network`: under `none` no registry opens for
    /// the setup.
    pub network: String,
    /// What the check probe reads the check's output with.
    pub probe: ProbeHints,
}

/// What the check probe needs, read from the repository on the host, to
/// tell a check that cannot run in the image from one that runs and fails.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProbeHints {
    /// The check's own program (looked up with `command -v`).
    pub program: Option<String>,
    /// The program of the `package.json` script the check runs
    /// (`npm test` running `mocha`).
    pub script_program: Option<String>,
    /// The module of `python -m <module>`.
    pub runner_module: Option<String>,
    /// The Python packages the repository declares (normalized names).
    pub declared_packages: Vec<String>,
    /// The repository's test files for the run's toolchain (some of them).
    pub test_files: Vec<String>,
}

impl EnvironmentPlan {
    /// The image as a person reads it: the recipe image name, the named
    /// image, or the default.
    pub fn image_text(&self) -> String {
        if let Some(image) = &self.image {
            return image.clone();
        }
        if !self.recipes.is_empty() {
            return axocoatl_isolation::recipes::image_name(&self.recipes)
                .unwrap_or_else(|_| self.recipes.join(" "));
        }
        axocoatl_isolation::session_sandbox::DEFAULT_IMAGE.to_string()
    }

    /// The `axocoatl recipe build` command for the run's image.
    pub fn build_command(&self) -> Option<String> {
        let names = axocoatl_isolation::recipes::canonical_names(&self.recipes).ok()?;
        (!names.is_empty()).then(|| format!("axocoatl recipe build {}", names.join(" ")))
    }

    /// One line for the run's start: `image X · toolchain Y (why) · check Z
    /// · setup W`.
    pub fn summary(&self) -> String {
        let toolchain = match self.toolchain {
            Some(toolchain) => format!("toolchain {} ({})", toolchain.name(), self.reason),
            None => format!("no toolchain ({})", self.reason),
        };
        let check = match &self.check {
            Some((check, source)) => format!("check {check} ({})", source.describe()),
            None => "no repository check".into(),
        };
        let setup = match (&self.setup, &self.plain_setup) {
            (Some(setup), _) if self.network != "egress" => format!(
                "setup {} ({}; no registry: the loadout's network is {})",
                setup.command,
                setup.source.as_str(),
                self.network
            ),
            (Some(setup), _) => format!(
                "setup {} ({}; registries {} open for it alone)",
                setup.command,
                setup.source.as_str(),
                if setup.registries.is_empty() {
                    "none".to_string()
                } else {
                    setup.registry_hosts().join(", ")
                }
            ),
            (None, Some(plain)) => format!("setup {plain}"),
            (None, None) => "setup none".into(),
        };
        format!(
            "image {} · {toolchain} · {check} · {setup}",
            self.image_text()
        )
    }
}

/// The inputs of [`plan`] besides the repository.
#[derive(Debug, Clone, Copy)]
pub struct PlanInput<'a> {
    pub file: &'a LoadoutFile,
    pub request: &'a EnvironmentRequest,
    /// `--check`.
    pub check_flag: Option<&'a str>,
    /// `--setup`.
    pub setup_flag: Option<&'a str>,
    /// The loadout's `environment.setup`, its parameter resolved.
    pub loadout_setup: Option<&'a str>,
}

enum Mode {
    None(String),
    Fixed(Toolchain, String),
    Detect,
}

fn nonempty(text: Option<&str>) -> Option<&str> {
    text.map(str::trim).filter(|text| !text.is_empty())
}

/// Decide a run's environment. `Err` is a usage error (exit 3) in words.
pub fn plan(repo: &Path, input: PlanInput<'_>) -> Result<EnvironmentPlan, String> {
    let file = input.file;
    let request = input.request;
    let environment = file.environment.clone().unwrap_or_default();
    let has_detected_check = file.checks.iter().any(|check| check.run.detected);
    let image_flag = nonempty(request.image.as_deref());
    let toolchain_flag = nonempty(request.toolchain.as_deref());
    if let Some(value) = toolchain_flag {
        if !axocoatl_config::loadout::TOOLCHAIN_VALUES.contains(&value) {
            return Err(format!(
                "--toolchain {value:?}: use one of {}",
                axocoatl_config::loadout::TOOLCHAIN_VALUES.join(", ")
            ));
        }
    }
    if let Some(image) = image_flag {
        if image.starts_with('-') || image.chars().any(|c| c.is_control() || c.is_whitespace()) {
            return Err(format!("--image {image:?} is not an image reference"));
        }
        if toolchain_flag.is_some_and(|value| !matches!(value, "none" | "detected")) {
            return Err(
                "give --toolchain or --image, not both: --toolchain runs Axocoatl's image of \
                 that language, --image runs your image as it is (give its installs with --setup)"
                    .into(),
            );
        }
    }
    let loadout_language = environment
        .recipes
        .iter()
        .find(|recipe| Toolchain::parse(recipe).is_some())
        .cloned();
    let fixed = |name: &str, why: String| -> Mode {
        match Toolchain::parse(name) {
            Some(toolchain) => Mode::Fixed(toolchain, why),
            None if name == "detected" => Mode::Detect,
            None => Mode::None(why),
        }
    };
    let mode =
        match toolchain_flag {
            Some("detected") if image_flag.is_some() => return Err(
                "--toolchain detected chooses Axocoatl's image; --image runs your own. Give one"
                    .into(),
            ),
            Some(value) => fixed(value, format!("--toolchain {value}")),
            None if image_flag.is_some() => Mode::None("--image runs the image as it is".into()),
            None => match (&environment.toolchain, &loadout_language) {
                (Some(value), _) => fixed(value, format!("the loadout's toolchain: {value}")),
                (None, Some(language)) => {
                    fixed(language, format!("the loadout's recipes name {language}"))
                }
                (None, None) if environment.image.is_some() => {
                    Mode::None("the loadout names its image".into())
                }
                (None, None) if has_detected_check => Mode::Detect,
                (None, None) => Mode::None("the loadout runs no repository check".into()),
            },
        };
    let dir =
        SecureDir::open(repo).map_err(|error| format!("repository {}: {error}", repo.display()))?;
    let root = RootFiles::read(&dir).unwrap_or_default();
    // The check `detected` checks run: `--check`, AXOCOATL.md, or detected.
    let check = if !has_detected_check {
        None
    } else if let Some(check) = nonempty(input.check_flag) {
        Some((check.to_string(), detect::CheckSource::Flag))
    } else {
        detect::axocoatl_md_check(&dir)
            .map(|check| (check, detect::CheckSource::AxocoatlMd))
            .or_else(|| {
                detect::detect_check_in(&dir, &root)
                    .map(|check| (check, detect::CheckSource::Detected))
            })
    };
    let (toolchain, reason) = match mode {
        Mode::None(why) => (None, why),
        Mode::Fixed(toolchain, why) => (Some(toolchain), why),
        Mode::Detect => {
            let candidates = detect::candidates(&root);
            match detect::choose(&candidates, check.as_ref().map(|(check, _)| check.as_str())) {
                Choice::Chosen { toolchain, reason } => (Some(toolchain), reason),
                Choice::Nothing { reason } => (None, reason),
                Choice::Ambiguous { candidates } => {
                    return Err(detect::ambiguous_message(
                        &candidates,
                        check.as_ref().map(|(check, _)| check.as_str()),
                    ))
                }
            }
        }
    };
    let explicit_setup = if request.no_setup {
        None
    } else if let Some(setup) = nonempty(input.setup_flag) {
        Some((setup.to_string(), SetupSource::Flag))
    } else {
        nonempty(input.loadout_setup).map(|setup| (setup.to_string(), SetupSource::Loadout))
    };
    let network = file.sandbox.network.clone();
    let probe = probe_hints(
        &dir,
        &root,
        toolchain,
        check.as_ref().map(|(c, _)| c.as_str()),
    );
    let Some(toolchain) = toolchain else {
        return Ok(EnvironmentPlan {
            toolchain: None,
            reason,
            recipes: if image_flag.is_some() {
                Vec::new()
            } else {
                environment.recipes.clone()
            },
            image: image_flag
                .map(str::to_string)
                .or_else(|| environment.image.clone()),
            check,
            setup: None,
            plain_setup: explicit_setup.map(|(setup, _)| setup),
            network,
            probe,
        });
    };
    let mut recipes: Vec<String> = environment
        .recipes
        .iter()
        .filter(|recipe| Toolchain::parse(recipe).is_none())
        .cloned()
        .collect();
    recipes.push(toolchain.name().to_string());
    let image_name = axocoatl_isolation::recipes::image_name(&recipes)
        .map_err(|error| format!("the run's image: {error}"))?;
    let check_command = check.as_ref().map(|(check, _)| check.as_str());
    let setup = match explicit_setup {
        Some((command, source)) => Some(SetupPlan {
            script: detect::setup_script(toolchain, &command),
            command,
            source,
            registries: Vec::new(),
            key: None,
        }),
        None if request.no_setup => None,
        None => detect::install_for(&dir, &root, toolchain, check_command).map(|install| {
            let key = toolchain.keeps_setup_stamp().then(|| {
                detect::setup_key(
                    &dir,
                    toolchain,
                    &image_name,
                    &install.command,
                    &install.inputs,
                )
            });
            SetupPlan {
                script: detect::setup_script(toolchain, &install.command),
                command: install.command,
                source: SetupSource::Detected,
                registries: Vec::new(),
                key,
            }
        }),
    };
    let setup = setup.map(|mut setup| {
        let mut presets: Vec<&str> = toolchain.registry_presets().to_vec();
        // yarn.lock resolves packages at registry.yarnpkg.com.
        if toolchain == Toolchain::Node && root.has("yarn.lock") {
            presets.push("yarn");
        }
        setup.registries = presets
            .iter()
            .filter_map(|name| axocoatl_config::egress_presets::preset(name))
            .flat_map(|preset| preset.hosts.iter())
            .map(|(host, ports)| (host.to_string(), ports.to_vec()))
            .collect();
        setup
    });
    Ok(EnvironmentPlan {
        toolchain: Some(toolchain),
        reason,
        recipes,
        image: None,
        check,
        setup,
        plain_setup: None,
        network,
        probe,
    })
}

/// The probe's hints for `check`, read on the host.
fn probe_hints(
    dir: &SecureDir,
    root: &RootFiles,
    toolchain: Option<Toolchain>,
    check: Option<&str>,
) -> ProbeHints {
    let Some(check) = check else {
        return ProbeHints::default();
    };
    let program = detect::check_program(check);
    let language = toolchain
        .or_else(|| program.as_deref().and_then(detect::toolchain_for_program))
        .or_else(|| detect::choose(&detect::candidates(root), Some(check)).toolchain());
    ProbeHints {
        script_program: detect::package_script_program(dir, check),
        runner_module: detect::python_module_of(check),
        declared_packages: if detect::has_python_files(root) {
            detect::declared_python_packages(dir, root)
        } else {
            Vec::new()
        },
        test_files: language
            .map(|language| detect::test_files(dir, language))
            .unwrap_or_default(),
        program,
    }
}

/// Why a check cannot run in the run's image, found by the probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeMissing {
    /// The check's program, or the test runner it starts, is not on the
    /// writer's PATH.
    Program(String),
    /// The Python module the check runs (`python -m <module>`), or a
    /// package the repository declares, is not installed.
    Module(String),
    /// The check ran no test although the repository has test files: it
    /// does not find them, so no change can make it pass.
    NoTests { files: Vec<String> },
    /// The Session's sandbox refuses what the check does: a cross-directory
    /// rename under the hardened supervisor's Landlock domain on a kernel
    /// with only Landlock ABI 1 (before Linux 5.19), which cannot allow it.
    Sandbox(String),
}

impl ProbeMissing {
    pub fn describe(&self) -> String {
        match self {
            ProbeMissing::Program(program) => format!("the program `{program}` was not found"),
            ProbeMissing::Module(module) => {
                format!("the Python module `{module}` is not installed")
            }
            ProbeMissing::NoTests { files } => format!(
                "it ran no test, although the repository has test files ({}{})",
                files.iter().take(3).cloned().collect::<Vec<_>>().join(", "),
                if files.len() > 3 { ", ..." } else { "" }
            ),
            ProbeMissing::Sandbox(what) => format!("the Session's sandbox refuses it: {what}"),
        }
    }

    /// Whether the refusal is the environment's (exit 5) rather than the
    /// run's request (exit 3).
    pub fn is_environment(&self) -> bool {
        matches!(self, ProbeMissing::Sandbox(_))
    }
}

/// The program a shell said it could not find: `sh: 1: mocha: not found`,
/// `sh: mocha: not found`, `bash: line 1: mocha: command not found`,
/// `/bin/sh: pytest: not found`.
fn shell_not_found(line: &str) -> Option<String> {
    let line = line.trim();
    let rest = line
        .strip_suffix(": command not found")
        .or_else(|| line.strip_suffix(": not found"))?;
    let (shell, rest) = rest.split_once(": ")?;
    let shell = shell.rsplit('/').next().unwrap_or(shell);
    if !matches!(shell, "sh" | "bash" | "dash" | "ash" | "zsh") {
        return None;
    }
    let program = match rest.rsplit_once(": ") {
        // `1: mocha` or `line 1: mocha`
        Some((position, program))
            if position
                .trim_start_matches("line ")
                .bytes()
                .all(|b| b.is_ascii_digit()) =>
        {
            program
        }
        Some(_) => return None,
        None => rest,
    };
    let program = program.trim();
    (!program.is_empty() && !program.contains(char::is_whitespace)).then(|| program.to_string())
}

/// The module of `No module named 'x'` / `No module named x`.
fn missing_module(line: &str) -> Option<String> {
    let (_, rest) = line.split_once("No module named ")?;
    let name = rest
        .trim()
        .trim_matches(|c| c == '\'' || c == '"')
        .split(['\'', '"', ' '])
        .next()?
        .trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// Whether `task` names `module` (its top-level package) as a word: a
/// module the task asks to create is missing on the untouched tree by
/// design.
fn task_names(task: &str, module: &str) -> bool {
    let top = module.split('.').next().unwrap_or(module);
    task.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .any(|word| word.eq_ignore_ascii_case(top))
}

/// Whether the output says the check ran no test at all: unittest's `Ran 0
/// tests`, pytest's exit 5 (`no tests ran`), mocha's, jest's and vitest's
/// "no test files".
fn ran_no_tests(exit_code: Option<i32>, output: &str) -> bool {
    output.lines().any(|line| {
        let line = line.trim();
        (line.starts_with("Ran 0 tests in ") && exit_code != Some(0))
            || line.starts_with("Error: No test files found")
            || line.starts_with("No tests found, exiting with code")
            || line.starts_with("No test files found, exiting with code")
    }) || (exit_code == Some(5)
        && (output.contains("no tests ran") || output.contains("collected 0 items")))
}

/// What the probe reads a check's output with.
#[derive(Debug, Clone, Copy)]
pub struct ProbeContext<'a> {
    pub hints: &'a ProbeHints,
    /// The run's task: a module it names is missing by design.
    pub task: &'a str,
}

/// What the probe's check run on the untouched tree says: `Some` when the
/// check cannot run (or cannot pass) in this image, whatever the writer
/// changes in the repository. A check that runs and fails is what a fix run
/// is for, and is not a refusal: a missing program counts only when it is
/// the check's own, the test runner its `package.json` script starts, or a
/// known toolchain or test-runner program (never a path, which the writer
/// may create); a missing module only when it is the one `python -m` runs
/// or a package the repository declares (never a typo or a module the task
/// creates).
pub fn classify_probe(
    exit_code: Option<i32>,
    stdout: &str,
    stderr: &str,
    context: ProbeContext<'_>,
) -> Option<ProbeMissing> {
    let hints = context.hints;
    let lines = || stderr.lines().chain(stdout.lines());
    if exit_code != Some(0) && lines().any(|line| line.contains("Invalid cross-device link")) {
        return Some(ProbeMissing::Sandbox(
            "a rename into another directory failed with \"Invalid cross-device link\"".into(),
        ));
    }
    let named = |program: &str| {
        hints.program.as_deref() == Some(program)
            || hints.script_program.as_deref() == Some(program)
            || detect::toolchain_for_program(program).is_some()
    };
    for line in lines() {
        if let Some(program) = shell_not_found(line) {
            if !program.contains('/') && named(&program) {
                return Some(ProbeMissing::Program(program));
            }
        }
    }
    for line in lines() {
        if let Some(module) = missing_module(line) {
            let top = module.split('.').next().unwrap_or(&module);
            let runner = hints
                .runner_module
                .as_deref()
                .is_some_and(|runner| runner.split('.').next() == Some(top));
            let declared = hints
                .declared_packages
                .contains(&detect::normalize_package(top));
            if runner || (declared && !task_names(context.task, &module)) {
                return Some(ProbeMissing::Module(module));
            }
        }
    }
    if exit_code == Some(127) {
        if let Some(program) = hints.program.as_deref().filter(|p| !p.contains('/')) {
            if !lines().any(|line| shell_not_found(line).is_some()) {
                return Some(ProbeMissing::Program(program.to_string()));
            }
        }
    }
    if !hints.test_files.is_empty() && ran_no_tests(exit_code, &format!("{stdout}\n{stderr}")) {
        return Some(ProbeMissing::NoTests {
            files: hints.test_files.clone(),
        });
    }
    None
}

/// Programs no setup can install: a setup runs as the Session's non-root
/// writer, with only the language's package registries open.
fn system_program(program: &str) -> bool {
    matches!(
        program,
        "make"
            | "gcc"
            | "cc"
            | "g++"
            | "clang"
            | "cmake"
            | "bash"
            | "curl"
            | "wget"
            | "zip"
            | "unzip"
            | "java"
            | "ruby"
            | "php"
            | "perl"
            | "docker"
            | "podman"
    )
}

/// The usage error of a check that cannot run in the run's image.
pub fn probe_refusal(
    plan: &EnvironmentPlan,
    check: &str,
    missing: &ProbeMissing,
    setup_note: Option<&str>,
) -> String {
    let toolchain = match plan.toolchain {
        Some(toolchain) => format!("the {} toolchain ({})", toolchain.name(), plan.reason),
        None => format!("no toolchain ({})", plan.reason),
    };
    let declare = match plan.toolchain {
        Some(Toolchain::Node) => "declare it in package.json's devDependencies",
        Some(Toolchain::Python) => {
            "declare it in pyproject.toml's dependencies or test extra, or in a requirements file"
        }
        Some(Toolchain::Go) => "declare it in go.mod",
        Some(Toolchain::Rust) => "declare it in Cargo.toml",
        None => "declare it in the project's dependencies",
    };
    let mut fixes = Vec::new();
    match missing {
        // Without a toolchain no setup reaches a package registry.
        ProbeMissing::Module(_) if plan.toolchain.is_none() => fixes.push(
            "pass --toolchain python for Axocoatl's Python image, whose setup installs the \
             project's dependencies"
                .into(),
        ),
        ProbeMissing::Module(module) => fixes.push(format!(
            "{declare} (the package that provides {module}), or give its install with --setup \
             \"pip install PACKAGE\", where PACKAGE provides {module}"
        )),
        ProbeMissing::Program(program) if system_program(program) => fixes.push(format!(
            "pass --check \"COMMAND\" for a check that does not need {program}, or --image \
             IMAGE of an image that has it ({program} is a system program no setup can install)"
        )),
        ProbeMissing::Program(program) if plan.toolchain.is_some() => fixes.push(format!(
            "{declare}, or give its install with --setup \"COMMAND\", where COMMAND installs \
             {program}"
        )),
        ProbeMissing::NoTests { .. } => fixes.push(
            "pass --check \"COMMAND\" (or write a `Check command:` line in AXOCOATL.md) \
             with the command that runs the repository's tests"
                .into(),
        ),
        ProbeMissing::Sandbox(_) => fixes.push(
            "pass --check \"COMMAND\" for a check that does not need it; on a Linux kernel \
             before 5.19 (Landlock ABI 1) Axocoatl's sandbox refuses every rename into another \
             directory, a limit of the sandbox, not of the repository"
                .into(),
        ),
        ProbeMissing::Program(_) => {}
    }
    if !matches!(
        missing,
        ProbeMissing::NoTests { .. } | ProbeMissing::Sandbox(_)
    ) {
        if plan.toolchain.is_none() {
            // A missing module already named the Python toolchain.
            if !matches!(missing, ProbeMissing::Module(_)) {
                // A placeholder with the choices in words: `node|python`
                // pasted into a shell is a pipeline.
                fixes.push(
                    "pass --toolchain LANGUAGE for Axocoatl's image of the repository's \
                     language, where LANGUAGE is node, python, rust or go"
                        .into(),
                );
            }
        } else {
            fixes.push(
                "pass --toolchain LANGUAGE when the check runs another language, where LANGUAGE \
                 is node, python, rust or go"
                    .into(),
            );
        }
        fixes.push("pass --image IMAGE of an image that has it".into());
        fixes.push("pass --check \"COMMAND\" for a check this image can run".into());
        fixes.push("use a loadout whose environment provides it".into());
    }
    fixes.push("or pass --no-probe to run anyway when the probe is wrong about this check".into());
    let cannot = match missing {
        ProbeMissing::NoTests { .. } => "can never pass in this run",
        ProbeMissing::Sandbox(_) => "cannot run in this run's Session",
        _ => "cannot run in this run's image",
    };
    format!(
        "the check `{check}` {cannot}: {}. Image: {}, with {toolchain}.{} No writer or reviewer \
         turn ran. To fix it: {}",
        missing.describe(),
        plan.image_text(),
        setup_note
            .map(|note| format!(" Setup: {note}."))
            .unwrap_or_default(),
        fixes.join("; ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axocoatl_config::loadout::{builtin_loadouts, Loadout};

    fn builtin(id: &str) -> Loadout {
        builtin_loadouts()
            .into_iter()
            .map(|loadout| loadout.unwrap())
            .find(|loadout| loadout.file.id == id)
            .unwrap()
    }

    fn repo(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, contents) in files {
            let path = dir.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }
        dir
    }

    fn run_plan(
        dir: &Path,
        id: &str,
        request: EnvironmentRequest,
        check: Option<&str>,
        setup: Option<&str>,
    ) -> Result<EnvironmentPlan, String> {
        let loadout = builtin(id);
        plan(
            dir,
            PlanInput {
                file: &loadout.file,
                request: &request,
                check_flag: check,
                setup_flag: setup,
                loadout_setup: None,
            },
        )
    }

    const MOCHA: &str = r#"{"scripts":{"test":"mocha"},"devDependencies":{"mocha":"10.7.3"}}"#;

    #[test]
    fn fix_detects_the_repository_toolchain_its_install_and_its_check() {
        let dir = repo(&[("package.json", MOCHA), ("package-lock.json", "{}")]);
        let plan = run_plan(dir.path(), "fix", EnvironmentRequest::default(), None, None).unwrap();
        assert_eq!(plan.toolchain, Some(Toolchain::Node));
        assert_eq!(plan.recipes, ["node"]);
        assert_eq!(plan.image, None);
        assert!(plan
            .image_text()
            .starts_with("localhost/axocoatl-recipe-node:"));
        assert_eq!(
            plan.check,
            Some(("npm run test".into(), detect::CheckSource::Detected))
        );
        let setup = plan.setup.as_ref().unwrap();
        assert_eq!(
            setup.command,
            "npm ci --prefer-offline --no-audit --no-fund"
        );
        assert_eq!(setup.source, SetupSource::Detected);
        assert_eq!(setup.script, setup.command);
        assert_eq!(setup.key, None, "node keeps no stamp");
        assert_eq!(setup.registry_hosts(), ["registry.npmjs.org"]);
        assert_eq!(plan.build_command().unwrap(), "axocoatl recipe build node");
        let summary = plan.summary();
        assert!(
            summary.contains("toolchain node (package.json at the repository root)"),
            "{summary}"
        );
        assert!(summary.contains("check npm run test"), "{summary}");
        assert!(
            summary.contains("registries registry.npmjs.org open for it alone"),
            "{summary}"
        );
    }

    /// The new-user gate's fixtures (tests/fixtures/newuser): `run fix`
    /// plans each with the toolchain and image its fixtures.json entry
    /// names, and a fixture whose entry names a setup gets that setup with
    /// its registries open, so the gate's check of the live run's output can
    /// only fail on what the live run did. The plan lines the gate's
    /// self-test reads (scripts/release-gate/testdata/run-plan-lines.txt)
    /// are this summary, but for the recipe image's tag.
    #[test]
    fn the_new_user_fixtures_plan_what_the_gate_expects() {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let root = repository.join("tests/fixtures/newuser");
        let expected: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(root.join("fixtures.json")).unwrap())
                .unwrap();
        let fixtures = expected["fixtures"].as_object().unwrap();
        assert!(fixtures.len() >= 12, "{fixtures:?}");
        let lines = std::fs::read_to_string(
            repository.join("scripts/release-gate/testdata/run-plan-lines.txt"),
        )
        .unwrap();
        let recorded: std::collections::HashMap<&str, &str> = lines
            .lines()
            .filter(|line| !line.starts_with('#') && !line.is_empty())
            .map(|line| line.split_once('\t').unwrap())
            .collect();
        // The recipe image's tag is a hash of the recipes, which other
        // changes move.
        let untagged = |line: &str| {
            line.split(' ')
                .map(|word| match word.split_once(':') {
                    Some((image, _)) if image.starts_with("localhost/axocoatl-recipe-") => {
                        format!("{image}:TAG")
                    }
                    _ => word.to_string(),
                })
                .collect::<Vec<_>>()
                .join(" ")
        };
        let mut stale = Vec::new();
        for (name, fixture) in fixtures {
            let plan = run_plan(
                &root.join(name),
                "fix",
                EnvironmentRequest::default(),
                None,
                None,
            )
            .unwrap_or_else(|error| panic!("{name}: {error}"));
            let summary = plan.summary();
            let language = fixture["language"].as_str().unwrap();
            assert_eq!(
                plan.toolchain.map(Toolchain::name),
                Some(language),
                "{name}: {summary}"
            );
            let image = fixture["image_contains"].as_str().unwrap();
            assert!(plan.image_text().contains(image), "{name}: {summary}");
            if let Some(setup) = fixture["setup_contains"].as_str() {
                assert!(summary.contains(setup), "{name}: {setup} in {summary}");
                let planned = plan.setup.as_ref().unwrap();
                assert!(!planned.registries.is_empty(), "{name}: {summary}");
            }
            if recorded.get(name.as_str()).map(|line| untagged(line)) != Some(untagged(&summary)) {
                stale.push(format!("{name}\t{summary}"));
            }
        }
        assert!(
            stale.is_empty(),
            "run-plan-lines.txt should hold these lines:\n{}",
            stale.join("\n")
        );
    }

    #[test]
    fn external_writers_compose_their_recipe_with_the_toolchain() {
        let dir = repo(&[(
            "pyproject.toml",
            "[project]\nname = 'pages'\n[tool.pytest.ini_options]\n",
        )]);
        for (id, agent) in [("fix-claude-code", "claude-code"), ("fix-codex", "codex")] {
            let plan = run_plan(dir.path(), id, EnvironmentRequest::default(), None, None).unwrap();
            assert_eq!(plan.toolchain, Some(Toolchain::Python), "{id}");
            assert_eq!(plan.recipes, [agent, "python"], "{id}");
            assert_eq!(
                plan.build_command().unwrap(),
                format!("axocoatl recipe build {agent} python")
            );
            let setup = plan.setup.unwrap();
            assert_eq!(setup.command, "pip install -e . pytest");
            assert_eq!(
                setup.key.as_deref().map(str::len),
                Some(32),
                "python keeps a stamp"
            );
            assert!(
                !setup.script.contains("stamp"),
                "root keeps it, not the script"
            );
            assert_eq!(
                setup.registry_hosts(),
                ["pypi.org", "files.pythonhosted.org"]
            );
        }
    }

    #[test]
    fn flags_override_the_toolchain_the_image_and_the_setup() {
        let dir = repo(&[("package.json", MOCHA), ("package-lock.json", "{}")]);
        // --toolchain none: the run as before toolchains.
        let none = run_plan(
            dir.path(),
            "fix",
            EnvironmentRequest {
                toolchain: Some("none".into()),
                ..Default::default()
            },
            None,
            Some("npm ci"),
        )
        .unwrap();
        assert_eq!(none.toolchain, None);
        assert!(none.recipes.is_empty());
        assert_eq!(none.setup, None);
        assert_eq!(none.plain_setup.as_deref(), Some("npm ci"));
        assert_eq!(none.image_text(), "docker.io/library/alpine:3.20");
        // --image: the image as it is, no toolchain.
        let image = run_plan(
            dir.path(),
            "fix",
            EnvironmentRequest {
                image: Some("docker.io/library/node:20-slim".into()),
                ..Default::default()
            },
            None,
            None,
        )
        .unwrap();
        assert_eq!(image.toolchain, None);
        assert_eq!(
            image.image.as_deref(),
            Some("docker.io/library/node:20-slim")
        );
        let error = run_plan(
            dir.path(),
            "fix",
            EnvironmentRequest {
                image: Some("docker.io/library/node:20-slim".into()),
                toolchain: Some("node".into()),
                ..Default::default()
            },
            None,
            None,
        )
        .unwrap_err();
        assert!(
            error.contains("give --toolchain or --image, not both"),
            "{error}"
        );
        // --setup replaces the detected install; registries still open.
        let setup = run_plan(
            dir.path(),
            "fix",
            EnvironmentRequest::default(),
            None,
            Some("npm ci --ignore-scripts"),
        )
        .unwrap()
        .setup
        .unwrap();
        assert_eq!(setup.command, "npm ci --ignore-scripts");
        assert_eq!(setup.source, SetupSource::Flag);
        assert_eq!(setup.registry_hosts(), ["registry.npmjs.org"]);
        // --no-setup: none at all.
        let no_setup = run_plan(
            dir.path(),
            "fix",
            EnvironmentRequest {
                no_setup: true,
                ..Default::default()
            },
            None,
            None,
        )
        .unwrap();
        assert_eq!(no_setup.setup, None);
        assert_eq!(no_setup.toolchain, Some(Toolchain::Node));
        // --toolchain names one explicitly.
        let go = run_plan(
            dir.path(),
            "fix",
            EnvironmentRequest {
                toolchain: Some("go".into()),
                ..Default::default()
            },
            Some("go test ./..."),
            None,
        )
        .unwrap();
        assert_eq!(go.toolchain, Some(Toolchain::Go));
        assert_eq!(go.reason, "--toolchain go");
        // TC-NU-3: Rust is planned like the others.
        let rust = run_plan(
            dir.path(),
            "fix",
            EnvironmentRequest {
                toolchain: Some("rust".into()),
                ..Default::default()
            },
            Some("cargo test"),
            None,
        )
        .unwrap();
        assert_eq!(rust.toolchain, Some(Toolchain::Rust));
        assert_eq!(rust.reason, "--toolchain rust");
        assert_eq!(rust.recipes, ["rust"]);
        let error = run_plan(
            dir.path(),
            "fix",
            EnvironmentRequest {
                toolchain: Some("java".into()),
                ..Default::default()
            },
            None,
            None,
        )
        .unwrap_err();
        assert!(error.contains("--toolchain \"java\""), "{error}");
    }

    #[test]
    fn several_toolchains_follow_the_check_or_refuse() {
        let dir = repo(&[
            ("package.json", r#"{"devDependencies":{"x":"1"}}"#),
            ("go.mod", "module x\n"),
            ("Makefile", "test:\n"),
        ]);
        // No package.json script: the detected check is `go test ./...`.
        let plan = run_plan(dir.path(), "fix", EnvironmentRequest::default(), None, None).unwrap();
        assert_eq!(plan.toolchain, Some(Toolchain::Go));
        assert!(plan.reason.contains("the check runs go"), "{}", plan.reason);
        let error = run_plan(
            dir.path(),
            "fix",
            EnvironmentRequest::default(),
            Some("make test"),
            None,
        )
        .unwrap_err();
        assert!(
            error.contains("Pass --toolchain LANGUAGE, where LANGUAGE is node or go"),
            "{error}"
        );
    }

    #[test]
    fn loadouts_without_a_repository_check_keep_their_image() {
        let dir = repo(&[("package.json", MOCHA)]);
        let qa = run_plan(dir.path(), "qa", EnvironmentRequest::default(), None, None).unwrap();
        assert_eq!(qa.toolchain, None);
        assert_eq!(qa.reason, "the loadout runs no repository check");
        assert_eq!(qa.check, None);
        // Nothing to detect: no toolchain, today's image.
        let empty = repo(&[("README.md", "")]);
        let plan = run_plan(
            empty.path(),
            "fix",
            EnvironmentRequest::default(),
            Some("sh test.sh"),
            None,
        )
        .unwrap();
        assert_eq!(plan.toolchain, None);
        assert_eq!(
            plan.check,
            Some(("sh test.sh".into(), detect::CheckSource::Flag))
        );
    }

    #[test]
    fn axocoatl_md_gives_the_check_and_chooses_the_toolchain() {
        let dir = repo(&[
            (
                "AXOCOATL.md",
                "# Notes\n\nCheck command: `go test ./pkg/...`\n",
            ),
            ("go.mod", "module x\n"),
            ("package.json", MOCHA),
        ]);
        let plan = run_plan(dir.path(), "fix", EnvironmentRequest::default(), None, None).unwrap();
        assert_eq!(plan.toolchain, Some(Toolchain::Go));
        assert_eq!(
            plan.check,
            Some(("go test ./pkg/...".into(), detect::CheckSource::AxocoatlMd))
        );
        assert_eq!(plan.setup, None, "no dependencies to fetch");
        // A Rust repository is planned like the others: its image, its
        // check and `cargo fetch` with crates.io open.
        let dir = repo(&[
            ("AXOCOATL.md", "Check command: `cargo test --lib`\n"),
            (
                "Cargo.toml",
                "[package]\nname = 'x'\n[dev-dependencies]\nitoa = '1'\n",
            ),
            ("package.json", MOCHA),
        ]);
        let plan = run_plan(dir.path(), "fix", EnvironmentRequest::default(), None, None).unwrap();
        assert_eq!(plan.toolchain, Some(Toolchain::Rust));
        assert_eq!(plan.recipes, ["rust"]);
        assert_eq!(plan.image, None);
        assert!(plan
            .image_text()
            .starts_with("localhost/axocoatl-recipe-rust:"));
        assert_eq!(
            plan.build_command().as_deref(),
            Some("axocoatl recipe build rust")
        );
        assert_eq!(
            plan.check,
            Some(("cargo test --lib".into(), detect::CheckSource::AxocoatlMd))
        );
        let setup = plan.setup.expect("cargo fetch");
        assert_eq!(setup.command, "cargo fetch");
        assert_eq!(setup.source, SetupSource::Detected);
        assert_eq!(
            setup.registry_hosts(),
            ["index.crates.io", "static.crates.io", "crates.io"]
        );
        // Without AXOCOATL.md the detected check is `cargo test`.
        let dir = repo(&[(
            "Cargo.toml",
            "[package]\nname = 'x'\n[dependencies]\nitoa = '1'\n",
        )]);
        let plan = run_plan(dir.path(), "fix", EnvironmentRequest::default(), None, None).unwrap();
        assert_eq!(plan.toolchain, Some(Toolchain::Rust));
        assert_eq!(
            plan.check,
            Some(("cargo test".into(), detect::CheckSource::Detected))
        );
        assert_eq!(
            plan.setup.map(|setup| setup.command).as_deref(),
            Some("cargo fetch")
        );
    }

    /// TC-DOC-1: under `network: none` the summary says no registry opens.
    #[test]
    fn the_summary_says_when_no_registry_opens() {
        let dir = repo(&[("package.json", MOCHA), ("package-lock.json", "{}")]);
        let mut plan =
            run_plan(dir.path(), "fix", EnvironmentRequest::default(), None, None).unwrap();
        assert_eq!(plan.network, "egress");
        plan.network = "none".into();
        let summary = plan.summary();
        assert!(
            summary.contains("no registry: the loadout's network is none"),
            "{summary}"
        );
        assert!(!summary.contains("open for it alone"), "{summary}");
    }

    fn hints(program: &str) -> ProbeHints {
        ProbeHints {
            program: Some(program.into()),
            ..ProbeHints::default()
        }
    }

    fn classify(
        exit: Option<i32>,
        stdout: &str,
        stderr: &str,
        hints: &ProbeHints,
        task: &str,
    ) -> Option<ProbeMissing> {
        classify_probe(exit, stdout, stderr, ProbeContext { hints, task })
    }

    #[test]
    fn probes_tell_a_missing_program_from_a_failing_check() {
        let npm = ProbeHints {
            program: Some("npm".into()),
            script_program: Some("mocha".into()),
            ..ProbeHints::default()
        };
        assert_eq!(
            classify(
                Some(127),
                "",
                "sh: 1: mocha: not found\n",
                &npm,
                "fix paging"
            ),
            Some(ProbeMissing::Program("mocha".into()))
        );
        assert_eq!(
            classify(
                Some(1),
                "",
                "> test\n> mocha\n\nsh: mocha: not found\nnpm error code 127",
                &npm,
                ""
            ),
            Some(ProbeMissing::Program("mocha".into()))
        );
        assert_eq!(
            classify(
                Some(127),
                "",
                "bash: line 1: cargo: command not found",
                &ProbeHints::default(),
                ""
            ),
            Some(ProbeMissing::Program("cargo".into()))
        );
        let python = ProbeHints {
            program: Some("python3".into()),
            runner_module: Some("pytest".into()),
            ..ProbeHints::default()
        };
        assert_eq!(
            classify(
                Some(1),
                "",
                "/usr/local/bin/python3: No module named pytest",
                &python,
                ""
            ),
            Some(ProbeMissing::Module("pytest".into()))
        );
        // A package the repository declares, not installed.
        let declared = ProbeHints {
            program: Some("pytest".into()),
            declared_packages: vec!["requests".into(), "pyyaml".into()],
            ..ProbeHints::default()
        };
        assert_eq!(
            classify(
                Some(2),
                "E   ModuleNotFoundError: No module named 'requests'",
                "",
                &declared,
                "fix the page count"
            ),
            Some(ProbeMissing::Module("requests".into()))
        );
        // ... unless the task asks to create that module.
        assert_eq!(
            classify(
                Some(2),
                "E   ModuleNotFoundError: No module named 'requests.adapters'",
                "",
                &declared,
                "Add a requests module"
            ),
            None
        );
        // A check that runs and fails is not a refusal, even when its own
        // output says "not found".
        assert_eq!(
            classify(
                Some(1),
                "AssertionError: expected 3, got 2\nError: page not found",
                "",
                &hints("pytest"),
                ""
            ),
            None
        );
        assert_eq!(classify(Some(0), "", "", &hints("npm"), ""), None);
        assert_eq!(
            classify(Some(127), "", "", &hints("jest"), ""),
            Some(ProbeMissing::Program("jest".into()))
        );
    }

    /// TC-COR-2: what a fix run is for is never refused: an import typo, a
    /// module the task creates, a script the task adds, a program a test
    /// runs.
    #[test]
    fn failures_the_writer_can_fix_are_not_refused() {
        let pytest = ProbeHints {
            program: Some("pytest".into()),
            declared_packages: vec!["requests".into()],
            test_files: vec!["tests/test_calc.py".into()],
            ..ProbeHints::default()
        };
        for (exit, stdout, stderr, task) in [
            (
                Some(2),
                "E   ModuleNotFoundError: No module named 'colections'",
                "",
                "The tests fail to import; fix it",
            ),
            (
                Some(2),
                "E   ModuleNotFoundError: No module named 'calc.utils'",
                "",
                "Create the utils module with an add() function",
            ),
            (
                Some(1),
                "",
                "/bin/sh: 1: ./bin/greet: not found",
                "Add the bin/greet script that prints hello",
            ),
            (
                Some(1),
                "",
                "sh: 1: convert: not found\nFAILED tests/test_img.py::test_resize",
                "fix resizing",
            ),
            (
                Some(127),
                "",
                "/bin/sh: 1: scripts/run-tests.sh: not found",
                "add the test script",
            ),
        ] {
            assert_eq!(
                classify(exit, stdout, stderr, &pytest, task),
                None,
                "{stdout}{stderr}"
            );
        }
    }

    /// TC-NU-1: a check that finds none of the repository's tests can never
    /// pass; one in a repository without tests may (the writer adds them).
    #[test]
    fn a_check_that_runs_no_test_of_the_repository_is_refused() {
        let with_tests = ProbeHints {
            program: Some("python3".into()),
            runner_module: Some("unittest".into()),
            test_files: vec!["tests/test_pages.py".into()],
            ..ProbeHints::default()
        };
        let unittest = "\n----------------------------------------------------------------------\nRan 0 tests in 0.000s\n\nNO TESTS RAN\n";
        assert_eq!(
            classify(Some(5), "", unittest, &with_tests, "fix paging"),
            Some(ProbeMissing::NoTests {
                files: vec!["tests/test_pages.py".into()]
            })
        );
        assert_eq!(
            classify(
                Some(5),
                "collected 0 items\n\n===== no tests ran in 0.01s =====",
                "",
                &with_tests,
                ""
            ),
            Some(ProbeMissing::NoTests {
                files: vec!["tests/test_pages.py".into()]
            })
        );
        assert!(matches!(
            classify(
                Some(1),
                "",
                "Error: No test files found: \"test\"",
                &with_tests,
                ""
            ),
            Some(ProbeMissing::NoTests { .. })
        ));
        let without = ProbeHints {
            test_files: Vec::new(),
            ..with_tests.clone()
        };
        assert_eq!(classify(Some(5), "", unittest, &without, "add tests"), None);
        // Tests that ran and failed are the run's to fix.
        assert_eq!(
            classify(
                Some(1),
                "",
                "Ran 2 tests in 0.001s\n\nFAILED (failures=1)",
                &with_tests,
                ""
            ),
            None
        );
    }

    #[test]
    fn a_sandbox_refusal_is_the_environment() {
        let missing = classify(
            Some(101),
            "",
            "error: failed to write `/tmp/t/debug/deps/libx.rmeta`\n\nCaused by:\n  Invalid cross-device link (os error 18)",
            &hints("cargo"),
            "",
        )
        .unwrap();
        assert!(matches!(missing, ProbeMissing::Sandbox(_)));
        assert!(missing.is_environment());
        assert!(!ProbeMissing::Program("x".into()).is_environment());
    }

    #[test]
    fn a_probe_refusal_names_the_image_the_check_the_program_and_the_fixes() {
        let dir = repo(&[("package.json", r#"{"scripts":{"test":"jest"}}"#)]);
        let plan = run_plan(dir.path(), "fix", EnvironmentRequest::default(), None, None).unwrap();
        assert_eq!(plan.setup, None, "no dependencies declared");
        let message = probe_refusal(
            &plan,
            "npm run test",
            &ProbeMissing::Program("jest".into()),
            None,
        );
        for expected in [
            "the check `npm run test` cannot run in this run's image",
            "the program `jest` was not found",
            "localhost/axocoatl-recipe-node:",
            "the node toolchain (package.json at the repository root)",
            "No writer or reviewer turn ran",
            "package.json's devDependencies",
            "--setup",
            "--image",
            "loadout",
            "--no-probe",
            // Each value to fill in is a placeholder in capitals, which
            // pastes as it is, never one in angle brackets.
            "give its install with --setup \"COMMAND\", where COMMAND installs jest",
            "pass --toolchain LANGUAGE when the check runs another language, where LANGUAGE is \
             node, python, rust or go",
            "pass --image IMAGE of an image that has it",
        ] {
            assert!(message.contains(expected), "{expected}: {message}");
        }
        assert!(!message.contains('<'), "{message}");
    }

    /// TC-NU-5: the advice fits the language and the program, and the setup
    /// note is words, not Rust's debug output.
    #[test]
    fn probe_refusals_give_advice_that_can_work() {
        let dir = repo(&[(
            "pyproject.toml",
            "[project]\nname = 'pages'\n[tool.pytest.ini_options]\n",
        )]);
        let plan = run_plan(dir.path(), "fix", EnvironmentRequest::default(), None, None).unwrap();
        let make = probe_refusal(
            &plan,
            "make test",
            &ProbeMissing::Program("make".into()),
            Some("`pip install -e . pytest`: exited 0"),
        );
        assert!(
            make.contains("system program no setup can install"),
            "{make}"
        );
        assert!(!make.contains("where COMMAND installs make"), "{make}");
        assert!(!make.contains("devDependencies"), "{make}");
        assert!(
            make.contains("Setup: `pip install -e . pytest`: exited 0."),
            "{make}"
        );
        let module = probe_refusal(&plan, "pytest", &ProbeMissing::Module("yaml".into()), None);
        assert!(module.contains("pyproject.toml"), "{module}");
        assert!(
            module.contains("--setup \"pip install PACKAGE\", where PACKAGE provides yaml"),
            "{module}"
        );
        let tests = probe_refusal(
            &plan,
            "python3 -m unittest discover",
            &ProbeMissing::NoTests {
                files: vec!["tests/test_a.py".into()],
            },
            None,
        );
        assert!(
            tests.contains("can never pass in this run: it ran no test"),
            "{tests}"
        );
        assert!(tests.contains("tests/test_a.py"), "{tests}");
        assert!(tests.contains("Check command:"), "{tests}");
    }

    #[test]
    fn a_missing_module_without_a_toolchain_points_to_the_python_toolchain() {
        let dir = repo(&[("pyproject.toml", "[project]\nname = 'x'\n")]);
        let plan = run_plan(
            dir.path(),
            "fix",
            EnvironmentRequest {
                toolchain: Some("none".into()),
                ..Default::default()
            },
            Some("python3 -m pytest"),
            None,
        )
        .unwrap();
        let message = probe_refusal(
            &plan,
            "python3 -m pytest",
            &ProbeMissing::Module("pytest".into()),
            None,
        );
        assert!(
            message.contains("To fix it: pass --toolchain python"),
            "{message}"
        );
        assert!(!message.contains("pip install PACKAGE"), "{message}");
        assert!(!message.contains("node, python, rust or go"), "{message}");
    }

    /// The probe's hints come from the repository's files.
    #[test]
    fn the_plan_carries_the_probes_hints() {
        let dir = repo(&[
            ("package.json", MOCHA),
            ("package-lock.json", "{}"),
            ("test/paging.test.js", ""),
        ]);
        let plan = run_plan(dir.path(), "fix", EnvironmentRequest::default(), None, None).unwrap();
        assert_eq!(plan.probe.program.as_deref(), Some("npm"));
        assert_eq!(plan.probe.script_program.as_deref(), Some("mocha"));
        assert_eq!(plan.probe.test_files, ["test/paging.test.js"]);
        let dir = repo(&[
            ("requirements.txt", "PyYAML\n"),
            ("tests/test_x.py", "def test_x():\n    pass\n"),
        ]);
        let plan = run_plan(
            dir.path(),
            "fix",
            EnvironmentRequest::default(),
            Some("python3 -m pytest -q"),
            None,
        )
        .unwrap();
        assert_eq!(plan.probe.runner_module.as_deref(), Some("pytest"));
        assert_eq!(plan.probe.declared_packages, ["pyyaml"]);
        assert_eq!(plan.probe.test_files, ["tests/test_x.py"]);
        // Detected: plain test functions get pytest, and the setup brings it.
        let plan = run_plan(dir.path(), "fix", EnvironmentRequest::default(), None, None).unwrap();
        assert_eq!(
            plan.check,
            Some(("python3 -m pytest".into(), detect::CheckSource::Detected))
        );
        assert_eq!(plan.probe.runner_module.as_deref(), Some("pytest"));
        assert_eq!(
            plan.setup.unwrap().command,
            "pip install -r requirements.txt pytest"
        );
    }

    #[test]
    fn environment_requests_serialize_only_what_they_say() {
        assert!(EnvironmentRequest::default().is_default());
        assert_eq!(
            serde_json::to_value(EnvironmentRequest::default()).unwrap(),
            serde_json::json!({})
        );
        let request: EnvironmentRequest = serde_json::from_value(
            serde_json::json!({"toolchain": "go", "build_images": false, "no_probe": true}),
        )
        .unwrap();
        assert_eq!(request.toolchain.as_deref(), Some("go"));
        assert!(!request.build_images);
        assert!(request.no_probe);
        assert!(serde_json::from_value::<EnvironmentRequest>(serde_json::json!({"x": 1})).is_err());
    }
}
