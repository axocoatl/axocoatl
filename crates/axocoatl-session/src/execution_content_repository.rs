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
/// repository, and nothing the repository configures can run a program. Its
/// output stays within the smallest capture bound whatever the change: a
/// patch larger than [`MAX_RECORDED_PATCH_BYTES`] is recorded by its digest,
/// its size and its first [`MAX_PATCH_PREFIX_BYTES`]. A patch that reaches
/// the 64 MiB limit on its temporary files (about 49 MiB of new binary file
/// data) ends it with exit code 68: a change larger than it records.
pub const REPOSITORY_SNAPSHOT_COMMAND: &str = concat!(
    "capture_mode=observe\n",
    include_str!("execution_content_repository_snapshot.sh")
);

/// The capture command of earlier releases, which let Git read the home
/// directory's configuration. Turns admitted with it keep it: their recorded
/// check definitions name it, so they still load, render and finish.
pub const REPOSITORY_SNAPSHOT_COMMAND_V1: &str =
    include_str!("execution_content_repository_snapshot_v1.sh");

/// The capture command of 1.2 and 1.3 (`format=1`), which recorded no lines
/// of the paths that differ from `HEAD`. Turns admitted with it keep it.
pub const REPOSITORY_SNAPSHOT_COMMAND_V2: &str = concat!(
    "capture_mode=observe\n",
    include_str!("execution_content_repository_snapshot_v2.sh")
);

/// The capture command of 1.4 and the first 1.5 builds, which printed the
/// whole patch however large, so a change whose patch passed the capture's
/// output bound could not be recorded. Turns admitted with it keep it.
pub const REPOSITORY_SNAPSHOT_COMMAND_V3: &str = concat!(
    "capture_mode=observe\n",
    include_str!("execution_content_repository_snapshot_v3.sh")
);

/// Most bytes of the patch against `HEAD` one capture records whole
/// ([`ActivationRepositorySnapshot::patch_base64`]). A larger patch is
/// recorded by its digest and size and [`MAX_PATCH_PREFIX_BYTES`] of it.
pub const MAX_RECORDED_PATCH_BYTES: usize = 512 * 1024;

/// Most bytes of a patch one capture keeps as text to show
/// ([`ActivationRepositorySnapshot::patch_prefix`]).
pub const MAX_PATCH_PREFIX_BYTES: usize = 12 * 1024;

/// Most bytes of the manifest lines of the paths that differ from `HEAD`
/// one capture records ([`ActivationRepositorySnapshot::changed_manifest`]).
pub const MAX_CHANGED_MANIFEST_BYTES: usize = 576 * 1024;

