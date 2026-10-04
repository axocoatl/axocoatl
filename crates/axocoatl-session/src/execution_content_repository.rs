//! Actual repository observations, linked to their canonical tool receipt.
use super::*;
use crate::turn_contract::InvocationEvidence;

/// The capture program. The host prefixes its mode: see
/// [`REPOSITORY_SNAPSHOT_COMMAND`] and the write-scope captures in the daemon.
pub const REPOSITORY_CAPTURE_SCRIPT: &str =
    include_str!("execution_content_repository_snapshot.sh");

/// Run at the exact working root to observe HEAD, the file manifest and the
/// patch against HEAD. It writes only under its own temporary directory and
/// takes no git locks. Git reads no configuration from outside the
/// repository, and nothing the repository configures can run a program.
pub const REPOSITORY_SNAPSHOT_COMMAND: &str = concat!(
    "capture_mode=observe\n",
    include_str!("execution_content_repository_snapshot.sh")
);

/// The capture command of earlier releases, which let Git read the home
/// directory's configuration. Turns admitted with it keep it: their recorded
/// check definitions name it, so they still load, render and finish.
pub const REPOSITORY_SNAPSHOT_COMMAND_V1: &str =
    include_str!("execution_content_repository_snapshot_v1.sh");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepositorySnapshotPhase {
    Before,
    After,
    BeforeCheck { index: u32 },
    AfterCheck { index: u32 },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActivationRepositorySnapshot {
    pub activation: ActivationRef,
    pub phase: RepositorySnapshotPhase,
    pub repository: EvidenceRef,
    pub invocation: Option<InvocationId>,
    #[serde(default)]
    pub condition_run: Option<ConditionRunId>,
    pub outcome: Option<EvidenceRef>,
    pub head: Option<String>,
    pub tree_sha256: Option<String>,
    pub manifest_bytes: u64,
    pub manifest_prefix: String,
    pub manifest_complete: bool,
    pub patch_sha256: Option<String>,
    pub patch_bytes: u64,
    pub patch_prefix: String,
    pub patch_complete: bool,
    pub patch_base64: Option<String>,
    pub unavailable: Option<String>,
    /// SHA-256 of the complete manifest a write-scope judgement compares:
    /// every tracked and untracked path with its index entry, each ignore
    /// file Git reads, and Git's own settings, hooks and exclude files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judged_sha256: Option<String>,
    /// A Before capture's directory in the sandbox where that complete
    /// manifest is kept for the After capture to compare with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline: Option<String>,
    /// An After capture's comparison: the digest of the kept Before manifest
    /// it verified, and every path whose entry differs from it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compared: Option<RepositoryComparison>,
}

/// Paths whose entries differ between a verified Before manifest and the
/// After capture's own. Complete: a capture that could not list every path
/// records no comparison at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryComparison {
    pub before_sha256: String,
    pub changed_paths: Vec<String>,
}

/// Most bytes of changed paths one comparison may record.
pub const MAX_COMPARED_PATH_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ActivationRepositorySnapshotView {
    pub reference: EvidenceRef,
    pub content: ActivationRepositorySnapshot,
}

/// A check result recorded by the removed standing-work inbox. Stored records
/// still load; nothing records a new one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StandingRepositoryCheck {
    pub activation: ActivationRef,
    pub index: u32,
    pub argv: Vec<String>,
    pub before: EvidenceRef,
    pub after: EvidenceRef,
    pub invocation: InvocationId,
    pub outcome: EvidenceRef,
    pub exit_code: Option<i32>,
    pub candidate_sha256: Option<String>,
    pub passed: bool,
}

