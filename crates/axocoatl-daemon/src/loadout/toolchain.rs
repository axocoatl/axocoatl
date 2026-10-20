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
            script: detect::setup_script(toolchain, &command, None),
            command,
            source,
            registries: Vec::new(),
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
                script: detect::setup_script(toolchain, &install.command, key.as_deref()),
                command: install.command,
                source: SetupSource::Detected,
                registries: Vec::new(),
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
    })
}

/// Why a check cannot run in the run's image, found by the probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeMissing {
    /// A program the check runs is not on the writer's PATH.
    Program(String),
    /// A Python module the check imports is not installed.
    Module(String),
}

impl ProbeMissing {
    pub fn describe(&self) -> String {
        match self {
            ProbeMissing::Program(program) => format!("the program `{program}` was not found"),
            ProbeMissing::Module(module) => {
                format!("the Python module `{module}` is not installed")
            }
        }
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

/// What the probe's check run on the untouched tree says: `Some` when the
/// check cannot run in this image. A check that runs and fails is what a fix
/// run is for, and is not a refusal.
pub fn classify_probe(
    exit_code: Option<i32>,
    stdout: &str,
    stderr: &str,
    check_program: Option<&str>,
    task: &str,
) -> Option<ProbeMissing> {
    for line in stderr.lines().chain(stdout.lines()) {
        if let Some(program) = shell_not_found(line) {
            return Some(ProbeMissing::Program(program));
        }
    }
    for line in stderr.lines().chain(stdout.lines()) {
        if let Some(module) = missing_module(line) {
            if !task_names(task, &module) {
                return Some(ProbeMissing::Module(module));
            }
        }
    }
    if exit_code == Some(127) {
        return Some(ProbeMissing::Program(
            check_program.unwrap_or("the check's program").to_string(),
        ));
    }
    None
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
    let mut fixes = Vec::new();
    match (plan.toolchain, missing) {
        (_, ProbeMissing::Module(module)) => fixes.push(format!(
            "--setup \"pip install <the package that provides {module}>\" (or declare it in \
             the project's test dependencies)"
        )),
        (Some(_), ProbeMissing::Program(program)) => fixes.push(format!(
            "--setup \"<command that installs {program}>\" (or declare it in the project's \
             dependencies, such as package.json devDependencies)"
        )),
        (None, ProbeMissing::Program(_)) => {}
    }
    if plan.toolchain.is_none() {
        fixes.push(
            "--toolchain node|python|rust|go for Axocoatl's image of the repository's language"
                .into(),
        );
    } else {
        fixes.push("--toolchain <another language> when the check runs another one".into());
    }
    fixes.push("--image <ref> of an image that has it".into());
    fixes.push("--check \"<command>\" for a check this image can run".into());
    fixes.push("or a loadout whose environment provides it".into());
    format!(
        "the check `{check}` cannot run in this run's image: {}. Image: {}, with {toolchain}.{} \
         No writer or reviewer turn ran. Fix it with {}",
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
        assert_eq!(setup.script, setup.command, "node keeps no stamp");
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
            assert!(setup.script.contains("key="), "python keeps a stamp");
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
        assert!(error.contains("--toolchain node|go"), "{error}");
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
                "# Notes\n\nCheck command: `cargo test --lib`\n",
            ),
            ("Cargo.toml", "[package]\nname = 'x'\n"),
            ("package.json", MOCHA),
        ]);
        let plan = run_plan(dir.path(), "fix", EnvironmentRequest::default(), None, None).unwrap();
        assert_eq!(plan.toolchain, Some(Toolchain::Rust));
        assert_eq!(
            plan.check,
            Some(("cargo test --lib".into(), detect::CheckSource::AxocoatlMd))
        );
        assert_eq!(plan.setup, None, "no dependencies to fetch");
    }

    #[test]
    fn probes_tell_a_missing_program_from_a_failing_check() {
        assert_eq!(
            classify_probe(
                Some(127),
                "",
                "sh: 1: mocha: not found\n",
                Some("npm"),
                "fix paging"
            ),
            Some(ProbeMissing::Program("mocha".into()))
        );
        assert_eq!(
            classify_probe(
                Some(1),
                "",
                "> test\n> mocha\n\nsh: mocha: not found\nnpm error code 127",
                Some("npm"),
                ""
            ),
            Some(ProbeMissing::Program("mocha".into()))
        );
        assert_eq!(
            classify_probe(
                Some(127),
                "",
                "bash: line 1: cargo: command not found",
                None,
                ""
            ),
            Some(ProbeMissing::Program("cargo".into()))
        );
        assert_eq!(
            classify_probe(
                Some(1),
                "",
                "/usr/local/bin/python3: No module named pytest",
                Some("python3"),
                ""
            ),
            Some(ProbeMissing::Module("pytest".into()))
        );
        assert_eq!(
            classify_probe(
                Some(2),
                "E   ModuleNotFoundError: No module named 'requests'",
                "",
                Some("pytest"),
                "fix the page count"
            ),
            Some(ProbeMissing::Module("requests".into()))
        );
        // A module the task asks for is missing by design.
        assert_eq!(
            classify_probe(
                Some(2),
                "E   ModuleNotFoundError: No module named 'slugify'",
                "",
                Some("pytest"),
                "Add a slugify module with slugify(text)"
            ),
            None
        );
        // A check that runs and fails is not a refusal, even when its own
        // output says "not found".
        assert_eq!(
            classify_probe(
                Some(1),
                "AssertionError: expected 3, got 2\nError: page not found",
                "",
                Some("pytest"),
                ""
            ),
            None
        );
        assert_eq!(classify_probe(Some(0), "", "", Some("npm"), ""), None);
        assert_eq!(
            classify_probe(Some(127), "", "", Some("jest"), ""),
            Some(ProbeMissing::Program("jest".into()))
        );
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
            "--setup",
            "--image",
            "loadout",
        ] {
            assert!(message.contains(expected), "{expected}: {message}");
        }
    }

    #[test]
    fn environment_requests_serialize_only_what_they_say() {
        assert!(EnvironmentRequest::default().is_default());
        assert_eq!(
            serde_json::to_value(EnvironmentRequest::default()).unwrap(),
            serde_json::json!({})
        );
        let request: EnvironmentRequest =
            serde_json::from_value(serde_json::json!({"toolchain": "go", "build_images": false}))
                .unwrap();
        assert_eq!(request.toolchain.as_deref(), Some("go"));
        assert!(!request.build_images);
        assert!(serde_json::from_value::<EnvironmentRequest>(serde_json::json!({"x": 1})).is_err());
    }
}
