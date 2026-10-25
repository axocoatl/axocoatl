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
/// its size and its first [`MAX_PATCH_PREFIX_BYTES`], read as Git writes it
/// and never stored whole, so a patch of any size is captured. What it writes
/// to standard error is bounded too: its first and last 8 KiB. A submodule
/// or nested checkout is one record of kind `special` whose files it does
/// not read: a submodule's digest is the commit it has checked out (the one
/// its index entry records when it has none Git can read), a nested
/// checkout's is `unread`, and every capture names its nested checkouts
/// (`unread`). A file it cannot read fails it (exit 65), as does a listing
/// that reaches the 1 GiB limit on its temporary files (exit 69) and more
/// nested checkouts than it names (exit 71).
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
    /// The nested Git checkouts (not submodules) in the tree this capture
    /// saw, each a `special` record whose digest is `unread`: no capture
    /// reads their files, so a change inside one is in neither the tree, the
    /// manifest nor the patch. At most [`MAX_UNREAD_PATH_BYTES`] of names.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unread_paths: Vec<String>,
}

/// The kind of a capture record for a submodule or a nested Git checkout,
/// whose files no capture reads.
pub const SPECIAL_KIND: &str = "special";

/// The digest of a [`SPECIAL_KIND`] record for a nested Git checkout that is
/// not a submodule: its files are not read. A submodule's digest is the
/// commit it has checked out.
pub const UNREAD_DIGEST: &str = "unread";