impl ExecutionContentStore {
    /// A successful command applies only to the stable candidate actually
    /// captured around that command's own epoch and accepted input frontier,
    /// by its group's capture conditions `before_id` and `after_id`.
    pub fn check_candidate(
        &self,
        snapshot: &DurableTurnSnapshot,
        run: &crate::turn_contract::ConditionRunRef,
        before_id: &str,
        after_id: &str,
    ) -> Result<Option<(String, Option<String>)>, ExecutionContentError> {
        self.require_snapshot(snapshot)?;
        let runs = snapshot.contract().condition_runs();
        let Some(command_position) = runs.iter().position(|item| item.run == *run) else {
            return Ok(None);
        };
        let before = runs.iter().enumerate().find(|(_, item)| {
            item.run.epoch_id == run.epoch_id
                && item.run.activations == run.activations
                && item.run.condition_id.as_str() == before_id
        });
        let after = runs.iter().enumerate().find(|(_, item)| {
            item.run.epoch_id == run.epoch_id
                && item.run.activations == run.activations
                && item.run.condition_id.as_str() == after_id
        });
        let (Some((before_position, before)), Some((after_position, after))) = (before, after)
        else {
            return Ok(None);
        };
        if before_position >= command_position || after_position <= command_position {
            return Ok(None);
        }
        let Some(activation) = run.activations.first() else {
            return Ok(None);
        };
        let captures = self.repository_snapshots(snapshot, activation)?;
        let before = captures.iter().find(|capture| {
            capture.content.condition_run.as_ref() == Some(&before.run.run_id)
                && capture.content.phase == (RepositorySnapshotPhase::BeforeCheck { index: 0 })
        });
        let after = captures.iter().find(|capture| {
            capture.content.condition_run.as_ref() == Some(&after.run.run_id)
                && capture.content.phase == (RepositorySnapshotPhase::AfterCheck { index: 0 })
        });
        let (Some(before), Some(after)) = (before, after) else {
            return Ok(None);
        };
        if before.content.tree_sha256.is_none()
            || before.content.tree_sha256 != after.content.tree_sha256
            || before.content.head != after.content.head
            || before.content.repository != after.content.repository
        {
            return Ok(None);
        }
        Ok(after
            .content
            .tree_sha256
            .clone()
            .map(|tree| (tree, after.content.head.clone())))
    }

    /// Last protected post-execution capture in this closed turn. A missing or
    /// failed later capture is not replaced by an older convenient identity.
    pub fn completed_repository_candidate(
        &self,
        snapshot: &DurableTurnSnapshot,
    ) -> Result<Option<ActivationRepositorySnapshotView>, ExecutionContentError> {
        self.require_snapshot(snapshot)?;
        if snapshot.contract().state() != Some(LogicalTurnState::Completed) {
            return Ok(None);
        }
        for record in self
            .keyed(&segments::turn_repository_snapshot_key(snapshot.turn_id()))?
            .iter()
            .rev()
        {
            let Body::RepositorySnapshot(observation) = &record.body else {
                continue;
            };
            if observation.activation.turn_id != *snapshot.turn_id()
                || !matches!(
                    observation.phase,
                    RepositorySnapshotPhase::After | RepositorySnapshotPhase::AfterCheck { .. }
                )
                || !snapshot
                    .contract()
                    .current_accepted_activations()
                    .iter()
                    .any(|accepted| accepted.activation == observation.activation)
            {
                continue;
            }
            validate_observation(snapshot, observation)?;
            return Ok(observation.tree_sha256.is_some().then(|| {
                ActivationRepositorySnapshotView {
                    reference: record.reference.clone(),
                    content: observation.clone(),
                }
            }));
        }
        Ok(None)
    }
    pub fn retain_repository_snapshot(
        &mut self,
        snapshot: &DurableTurnSnapshot,
        observation: ActivationRepositorySnapshot,
    ) -> Result<DurableActivationEvidence, ExecutionContentError> {
        self.require_activation(snapshot, &observation.activation)?;
        validate_observation(snapshot, &observation)?;
        let previous = self.repository_snapshots(snapshot, &observation.activation)?;
        if let Some(previous) = previous.iter().find(|previous| {
            previous.content.phase == observation.phase
                && previous.content.condition_run == observation.condition_run
        }) {
            if previous.content != observation {
                return Err(ExecutionContentError::Conflict);
            }
        }
        let reference = self.append(Body::RepositorySnapshot(observation))?;
        Ok(DurableActivationEvidence {
            identity: self.identity.clone(),
            reference,
        })
    }