/// Most paths that differ from `HEAD` whose lines one capture records
/// ([`ActivationRepositorySnapshot::changed_manifest`]); with more, or when
/// their lines do not fit its output, it records none.
pub const MAX_CHANGED_PATHS: usize = 10_000;

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
    /// The patch's first bytes as text, at most [`MAX_PATCH_PREFIX_BYTES`],
    /// to show; never file input.
    pub patch_prefix: String,
    /// Whether the capture recorded the whole patch (`patch_base64`). A
    /// capture whose patch is larger than [`MAX_RECORDED_PATCH_BYTES`], or
    /// does not fit its output beside the rest, records only its digest,
    /// size and prefix.
    pub patch_complete: bool,
    /// The whole patch against `HEAD`, exactly, when `patch_complete`;
    /// otherwise absent.
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
    /// The manifest lines (`<base64 path>\t<mode>\t<kind>\t<sha256>`) of
    /// every path whose state differs from `HEAD`, as the tree this capture
    /// saw had them, whatever the repository's size; a path gone from it is
    /// `0\tdeleted\tmissing`. Absent when the capture predates it (format 1)
    /// or the lines did not fit its output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed_manifest: Option<String>,
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
        || observation
            .changed_manifest
            .as_ref()
            .is_some_and(|lines| lines.len() > MAX_CHANGED_MANIFEST_BYTES)
        || observation.patch_prefix.len() > MAX_PATCH_PREFIX_BYTES
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
    // The protected patch is only ever the whole patch: a capture that could
    // not record it whole keeps its digest, size and a prefix to show, and
    // nothing anything could apply.
    if let Some(encoded) = &observation.patch_base64 {
        use base64::Engine as _;
        if encoded.len() > 700 * 1024 {
            return Err(ExecutionContentError::Capacity);
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|_| ExecutionContentError::Invalid("invalid protected patch bytes"))?;
        if bytes.len() > MAX_RECORDED_PATCH_BYTES
            || !observation.patch_complete
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
            || observation.patch_base64.is_some() != observation.patch_complete
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

    /// The capture records the lines of at most [`MAX_CHANGED_PATHS`]
    /// paths that differ from HEAD, the limit the host names when it cannot
    /// attribute a change of more.
    #[test]
    fn the_capture_lists_at_most_the_changed_paths_the_host_names() {
        assert!(REPOSITORY_CAPTURE_SCRIPT
            .contains(&format!("if [ \"$differ_count\" -le {MAX_CHANGED_PATHS} ]")));
    }

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

    /// The lines a capture records of the paths that differ from HEAD:
    /// `(path, mode, kind, digest)`, or `None` when it left them out.
    fn changed_lines(output: &str) -> Option<Vec<(String, String, String, String)>> {
        if field(output, "changed_complete") != "1" {
            assert_eq!(field(output, "changed_b64"), "");
            return None;
        }
        let text = String::from_utf8(
            base64::engine::general_purpose::STANDARD
                .decode(field(output, "changed_b64"))
                .unwrap(),
        )
        .unwrap();
        Some(
            text.lines()
                .map(|line| {
                    let fields: Vec<&str> = line.split('\t').collect();
                    let path = base64::engine::general_purpose::STANDARD
                        .decode(fields[0])
                        .unwrap();
                    (
                        String::from_utf8(path).unwrap(),
                        fields[1].to_owned(),
                        fields[2].to_owned(),
                        fields[3].to_owned(),
                    )
                })
                .collect(),
        )
    }

    fn sha256_text(text: &str) -> String {
        use sha2::Digest as _;
        format!("{:x}", sha2::Sha256::digest(text.as_bytes()))
    }

    /// In a repository far beyond the 8 KiB manifest, a capture still
    /// records, for every path that differs from HEAD, the line its whole
    /// manifest has: edits, additions, a link, a deletion, a path removed
    /// from the index and the worktree, staged and unstaged changes; never
    /// an unchanged or ignored path.
    #[test]
    fn a_capture_records_the_manifest_lines_of_every_path_that_differs_from_head() {
        let repo = sizeable_repository();
        let root = repo.path();
        let unchanged = capture(root, "capture_mode=observe\n").unwrap();
        assert_eq!(field(&unchanged, "format"), "2");
        assert!(field(&unchanged, "manifest_bytes").parse::<u64>().unwrap() > 8192);
        assert_eq!(changed_lines(&unchanged), Some(Vec::new()));

        std::fs::write(root.join("config/prod.js"), "prod, fixed\n").unwrap();
        std::fs::write(root.join("lib/file-7.js"), "staged\n").unwrap();
        git(root, &["add", "lib/file-7.js"]);
        std::fs::remove_file(root.join("lib/file-1.js")).unwrap();
        git(root, &["rm", "--quiet", "lib/file-2.js"]);
        std::fs::write(root.join("new file.txt"), "new\n").unwrap();
        std::os::unix::fs::symlink("prod.js", root.join("config/alias.js")).unwrap();
        std::fs::create_dir_all(root.join("build")).unwrap();
        std::fs::write(root.join("build/out.js"), "ignored\n").unwrap();
        let output = capture(root, "capture_mode=observe\n").unwrap();
        let row = |path: &str, mode: &str, kind: &str, digest: &str| {
            (
                path.to_owned(),
                mode.to_owned(),
                kind.to_owned(),
                digest.to_owned(),
            )
        };
        assert_eq!(
            changed_lines(&output).unwrap(),
            vec![
                row("config/alias.js", "777", "link", &sha256_text("prod.js")),
                row(
                    "config/prod.js",
                    "644",
                    "file",
                    &sha256_text("prod, fixed\n")
                ),
                row("lib/file-1.js", "0", "deleted", "missing"),
                row("lib/file-2.js", "0", "deleted", "missing"),
                row("lib/file-7.js", "644", "file", &sha256_text("staged\n")),
                row("new file.txt", "644", "file", &sha256_text("new\n")),
            ]
        );
        // The same lines in every mode, which also report their judgement.
        let kept = capture(root, "capture_mode=keep\n").unwrap();
        assert_eq!(changed_lines(&kept), changed_lines(&output));
        let _ = std::fs::remove_dir_all(field(&kept, "baseline"));
    }

    /// `len` bytes no compressor shrinks, as a compiled test program's would
    /// barely be, at `path`.
    fn random_file(path: &std::path::Path, len: usize, seed: u64) {
        let mut state = seed | 1;
        let mut bytes = Vec::with_capacity(len + 8);
        while bytes.len() < len {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            bytes.extend_from_slice(&state.to_le_bytes());
        }
        bytes.truncate(len);
        std::fs::write(path, bytes).unwrap();
    }

    /// The output of a capture command at `root`: its exit code, standard
    /// output and standard error.
    fn run_command(root: &std::path::Path, command: &str) -> (i32, String, String) {
        let output = std::process::Command::new("sh")
            .args(["-c", command])
            .current_dir(root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        (
            output.status.code().unwrap_or(-1),
            String::from_utf8(output.stdout).unwrap(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }

    /// The most a capture prints: the 768 KiB of output a check's capture
    /// takes, less the 16 KiB kept for the fixed lines, plus those lines.
    const MOST_PRINTED: usize = 768 * 1024 - 16 * 1024 + 1024;

    fn decoded(output: &str, name: &str) -> Vec<u8> {
        base64::engine::general_purpose::STANDARD
            .decode(field(output, name))
            .unwrap()
    }

    /// A change whose patch is far larger than a capture's output may take
    /// (compiled test programs left in the repository root, more than 5 MiB
    /// together, or one 9 MiB file) is captured in every mode within the
    /// bound: the whole patch's digest and size and its first 12 KiB, exactly
    /// the 1.4 capture's patch's, every other field the 1.4 capture's, and
    /// the lines of every changed path, which 1.4 left out. The comparison
    /// a write scope is judged on names every changed path.
    #[test]
    fn a_change_too_large_for_one_capture_is_recorded_by_digest_size_and_first_bytes() {
        for files in [
            &[("test_fix", 4_628_176), ("test_reproduction", 4_627_816)][..],
            &[("big.bin", 9 * 1024 * 1024)][..],
        ] {
            let repo = sizeable_repository();
            let root = repo.path();
            let kept = capture(root, "capture_mode=keep\n").unwrap();
            let baseline = field(&kept, "baseline").to_owned();
            let expected = field(&kept, "judged").to_owned();
            std::fs::write(root.join("config/prod.js"), "prod, fixed\n").unwrap();
            for (index, (name, len)) in files.iter().enumerate() {
                random_file(&root.join(name), *len, index as u64 + 7);
            }
            let (code, unbounded, _) = run_command(root, REPOSITORY_SNAPSHOT_COMMAND_V3);
            assert_eq!(code, 0);
            let whole = decoded(&unbounded, "patch_b64");
            assert!(whole.len() > 5 * 1024 * 1024, "{}", whole.len());
            assert!(
                unbounded.len() > 1024 * 1024,
                "1.4's capture overflowed its bound"
            );
            let observed = capture(root, "capture_mode=observe\n").unwrap();
            assert!(observed.len() <= MOST_PRINTED, "{}", observed.len());
            assert_eq!(field(&observed, "patch_bytes"), whole.len().to_string());
            assert_eq!(field(&observed, "patch_sha256"), sha256(&whole));
            assert_eq!(
                decoded(&observed, "patch_b64"),
                whole[..MAX_PATCH_PREFIX_BYTES]
            );
            for name in [
                "format",
                "head",
                "tree",
                "manifest_bytes",
                "manifest_b64",
                "patch_sha256",
                "patch_bytes",
            ] {
                assert_eq!(field(&observed, name), field(&unbounded, name), "{name}");
            }
            assert_eq!(field(&unbounded, "changed_complete"), "0");
            let mut changed: Vec<(String, String)> = changed_lines(&observed)
                .unwrap()
                .into_iter()
                .map(|(path, _, kind, digest)| (path, format!("{kind} {digest}")))
                .collect();
            changed.sort();
            let mut expected_lines = vec![(
                "config/prod.js".to_owned(),
                format!("file {}", sha256_text("prod, fixed\n")),
            )];
            for (name, _) in files {
                let bytes = std::fs::read(root.join(name)).unwrap();
                expected_lines.push(((*name).to_owned(), format!("file {}", sha256(&bytes))));
            }
            expected_lines.sort();
            assert_eq!(changed, expected_lines);

            let compared = capture(
                root,
                &format!("capture_mode=compare\nbaseline={baseline}\nexpected={expected}\n"),
            )
            .unwrap();
            assert!(compared.len() <= MOST_PRINTED, "{}", compared.len());
            assert_eq!(field(&compared, "patch_sha256"), sha256(&whole));
            assert_eq!(
                decoded(&compared, "patch_b64"),
                whole[..MAX_PATCH_PREFIX_BYTES]
            );
            let mut listed: Vec<String> = field(&compared, "changed")
                .split(',')
                .map(|encoded| {
                    String::from_utf8(
                        base64::engine::general_purpose::STANDARD
                            .decode(encoded)
                            .unwrap(),
                    )
                    .unwrap()
                })
                .collect();
            listed.sort();
            let names: Vec<String> = expected_lines.into_iter().map(|(path, _)| path).collect();
            assert_eq!(listed, names);
            assert!(!std::path::Path::new(&baseline).exists());
        }
    }

    /// Up to the 512 KiB the host records, a patch is printed whole and the
    /// capture is byte for byte the 1.4 capture; one line more and only its
    /// first 12 KiB is.
    #[test]
    fn a_patch_up_to_the_recorded_bound_is_printed_whole_as_before() {
        let repo = sizeable_repository();
        let root = repo.path();
        let (_, unchanged, _) = run_command(root, REPOSITORY_SNAPSHOT_COMMAND_V3);
        assert_eq!(capture(root, "capture_mode=observe\n").unwrap(), unchanged);
        std::fs::write(root.join("config/prod.js"), "prod, fixed\n").unwrap();
        // An untracked text file whose patch is within one line of the bound.
        let line = format!("{}\n", "x".repeat(63));
        std::fs::write(root.join("notes.txt"), line.repeat(8000)).unwrap();
        let (_, probe, _) = run_command(root, REPOSITORY_SNAPSHOT_COMMAND_V3);
        let size: usize = field(&probe, "patch_bytes").parse().unwrap();
        // Each line adds 65 bytes of patch ("+", 63 bytes, newline).
        let fit = 8000 + (MAX_RECORDED_PATCH_BYTES - size) / 65;
        std::fs::write(root.join("notes.txt"), line.repeat(fit)).unwrap();
        let (_, before, _) = run_command(root, REPOSITORY_SNAPSHOT_COMMAND_V3);
        let size: usize = field(&before, "patch_bytes").parse().unwrap();
        assert!(
            size <= MAX_RECORDED_PATCH_BYTES && size + 65 > MAX_RECORDED_PATCH_BYTES,
            "{size}"
        );
        let output = capture(root, "capture_mode=observe\n").unwrap();
        assert_eq!(
            output, before,
            "a patch within the bound is printed as 1.4 did"
        );
        assert_eq!(field(&output, "changed_complete"), "1");
        std::fs::write(root.join("notes.txt"), line.repeat(fit + 1)).unwrap();
        let (_, over, _) = run_command(root, REPOSITORY_SNAPSHOT_COMMAND_V3);
        let whole = decoded(&over, "patch_b64");
        assert!(whole.len() > MAX_RECORDED_PATCH_BYTES);
        let output = capture(root, "capture_mode=observe\n").unwrap();
        assert_eq!(
            decoded(&output, "patch_b64"),
            whole[..MAX_PATCH_PREFIX_BYTES]
        );
        assert_eq!(field(&output, "patch_bytes"), whole.len().to_string());
        assert_eq!(field(&output, "changed_complete"), "1");
    }

    /// A patch that reaches the 64 MiB limit on a capture's temporary files
    /// stops the capture with its own exit code, 68, which the host reads as
    /// a change larger than Axocoatl records, in every mode: 50 MiB of new
    /// random file data does (Git's binary patch writes 52 bytes as a line of
    /// 67), and 49 MiB is still captured, as any change too large to record
    /// whole.
    #[test]
    fn a_patch_past_the_temporary_file_limit_is_named_as_too_large() {
        let repo = sizeable_repository();
        let root = repo.path();
        let kept = capture(root, "capture_mode=keep\n").unwrap();
        let baseline = field(&kept, "baseline").to_owned();
        let expected = field(&kept, "judged").to_owned();
        random_file(&root.join("big.bin"), 49 * 1024 * 1024, 21);
        let output = capture(root, "capture_mode=observe\n").unwrap();
        let size: u64 = field(&output, "patch_bytes").parse().unwrap();
        assert!(size > 63 * 1024 * 1024 && size < 64 * 1024 * 1024, "{size}");
        assert_eq!(field(&output, "changed_complete"), "1");
        random_file(&root.join("big.bin"), 50 * 1024 * 1024, 21);
        for mode in [
            "capture_mode=observe\n".to_owned(),
            format!("capture_mode=compare\nbaseline={baseline}\nexpected={expected}\n"),
        ] {
            let (code, output, stderr) =
                run_command(root, &format!("{mode}{REPOSITORY_CAPTURE_SCRIPT}"));
            assert_eq!(code, 68, "{stderr}");
            assert!(
                stderr.contains("the patch is larger than one capture records"),
                "{stderr}"
            );
            assert_eq!(output, "");
        }
        let _ = std::fs::remove_dir_all(&baseline);
    }

    /// A comparison whose changed list is long leaves the patch less room,
    /// so the whole output still fits; a list longer than the host accepts
    /// is refused before anything is printed, and the kept manifest is
    /// removed as a finished comparison removes it.
    #[test]
    fn a_long_changed_list_takes_the_patchs_room_and_a_longer_one_is_refused() {
        for (files, refused) in [(1700, false), (2100, true)] {
            let repo = sizeable_repository();
            let root = repo.path();
            let kept = capture(root, "capture_mode=keep\n").unwrap();
            let baseline = field(&kept, "baseline").to_owned();
            let expected = field(&kept, "judged").to_owned();
            std::fs::create_dir(root.join("generated")).unwrap();
            for index in 0..files {
                // 200-byte names: about 270 bytes each in the changed list.
                let name = format!("{index:05}-{}", "n".repeat(194));
                std::fs::write(root.join("generated").join(name), index.to_string()).unwrap();
            }
            random_file(&root.join("blob.bin"), 2 * 1024 * 1024, 3);
            let (code, output, stderr) = run_command(
                root,
                &format!(
                    "capture_mode=compare\nbaseline={baseline}\nexpected={expected}\n\
                     {REPOSITORY_CAPTURE_SCRIPT}"
                ),
            );
            assert!(!std::path::Path::new(&baseline).exists());
            if refused {
                assert_eq!(code, 67, "{stderr}");
                assert!(stderr.contains("more than one capture records"), "{stderr}");
                assert_eq!(output, "");
                continue;
            }
            assert_eq!(code, 0, "{stderr}");
            assert!(output.len() <= MOST_PRINTED, "{}", output.len());
            let listed = field(&output, "changed");
            assert!(
                listed.len() > 400 * 1024 && listed.len() <= 512 * 1024,
                "{}",
                listed.len()
            );
            assert_eq!(listed.split(',').count(), files + 1);
            assert_eq!(decoded(&output, "patch_b64").len(), MAX_PATCH_PREFIX_BYTES);
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