/// Most bytes of nested checkouts' names one capture records
/// ([`ActivationRepositorySnapshot::unread_paths`]); a capture with more
/// fails.
pub const MAX_UNREAD_PATH_BYTES: usize = 48 * 1024;

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
        || observation
            .unread_paths
            .iter()
            .map(String::len)
            .sum::<usize>()
            > MAX_UNREAD_PATH_BYTES
    {
        return Err(ExecutionContentError::Capacity);
    }
    if observation
        .unread_paths
        .iter()
        .any(|path| path.is_empty() || path.len() > 4096)
    {
        return Err(ExecutionContentError::Invalid(
            "invalid nested checkout path in a repository capture",
        ));
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
        let repo = crate::private_tempdir_test_support::private_tempdir();
        let home = crate::private_tempdir_test_support::private_tempdir();
        let root = repo.path();
        let markers = crate::private_tempdir_test_support::private_tempdir();
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
        let repo = crate::private_tempdir_test_support::private_tempdir();
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
        assert_eq!(field(&unchanged, "format"), "3");
        assert_eq!(field(&unchanged, "unread"), "");
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
        // The modes the rows below assert, whatever the umask (0664 under
        // Ubuntu's 002).
        for path in ["config/prod.js", "lib/file-7.js", "new file.txt"] {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(root.join(path), std::fs::Permissions::from_mode(0o644))
                .unwrap();
        }
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

    /// Each file is read by its own name, never as standard input or an
    /// option, and a name sha256sum would escape or split is read alone: a
    /// file named `-` is recorded with its own mode (the capture read the
    /// mode of its standard input for it before), and names with a leading
    /// dash, a backslash, a newline, a tab or a percent sign with their own
    /// content's digest and mode.
    #[test]
    fn every_file_is_read_by_its_own_name_whatever_the_name() {
        use std::os::unix::fs::PermissionsExt;
        let repo = sizeable_repository();
        let root = repo.path();
        let files = [
            ("-", 0o600),
            ("-n", 0o755),
            ("back\\slash", 0o644),
            ("new\nline", 0o640),
            ("tab\tname", 0o644),
            ("%s%n", 0o644),
        ];
        for (name, mode) in files {
            std::fs::write(root.join(name), format!("{name}\n")).unwrap();
            std::fs::set_permissions(root.join(name), std::fs::Permissions::from_mode(mode))
                .unwrap();
        }
        let output = capture(root, "capture_mode=observe\n").unwrap();
        let mut expected: Vec<_> = files
            .iter()
            .map(|(name, mode)| {
                (
                    (*name).to_owned(),
                    format!("{mode:o}"),
                    "file".to_owned(),
                    sha256_text(&format!("{name}\n")),
                )
            })
            .collect();
        expected.sort();
        let mut recorded = changed_lines(&output).unwrap();
        recorded.sort();
        assert_eq!(recorded, expected);
        // Git reads a path "-" as standard input; the patch still holds the
        // file's own content, under the name Git gives any other file.
        let patch = String::from_utf8(decoded(&output, "patch_b64")).unwrap();
        assert!(
            patch.contains("diff --git a/- b/-\nnew file mode 100644\nindex 0000000.."),
            "{patch}"
        );
        assert!(
            patch.contains("--- /dev/null\n+++ b/-\n@@ -0,0 +1 @@\n+-\n"),
            "{patch}"
        );
        assert!(!patch.contains("./-"), "{patch}");
    }

    /// A repository of thousands of files, far more than one batch of records
    /// holds, is recorded whole: the manifest's size and tree digest are the
    /// ones its files' records give, computed here, each record once. A
    /// submodule and a nested checkout are one special record each (the
    /// submodule's commit, the nested checkout unread) and cost no other
    /// path its own: when records were read one at a time, a submodule first
    /// in its batch lost every record after it in that batch.
    #[test]
    fn a_manifest_of_many_batches_holds_every_file_once() {
        use std::os::unix::fs::PermissionsExt;
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path();
        git(root, &["init", "--quiet"]);
        let committer = ["-c", "user.name=t", "-c", "user.email=t@example.invalid"];
        let sub = root.join("0-sub");
        std::fs::create_dir(&sub).unwrap();
        git(&sub, &["init", "--quiet"]);
        std::fs::write(sub.join("inner.txt"), "inner\n").unwrap();
        git(&sub, &["add", "-A"]);
        git(
            &sub,
            &[&committer[..], &["commit", "--quiet", "-m", "sub"]].concat(),
        );
        let mut lines = Vec::new();
        for index in 0..4000 {
            let name = format!("dir-{}/file {index} {}", index % 7, "x".repeat(index % 50));
            let path = root.join(&name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let content = format!("{index}\n");
            std::fs::write(&path, &content).unwrap();
            let mode = if index % 3 == 0 { 0o755 } else { 0o644 };
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            lines.push(format!(
                "{}\t{mode:o}\tfile\t{}\n",
                base64::engine::general_purpose::STANDARD.encode(&name),
                sha256_text(&content)
            ));
        }
        git(root, &["add", "-A"]);
        git(
            root,
            &[&committer[..], &["commit", "--quiet", "-m", "many"]].concat(),
        );
        let nested = root.join("0-nested");
        std::fs::create_dir(&nested).unwrap();
        git(&nested, &["init", "--quiet"]);
        let output = capture(root, "capture_mode=observe\n").unwrap();
        let commit = String::from_utf8(
            std::process::Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(&sub)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        let mode = |path: &std::path::Path| {
            format!(
                "{:o}",
                std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
            )
        };
        let special = |name: &str, mode: String, digest: &str| {
            format!(
                "{}\t{mode}\tspecial\t{digest}\n",
                base64::engine::general_purpose::STANDARD.encode(name)
            )
        };
        lines.push(special("0-sub", mode(&sub), commit.trim()));
        lines.push(special("0-nested", mode(&nested), "unread"));
        // The nested checkout, which Git lists as untracked, differs from
        // HEAD; the submodule does not.
        assert_eq!(
            changed_lines(&output).unwrap(),
            [(
                "0-nested".to_owned(),
                mode(&nested),
                "special".to_owned(),
                "unread".to_owned()
            )]
        );
        assert_eq!(
            field(&output, "unread"),
            base64::engine::general_purpose::STANDARD.encode("0-nested")
        );
        lines.sort();
        let manifest = lines.concat();
        assert_eq!(field(&output, "manifest_bytes"), manifest.len().to_string());
        assert_eq!(field(&output, "tree"), sha256_text(&manifest));
        // In every mode.
        let kept = capture(root, "capture_mode=keep\n").unwrap();
        assert_eq!(field(&kept, "tree"), sha256_text(&manifest));
        let _ = std::fs::remove_dir_all(field(&kept, "baseline"));
    }

    /// A capture that cannot read a file it records fails, and says so (exit
    /// 65), in every mode: a file in `.git/hooks` the capture's user cannot
    /// read, or any record that does not come out whole. Before, the records
    /// went straight into `sort -u`, which hid the failure, so the capture
    /// succeeded without the rest of that batch: with an unreadable hook,
    /// the kept manifest had none of Git's own files, and a later change to
    /// `.git/config` was not judged.
    #[test]
    fn a_capture_that_cannot_read_a_record_fails_and_says_so() {
        use std::os::unix::fs::PermissionsExt;
        let repo = sizeable_repository();
        let root = repo.path();
        // Every record coming out of step.
        let broken = REPOSITORY_CAPTURE_SCRIPT.replace("  if (failed) exit 70\n", "  exit 70\n");
        assert_ne!(broken, REPOSITORY_CAPTURE_SCRIPT);
        for mode in ["capture_mode=observe\n", "capture_mode=keep\n"] {
            let (code, stdout, stderr) = run_command(root, &format!("{mode}{broken}"));
            assert_eq!(code, 65, "{mode}: {stderr}");
            assert_eq!(stdout, "");
            assert!(
                stderr.ends_with("the capture could not read every file it records\n"),
                "{stderr}"
            );
        }
        // A hook its user cannot read; root reads any file.
        let hook = root.join(".git/hooks/zz-locked");
        std::fs::write(&hook, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&hook).is_err() {
            let (code, _, stderr) = run_command(
                root,
                &format!("capture_mode=keep\n{REPOSITORY_CAPTURE_SCRIPT}"),
            );
            assert_eq!(code, 65, "{stderr}");
            assert!(stderr.contains("zz-locked: Permission denied"), "{stderr}");
            // Observing reads no file of Git's own.
            assert!(capture(root, "capture_mode=observe\n").is_ok());
        }
    }

    /// A listing that reaches the limit on a capture's temporary files fails
    /// the capture, which says so (exit 69), whatever command was writing.
    #[test]
    fn a_listing_past_the_temporary_file_limit_says_so() {
        let repo = sizeable_repository();
        let small = REPOSITORY_CAPTURE_SCRIPT.replace("\nfile_limit=2097152\n", "\nfile_limit=8\n");
        assert_ne!(small, REPOSITORY_CAPTURE_SCRIPT);
        let (code, stdout, stderr) =
            run_command(repo.path(), &format!("capture_mode=observe\n{small}"));
        assert_eq!(code, 69, "{stderr}");
        assert_eq!(stdout, "");
        assert!(
            stderr.ends_with(
                "a listing of the repository's files reached the 1 GiB a capture may write\n"
            ),
            "{stderr}"
        );
        // The real limit holds the same repository.
        assert!(capture(repo.path(), "capture_mode=observe\n").is_ok());
    }

    /// A submodule is one special record whose digest is the commit it has
    /// checked out, in every mode: moving it to another commit changes the
    /// tree and a comparison names it, and the patch shows it as Git does,
    /// by its commits; a file changed inside it without a new commit is not
    /// read and changes nothing. Git does not look inside it, so nothing it
    /// configures runs (a clean filter its configuration named ran as the
    /// capture read it, in 1.4.0 too). Not checked out, or with a `.git`
    /// that names no repository, its record holds the commit the index
    /// records, as Git compares it, and never the superproject's HEAD. A
    /// nested checkout (not a submodule) is one special record whose digest
    /// is `unread`, named in every capture's `unread`, never a deleted path
    /// and never in the patch.
    #[test]
    fn a_submodule_is_recorded_by_its_commit_and_a_nested_checkout_as_unread() {
        use std::os::unix::fs::PermissionsExt;
        let committer = ["-c", "user.name=t", "-c", "user.email=t@example.invalid"];
        let upstream = tempfile::tempdir().unwrap();
        git(upstream.path(), &["init", "--quiet"]);
        std::fs::write(upstream.path().join(".gitattributes"), "* filter=evil\n").unwrap();
        for content in ["x\n", "y\n"] {
            std::fs::write(upstream.path().join("x"), content).unwrap();
            git(upstream.path(), &["add", "-A"]);
            git(
                upstream.path(),
                &[&committer[..], &["commit", "--quiet", "-m", content]].concat(),
            );
        }
        let repo = sizeable_repository();
        let root = repo.path();
        git(
            root,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "--quiet",
                upstream.path().to_str().unwrap(),
                "sub",
            ],
        );
        git(
            root,
            &[&committer[..], &["commit", "--quiet", "-m", "sub"]].concat(),
        );
        let rev = |dir: &std::path::Path, name: &str| {
            String::from_utf8(
                std::process::Command::new("git")
                    .args(["rev-parse", name])
                    .current_dir(dir)
                    .output()
                    .unwrap()
                    .stdout,
            )
            .unwrap()
            .trim()
            .to_owned()
        };
        let (newest, older) = (rev(upstream.path(), "HEAD"), rev(upstream.path(), "HEAD~1"));
        let markers = tempfile::tempdir().unwrap();
        let filter = markers.path().join("filter.sh");
        std::fs::write(
            &filter,
            format!(
                "#!/bin/sh\ntouch '{}'\ncat\n",
                markers.path().join("ran").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&filter, std::fs::Permissions::from_mode(0o755)).unwrap();
        git(
            &root.join("sub"),
            &["config", "filter.evil.clean", filter.to_str().unwrap()],
        );
        // Unchanged: its commit, and no changed line.
        let observed = capture(root, "capture_mode=observe\n").unwrap();
        assert_eq!(changed_lines(&observed).unwrap(), []);
        assert_eq!(field(&observed, "unread"), "");
        let kept = capture(root, "capture_mode=keep\n").unwrap();
        let baseline = field(&kept, "baseline").to_owned();
        let expected = field(&kept, "judged").to_owned();
        let kept_manifest = std::fs::read_to_string(format!("{baseline}/manifest")).unwrap();
        let sub_line = kept_manifest
            .lines()
            .find(|line| {
                line.starts_with(&format!(
                    "{}\t",
                    base64::engine::general_purpose::STANDARD.encode("sub")
                ))
            })
            .unwrap()
            .to_owned();
        assert!(
            sub_line.contains(&format!("\tspecial\t{newest}\tH 160000 {newest} 0")),
            "{sub_line}"
        );
        // A file changed inside it, without a new commit: not read.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(root.join("sub/x"), "dirty\n").unwrap();
        let dirty = capture(root, "capture_mode=observe\n").unwrap();
        assert_eq!(field(&dirty, "tree"), field(&observed, "tree"));
        assert!(
            !markers.path().join("ran").exists(),
            "the capture ran the submodule's filter"
        );
        git(&root.join("sub"), &["checkout", "--quiet", "--", "x"]);
        // Moved to another commit: the tree, the comparison, the changed
        // lines and the patch say so.
        git(&root.join("sub"), &["checkout", "--quiet", &older]);
        let compared = capture(
            root,
            &format!("capture_mode=compare\nbaseline={baseline}\nexpected={expected}\n"),
        )
        .unwrap();
        let _ = std::fs::remove_dir_all(&baseline);
        assert_ne!(field(&compared, "tree"), field(&kept, "tree"));
        assert_eq!(
            field(&compared, "changed"),
            base64::engine::general_purpose::STANDARD.encode("sub")
        );
        let (_, _, kind, digest) = changed_lines(&compared)
            .unwrap()
            .into_iter()
            .find(|(path, ..)| path == "sub")
            .unwrap();
        assert_eq!(
            (kind.as_str(), digest.as_str()),
            ("special", older.as_str())
        );
        let patch = String::from_utf8(decoded(&compared, "patch_b64")).unwrap();
        assert!(
            patch.starts_with("diff --git a/sub b/sub\nindex ")
                && patch.contains(&format!("-Subproject commit {newest}\n"))
                && patch.contains(&format!("+Subproject commit {older}\n")),
            "{patch}"
        );
        // Not checked out, or with a .git that names no repository: the
        // commit the index records, never the superproject's HEAD.
        git(&root.join("sub"), &["checkout", "--quiet", &newest]);
        git(root, &["submodule", "deinit", "--quiet", "--force", "sub"]);
        let deinit = capture(root, "capture_mode=observe\n").unwrap();
        assert_eq!(field(&deinit, "tree"), field(&observed, "tree"));
        std::fs::create_dir(root.join("sub/.git")).unwrap();
        let broken = capture(root, "capture_mode=observe\n").unwrap();
        assert_eq!(field(&broken, "tree"), field(&observed, "tree"));
        std::fs::remove_dir(root.join("sub/.git")).unwrap();
        // A nested checkout the turn makes: one unread record, in every
        // mode, named, never a deleted path, no patch section; a change
        // inside it changes nothing a capture records.
        let nested = root.join("tools/nested");
        std::fs::create_dir_all(&nested).unwrap();
        git(&nested, &["init", "--quiet"]);
        std::fs::write(nested.join("payload.sh"), "secret\n").unwrap();
        for mode in ["capture_mode=observe\n", "capture_mode=keep\n"] {
            let made = capture(root, mode).unwrap();
            assert_eq!(
                field(&made, "unread"),
                base64::engine::general_purpose::STANDARD.encode("tools/nested"),
                "{mode}"
            );
            let (_, _, kind, digest) = changed_lines(&made)
                .unwrap()
                .into_iter()
                .find(|(path, ..)| path == "tools/nested")
                .unwrap();
            assert_eq!((kind.as_str(), digest.as_str()), ("special", "unread"));
            let patch = String::from_utf8(decoded(&made, "patch_b64")).unwrap();
            assert!(!patch.contains("nested"), "{patch}");
            if mode.contains("keep") {
                let _ = std::fs::remove_dir_all(field(&made, "baseline"));
            }
        }
        let made = capture(root, "capture_mode=observe\n").unwrap();
        std::fs::write(nested.join("payload.sh"), "changed\n").unwrap();
        let changed = capture(root, "capture_mode=observe\n").unwrap();
        assert_eq!(field(&changed, "tree"), field(&made, "tree"));
    }

    /// More nested checkouts than one capture names fail it, and it says
    /// so (exit 71).
    #[test]
    fn more_nested_checkouts_than_a_capture_names_fail_it_and_say_so() {
        let repo = sizeable_repository();
        let root = repo.path();
        for index in 0..3 {
            let nested = root.join(format!("nested-{index}"));
            std::fs::create_dir(&nested).unwrap();
            git(&nested, &["init", "--quiet"]);
        }
        assert!(capture(root, "capture_mode=observe\n").is_ok());
        let small = REPOSITORY_CAPTURE_SCRIPT.replace(
            "if [ \"$unread_size\" -gt 65536 ]; then",
            "if [ \"$unread_size\" -gt 20 ]; then",
        );
        assert_ne!(small, REPOSITORY_CAPTURE_SCRIPT);
        let (code, stdout, stderr) = run_command(root, &format!("capture_mode=observe\n{small}"));
        assert_eq!(code, 71, "{stderr}");
        assert_eq!(stdout, "");
        assert!(
            stderr.ends_with(
                "the repository holds more nested Git checkouts than one capture lists\n"
            ),
            "{stderr}"
        );
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
                "head",
                "tree",
                "manifest_bytes",
                "manifest_b64",
                "patch_sha256",
                "patch_bytes",
            ] {
                assert_eq!(field(&observed, name), field(&unbounded, name), "{name}");
            }
            assert_eq!(field(&observed, "format"), "3");
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

    /// A capture's output as the 1.4 capture's would be: format 2, without
    /// the nested checkouts' names (here none).
    fn as_v14(output: &str) -> String {
        assert!(output.starts_with("format=3\n"), "{output}");
        assert!(output.contains("\nunread=\n"), "{output}");
        output
            .replacen("format=3\n", "format=2\n", 1)
            .replacen("\nunread=\n", "\n", 1)
    }

    /// Up to the 512 KiB the host records, a patch is printed whole and the
    /// capture is byte for byte the 1.4 capture, but for its format and the
    /// nested checkouts' names; one line more and only its first 12 KiB is.
    #[test]
    fn a_patch_up_to_the_recorded_bound_is_printed_whole_as_before() {
        let repo = sizeable_repository();
        let root = repo.path();
        let (_, unchanged, _) = run_command(root, REPOSITORY_SNAPSHOT_COMMAND_V3);
        assert_eq!(
            as_v14(&capture(root, "capture_mode=observe\n").unwrap()),
            unchanged
        );
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
            as_v14(&output),
            before,
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

    /// The patch a capture's Git commands write, exactly as the capture
    /// reads it: every change against HEAD, then each untracked file's.
    fn whole_patch(root: &std::path::Path) -> Vec<u8> {
        let output = std::process::Command::new("sh")
            .args([
                "-c",
                "git diff --no-ext-diff --no-textconv --binary HEAD -- && \
                 git ls-files --others --exclude-standard -z | xargs -0 -r -n 1 sh -c \
                 'git diff --no-ext-diff --no-textconv --binary --no-index -- /dev/null \"$0\"; \
                 [ $? -le 1 ]'",
            ])
            .current_dir(root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(output.status.success());
        output.stdout
    }

    /// A patch of any size is captured, in every mode, by its digest, its
    /// size and its first 12 KiB, within the bound: the capture reads it as
    /// Git writes it and never stores it whole. 80 MiB of new random file
    /// data, a patch of about 108 MB, is captured; a capture that stored the
    /// patch under its 64 MiB limit on temporary files stopped at about 49
    /// MiB of such data.
    #[test]
    fn a_patch_of_any_size_is_captured_by_its_digest_size_and_first_bytes() {
        let repo = sizeable_repository();
        let root = repo.path();
        let kept = capture(root, "capture_mode=keep\n").unwrap();
        let baseline = field(&kept, "baseline").to_owned();
        let expected = field(&kept, "judged").to_owned();
        std::fs::write(root.join("config/prod.js"), "prod, fixed\n").unwrap();
        random_file(&root.join("big.bin"), 80 * 1024 * 1024, 21);
        let whole = whole_patch(root);
        assert!(whole.len() > 100 * 1024 * 1024, "{}", whole.len());
        for mode in [
            "capture_mode=observe\n".to_owned(),
            format!("capture_mode=compare\nbaseline={baseline}\nexpected={expected}\n"),
        ] {
            let (code, output, stderr) =
                run_command(root, &format!("{mode}{REPOSITORY_CAPTURE_SCRIPT}"));
            assert_eq!(code, 0, "{stderr}");
            assert!(output.len() <= MOST_PRINTED, "{}", output.len());
            assert_eq!(field(&output, "patch_bytes"), whole.len().to_string());
            assert_eq!(field(&output, "patch_sha256"), sha256(&whole));
            assert_eq!(
                decoded(&output, "patch_b64"),
                whole[..MAX_PATCH_PREFIX_BYTES]
            );
            assert_eq!(field(&output, "changed_complete"), "1");
        }
        assert!(!std::path::Path::new(&baseline).exists());
    }

    /// What a capture's commands write to standard error is bounded to its
    /// first and last 8 KiB, so warnings Git writes for each of thousands of
    /// files never pass the bound the host reads it within; what it prints
    /// is unchanged.
    #[test]
    fn a_capture_bounds_what_it_writes_to_standard_error() {
        let repo = sizeable_repository();
        let root = repo.path();
        std::fs::write(root.join("config/prod.js"), "prod, fixed\n").unwrap();
        let quiet = capture(root, "capture_mode=observe\n").unwrap();
        // Every Git command first warns 64 KiB, as Git can for each file.
        let wrapper = tempfile::tempdir().unwrap();
        let git = String::from_utf8(
            std::process::Command::new("sh")
                .args(["-c", "command -v git"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        let script = wrapper.path().join("git");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\ni=0\nwhile [ $i -lt 1024 ]; do\n  printf 'warning: %063d\\n' \
                 $i >&2\n  i=$((i + 1))\ndone\nexec {} \"$@\"\n",
                git.trim()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = format!(
            "{}:{}",
            wrapper.path().display(),
            std::env::var("PATH").unwrap()
        );
        let noisy = std::process::Command::new("sh")
            .args(["-c", REPOSITORY_SNAPSHOT_COMMAND])
            .current_dir(root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("PATH", path)
            .output()
            .unwrap();
        assert!(noisy.status.success());
        assert_eq!(String::from_utf8(noisy.stdout).unwrap(), quiet);
        let stderr = String::from_utf8(noisy.stderr).unwrap();
        assert_eq!(stderr.len(), 16 * 1024, "{}", stderr.len());
        assert!(stderr.starts_with(&format!("warning: {:063}\n", 0)));
        assert!(stderr.ends_with(&format!("warning: {:063}\n", 1023)));
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

    /// A comparison whose changed list leaves no room for a patch within the
    /// 512 KiB the host records whole prints only the patch's first 12 KiB,
    /// beside its digest and size, as for a patch too large to record: 1,000
    /// files under .git/hooks with 200-byte names, which the comparison lists
    /// and the patch does not hold, beside a 396 KB patch.
    #[test]
    fn a_long_changed_list_leaves_a_patch_within_the_bound_only_its_first_bytes() {
        let repo = sizeable_repository();
        let root = repo.path();
        let kept = capture(root, "capture_mode=keep\n").unwrap();
        let baseline = field(&kept, "baseline").to_owned();
        let expected = field(&kept, "judged").to_owned();
        for index in 0..1000 {
            let name = format!("{index:05}-{}", "h".repeat(194));
            std::fs::write(root.join(".git/hooks").join(name), "#!/bin/sh\n").unwrap();
        }
        let line = format!("{}\n", "x".repeat(63));
        std::fs::write(root.join("notes.txt"), line.repeat(6000)).unwrap();
        let whole = whole_patch(root);
        assert!(
            whole.len() > 390_000 && whole.len() <= MAX_RECORDED_PATCH_BYTES,
            "{}",
            whole.len()
        );
        let (code, output, stderr) = run_command(
            root,
            &format!(
                "capture_mode=compare\nbaseline={baseline}\nexpected={expected}\n\
                 {REPOSITORY_CAPTURE_SCRIPT}"
            ),
        );
        assert_eq!(code, 0, "{stderr}");
        assert!(output.len() <= MOST_PRINTED, "{}", output.len());
        assert_eq!(field(&output, "changed").split(',').count(), 1001);
        assert_eq!(field(&output, "patch_bytes"), whole.len().to_string());
        assert_eq!(field(&output, "patch_sha256"), sha256(&whole));
        assert_eq!(
            decoded(&output, "patch_b64"),
            whole[..MAX_PATCH_PREFIX_BYTES]
        );
        // Observed alone, the same patch is printed whole.
        let observed = capture(root, "capture_mode=observe\n").unwrap();
        assert_eq!(decoded(&observed, "patch_b64"), whole);
    }

    #[test]
    fn repository_capture_handles_shared_sandbox_ownership_only_at_the_exact_root() {
        use std::process::Command;
        let repo = crate::private_tempdir_test_support::private_tempdir();
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