    pub fn repository_snapshots(
        &self,
        snapshot: &DurableTurnSnapshot,
        activation: &ActivationRef,
    ) -> Result<Vec<ActivationRepositorySnapshotView>, ExecutionContentError> {
        self.require_activation(snapshot, activation)?;
        let mut result = Vec::new();
        for record in self
            .keyed(&segments::repository_snapshot_key(activation))?
            .iter()
        {
            let Body::RepositorySnapshot(observation) = &record.body else {
                continue;
            };
            if &observation.activation != activation {
                continue;
            }
            validate_observation(snapshot, observation)?;
            result.push(ActivationRepositorySnapshotView {
                reference: record.reference.clone(),
                content: observation.clone(),
            });
        }
        Ok(result)
    }
}

pub(super) fn validate_fields(
    observation: &ActivationRepositorySnapshot,
    owner: &ExecutionStoreOwner,
) -> Result<(), ExecutionContentError> {
    if observation.activation.session_id != owner.session_id {
        return Err(ExecutionContentError::OwnerMismatch);
    }
    encode_bounded(observation, 1024 * 1024)?;
    Ok(())
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// A directory `mktemp -d /tmp/axocoatl-baseline.XXXXXX` can name.
pub fn is_capture_baseline(path: &str) -> bool {
    path.strip_prefix("/tmp/axocoatl-baseline.")
        .is_some_and(|suffix| {
            (1..=64).contains(&suffix.len())
                && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric())
        })
}

fn validate_observation(
    snapshot: &DurableTurnSnapshot,
    observation: &ActivationRepositorySnapshot,
) -> Result<(), ExecutionContentError> {
    if observation.manifest_prefix.len() > 8192
        || observation.patch_prefix.len() > 12288
        || observation
            .unavailable
            .as_ref()
            .is_some_and(|reason| reason.len() > 4096)
        || observation.compared.as_ref().is_some_and(|compared| {
            compared
                .changed_paths
                .iter()
                .map(String::len)
                .sum::<usize>()
                > MAX_COMPARED_PATH_BYTES
        })
    {
        return Err(ExecutionContentError::Capacity);
    }
    let judgement = observation.judged_sha256.is_some()
        || observation.baseline.is_some()
        || observation.compared.is_some();
    if judgement
        && (observation.tree_sha256.is_none()
            || !observation
                .judged_sha256
                .as_deref()
                .is_some_and(valid_digest)
            || !observation
                .baseline
                .as_deref()
                .is_none_or(is_capture_baseline)
            || observation.compared.as_ref().is_some_and(|compared| {
                !valid_digest(&compared.before_sha256)
                    || compared
                        .changed_paths
                        .iter()
                        .any(|path| path.is_empty() || path.len() > 4096)
            }))
    {
        return Err(ExecutionContentError::Invalid(
            "invalid repository judgement capture",
        ));
    }
    if let Some(encoded) = &observation.patch_base64 {
        use base64::Engine as _;
        if encoded.len() > 700 * 1024 {
            return Err(ExecutionContentError::Capacity);
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|_| ExecutionContentError::Invalid("invalid protected patch bytes"))?;
        if bytes.len() > 512 * 1024
            || bytes.len() as u64 != observation.patch_bytes
            || observation.patch_sha256.as_deref() != Some(sha256(&bytes).as_str())
        {
            return Err(ExecutionContentError::Invalid(
                "protected patch digest differs",
            ));
        }
    }
    if observation.tree_sha256.is_some() {
        let valid_digest = |value: Option<&str>| value.is_some_and(valid_digest);
        if !valid_digest(observation.tree_sha256.as_deref())
            || !valid_digest(observation.patch_sha256.as_deref())
            || observation.unavailable.is_some()
            || observation.patch_base64.is_none()
        {
            return Err(ExecutionContentError::Invalid(
                "invalid checked repository identity",
            ));
        }
        if let Some(run) = &observation.condition_run {
            if observation.invocation.is_some() || !snapshot.contract().condition_run(run).is_some_and(|run| run.run.activations.contains(&observation.activation) && matches!(&run.resolution, Some(crate::turn_contract::ConditionEffectResolution::OutcomeRecorded {evidence}) if Some(evidence) == observation.outcome.as_ref())) { return Err(ExecutionContentError::Invalid("repository capture has no actual condition result")); }
        } else {
            let Some(invocation) = observation.invocation.as_ref().and_then(|id| {
                snapshot.contract().invocations().iter().find(|item| {
                    &item.invocation_id == id && item.activation == observation.activation
                })
            }) else {
                return Err(ExecutionContentError::Invalid(
                    "repository snapshot has no actual invocation",
                ));
            };
            if !matches!(&invocation.evidence, InvocationEvidence::Outcome { outcome: InvocationOutcome::Succeeded, evidence } if Some(evidence) == observation.outcome.as_ref())
            {
                return Err(ExecutionContentError::Invalid(
                    "repository snapshot has no successful effect receipt",
                ));
            }
        }
    } else if observation.unavailable.is_none() {
        return Err(ExecutionContentError::Invalid(
            "repository identity is neither checked nor unavailable",
        ));
    }
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use base64::Engine as _;

    /// One capture field of the command's output.
    fn field<'a>(output: &'a str, name: &str) -> &'a str {
        output
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{name}=")))
            .unwrap_or_else(|| panic!("capture has no {name}: {output}"))
    }

    /// Paths named by a capture's manifest.
    fn manifest_paths(output: &str) -> Vec<String> {
        let manifest = base64::engine::general_purpose::STANDARD
            .decode(field(output, "manifest_b64"))
            .unwrap();
        String::from_utf8(manifest)
            .unwrap()
            .lines()
            .map(|line| {
                let encoded = line.split('\t').next().unwrap();
                String::from_utf8(
                    base64::engine::general_purpose::STANDARD
                        .decode(encoded)
                        .unwrap(),
                )
                .unwrap()
            })
            .collect()
    }

    fn git(root: &std::path::Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    /// Configuration an Agent's shell could write in the home directory, or a
    /// writer in the repository itself, neither hides an untracked file from
    /// the capture nor makes the capture run a program.
    #[test]
    fn repository_capture_ignores_outside_configuration_and_runs_no_configured_program() {
        let repo = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let root = repo.path();
        let markers = tempfile::tempdir().unwrap();
        let program = |name: &str| {
            let path = markers.path().join(format!("{name}.sh"));
            std::fs::write(
                &path,
                format!(
                    "#!/bin/sh\ntouch '{}'\ncat\n",
                    markers.path().join(format!("ran-{name}")).display()
                ),
            )
            .unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path.display().to_string()
        };
        git(root, &["init", "--quiet"]);
        std::fs::write(root.join("tracked.txt"), "original\n").unwrap();
        git(root, &["add", "tracked.txt"]);
        git(
            root,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "base",
            ],
        );
        std::fs::write(root.join("tracked.txt"), "changed\n").unwrap();
        std::fs::write(root.join("secret.txt"), "secret\n").unwrap();
        let hide = home.path().join("hide");
        std::fs::write(&hide, "secret.txt\n").unwrap();
        std::fs::write(
            home.path().join(".gitconfig"),
            format!(
                "[core]\n\texcludesFile = {}\n\tfsmonitor = {}\n[diff]\n\texternal = {}\n\
                 [filter \"home\"]\n\tclean = {}\n\trequired = true\n",
                hide.display(),
                program("home-fsmonitor"),
                program("home-diff"),
                program("home-filter"),
            ),
        )
        .unwrap();
        let xdg = home.path().join(".config/git");
        std::fs::create_dir_all(&xdg).unwrap();
        std::fs::write(xdg.join("ignore"), "secret.txt\n").unwrap();
        std::fs::write(xdg.join("attributes"), "* filter=home\n").unwrap();
        // A writer could configure the repository itself the same way.
        for (key, value) in [
            ("core.fsmonitor", program("repo-fsmonitor")),
            ("diff.external", program("repo-diff")),
            ("filter.repo.clean", program("repo-filter")),
            ("filter.repo.process", program("repo-process")),
            ("filter.repo.required", "true".into()),
            ("color.ui", "always".into()),
            ("diff.noprefix", "true".into()),
        ] {
            git(root, &["config", key, &value]);
        }
        std::fs::write(
            root.join(".git/info/attributes"),
            "* filter=repo diff=repo\n",
        )
        .unwrap();
        let capture = std::process::Command::new("sh")
            .args(["-c", REPOSITORY_SNAPSHOT_COMMAND])
            .current_dir(root)
            .env("HOME", home.path())
            .env("XDG_CONFIG_HOME", home.path().join(".config"))
            .output()
            .unwrap();
        assert!(
            capture.status.success(),
            "{}",
            String::from_utf8_lossy(&capture.stderr)
        );
        let output = String::from_utf8(capture.stdout).unwrap();
        assert_eq!(manifest_paths(&output), ["secret.txt", "tracked.txt"]);
        let patch = String::from_utf8(
            base64::engine::general_purpose::STANDARD
                .decode(field(&output, "patch_b64"))
                .unwrap(),
        )
        .unwrap();
        assert!(
            patch.contains("diff --git a/secret.txt b/secret.txt"),
            "{patch}"
        );
        assert!(
            patch.contains("diff --git a/tracked.txt b/tracked.txt"),
            "{patch}"
        );
        assert!(patch.contains("+changed"), "{patch}");
        assert!(!patch.contains('\u{1b}'), "{patch}");
        let ran: Vec<_> = std::fs::read_dir(markers.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .filter(|name| name.starts_with("ran-"))
            .collect();
        assert!(ran.is_empty(), "the capture ran {ran:?}");
    }

    /// Runs one capture mode at `root` and returns its output.
    fn capture(root: &std::path::Path, mode: &str) -> Result<String, String> {
        let output = std::process::Command::new("sh")
            .args(["-c", &format!("{mode}{REPOSITORY_CAPTURE_SCRIPT}")])
            .current_dir(root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        if output.status.success() {
            Ok(String::from_utf8(output.stdout).unwrap())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).into_owned())
        }
    }

    /// A committed repository of 300 files, far beyond an 8 KiB manifest.
    fn sizeable_repository() -> tempfile::TempDir {
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path();
        git(root, &["init", "--quiet"]);
        std::fs::create_dir_all(root.join("lib")).unwrap();
        std::fs::create_dir_all(root.join("config")).unwrap();
        for index in 0..300 {
            std::fs::write(
                root.join(format!("lib/file-{index}.js")),
                format!("{index}\n"),
            )
            .unwrap();
        }
        std::fs::write(root.join("config/prod.js"), "prod\n").unwrap();
        std::fs::write(root.join("config/keep.js"), "keep\n").unwrap();
        std::fs::write(root.join(".gitignore"), "build/\n").unwrap();
        git(root, &["add", "-A"]);
        git(
            root,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "base",
            ],
        );
        repo
    }

    /// Keeps a Before manifest, runs `change`, then compares: the paths the
    /// After capture reports changed, or why the comparison failed.
    fn changed_by(change: &str) -> (u64, Result<Vec<String>, String>) {
        let repo = sizeable_repository();
        let root = repo.path();
        let kept = capture(root, "capture_mode=keep\n").unwrap();
        let manifest_bytes = field(&kept, "manifest_bytes").parse().unwrap();
        let baseline = field(&kept, "baseline").to_owned();
        let expected = field(&kept, "judged").to_owned();
        let status = std::process::Command::new("sh")
            .args(["-c", change])
            .current_dir(root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("BASELINE", &baseline)
            .status()
            .unwrap();
        assert!(status.success(), "{change}");
        let compared = capture(
            root,
            &format!("capture_mode=compare\nbaseline={baseline}\nexpected={expected}\n"),
        );
        let _ = std::fs::remove_dir_all(&baseline);
        let changed = compared.map(|output| {
            assert_eq!(field(&output, "compared"), expected);
            field(&output, "changed")
                .split(',')
                .filter(|encoded| !encoded.is_empty())
                .map(|encoded| {
                    String::from_utf8(
                        base64::engine::general_purpose::STANDARD
                            .decode(encoded)
                            .unwrap(),
                    )
                    .unwrap()
                })
                .collect()
        });
        (manifest_bytes, changed)
    }

    /// The complete comparison names every changed path in a real-size
    /// repository, whatever the index flags or ignore files an Agent's shell
    /// sets, and fails closed when the kept manifest is not the recorded one.
    #[test]
    fn write_scope_captures_compare_complete_manifests_that_an_agent_cannot_blind() {
        let (manifest_bytes, changed) = changed_by("printf x > config/prod.js");
        assert!(manifest_bytes > 8192, "{manifest_bytes}");
        assert_eq!(changed.unwrap(), ["config/prod.js"]);
        assert_eq!(changed_by("true").1.unwrap(), Vec::<String>::new());
        for (change, expected) in [
            (
                "git update-index --skip-worktree config/prod.js && printf x > config/prod.js",
                vec!["config/prod.js"],
            ),
            (
                "git update-index --assume-unchanged config/keep.js && printf x > config/keep.js",
                vec!["config/keep.js"],
            ),
            (
                "printf '*\\n' > config/.gitignore && printf x > config/evil.js",
                vec!["config/.gitignore"],
            ),
            (
                "mkdir fresh && printf '*\\n' > fresh/.gitignore && printf x > fresh/evil.js",
                vec!["fresh/.gitignore"],
            ),
            (
                "echo config/evil.js >> .git/info/exclude && printf x > config/evil.js",
                vec![".git/info/exclude"],
            ),
            (
                "printf '#!/bin/sh\\n' > .git/hooks/pre-commit",
                vec![".git/hooks/pre-commit"],
            ),
            ("git config core.hooksPath /tmp", vec![".git/config"]),
            (
                "git rm --quiet --cached config/keep.js",
                vec!["config/keep.js"],
            ),
            // Ignored build output is not judged.
            ("mkdir build && printf x > build/out.js", vec![]),
        ] {
            assert_eq!(changed_by(change).1.unwrap(), expected, "{change}");
        }
        for tamper in [
            "sed -i 1d \"$BASELINE/manifest\"",
            "rm -rf \"$BASELINE\"",
            "rm \"$BASELINE/manifest\" && ln -s /etc/hostname \"$BASELINE/manifest\"",
        ] {
            let failure = changed_by(tamper).1.unwrap_err();
            assert!(
                failure.contains("kept Before manifest"),
                "{tamper}: {failure}"
            );
        }
    }

    #[test]
    fn repository_capture_handles_shared_sandbox_ownership_only_at_the_exact_root() {
        use std::process::Command;
        let repo = tempfile::tempdir().unwrap();
        assert!(Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(repo.path())
            .status()
            .unwrap()
            .success());
        std::fs::write(repo.path().join("fixture.txt"), "original\n").unwrap();
        assert!(Command::new("git")
            .args(["add", "fixture.txt"])
            .current_dir(repo.path())
            .status()
            .unwrap()
            .success());
        std::fs::write(repo.path().join("fixture.txt"), "changed\n").unwrap();
        let baseline = Command::new("git")
            .args(["rev-parse", "--show-toplevel"])
            .current_dir(repo.path())
            .env("GIT_TEST_ASSUME_DIFFERENT_OWNER", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(!baseline.status.success());
        assert!(String::from_utf8_lossy(&baseline.stderr).contains("dubious ownership"));
        let capture = Command::new("sh")
            .args(["-c", REPOSITORY_SNAPSHOT_COMMAND])
            .current_dir(repo.path())
            .env("GIT_TEST_ASSUME_DIFFERENT_OWNER", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            capture.status.success(),
            "{}",
            String::from_utf8_lossy(&capture.stderr)
        );
        assert!(String::from_utf8_lossy(&capture.stdout).contains("patch_b64="));
        let nested = repo.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        let refused = Command::new("sh")
            .args(["-c", REPOSITORY_SNAPSHOT_COMMAND])
            .current_dir(nested)
            .env("GIT_TEST_ASSUME_DIFFERENT_OWNER", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(!refused.status.success());
    }
}
