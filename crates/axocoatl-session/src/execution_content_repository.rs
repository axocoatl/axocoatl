//! Actual repository observations, linked to their canonical tool receipt.
use super::*;
use crate::turn_contract::InvocationEvidence;

/// Run at the exact working root to observe HEAD, the file manifest and the
/// patch against HEAD. It writes only under its own temporary directory and
/// takes no git locks.
pub const REPOSITORY_SNAPSHOT_COMMAND: &str =
    include_str!("execution_content_repository_snapshot.sh");

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
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ActivationRepositorySnapshotView {
    pub reference: EvidenceRef,
    pub content: ActivationRepositorySnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StandingRepositoryCheck {
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StandingRepositoryCheckView {
    pub reference: EvidenceRef,
    pub content: StandingRepositoryCheck,
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
        for record in self.data.records.iter().rev() {
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
    pub fn retain_standing_check(
        &mut self,
        snapshot: &DurableTurnSnapshot,
        check: StandingRepositoryCheck,
    ) -> Result<EvidenceRef, ExecutionContentError> {
        self.require_activation(snapshot, &check.activation)?;
        self.validate_standing_check(snapshot, &check)?;
        self.append(Body::StandingRepositoryCheck(check))
    }
    pub fn standing_checks(
        &self,
        snapshot: &DurableTurnSnapshot,
        activation: &ActivationRef,
    ) -> Result<Vec<StandingRepositoryCheckView>, ExecutionContentError> {
        self.require_activation(snapshot, activation)?;
        let mut result = Vec::new();
        for record in &self.data.records {
            let Body::StandingRepositoryCheck(check) = &record.body else {
                continue;
            };
            if &check.activation != activation {
                continue;
            }
            self.validate_standing_check(snapshot, check)?;
            result.push(StandingRepositoryCheckView {
                reference: record.reference.clone(),
                content: check.clone(),
            });
        }
        Ok(result)
    }
    fn validate_standing_check(
        &self,
        snapshot: &DurableTurnSnapshot,
        check: &StandingRepositoryCheck,
    ) -> Result<(), ExecutionContentError> {
        let arguments = self
            .tool_arguments(snapshot, &check.activation, &check.invocation)?
            .ok_or(ExecutionContentError::Invalid(
                "check has no protected arguments",
            ))?;
        let actual: serde_json::Value =
            serde_json::from_slice(&self.read_tool_arguments(&arguments)?)?;
        if actual != serde_json::json!({"command":crate::turn_checks::check_command(&check.argv)?})
        {
            return Err(ExecutionContentError::Invalid(
                "check arguments differ from the recorded command",
            ));
        }
        let result = self
            .tool_result(&arguments)?
            .ok_or(ExecutionContentError::Invalid(
                "check has no protected result",
            ))?;
        if result.is_truncated() || result.protected_result().evidence_ref != check.outcome {
            return Err(ExecutionContentError::Invalid(
                "check result is missing or incomplete",
            ));
        }
        let actual: Result<serde_json::Value, String> =
            serde_json::from_slice(&self.read_tool_result(&result)?)?;
        let exit = actual
            .ok()
            .and_then(|value| value.get("exit_code").and_then(serde_json::Value::as_i64))
            .and_then(|exit| i32::try_from(exit).ok());
        if exit != check.exit_code {
            return Err(ExecutionContentError::Invalid(
                "check status differs from the actual process result",
            ));
        }
        let observations = self.repository_snapshots(snapshot, &check.activation)?;
        let before = observations
            .iter()
            .find(|item| {
                item.reference == check.before
                    && item.content.phase
                        == (RepositorySnapshotPhase::BeforeCheck { index: check.index })
            })
            .ok_or(ExecutionContentError::Invalid(
                "check lost its before snapshot",
            ))?;
        let after = observations
            .iter()
            .find(|item| {
                item.reference == check.after
                    && item.content.phase
                        == (RepositorySnapshotPhase::AfterCheck { index: check.index })
            })
            .ok_or(ExecutionContentError::Invalid(
                "check lost its after snapshot",
            ))?;
        if !snapshot.contract().invocations().iter().any(|item| item.invocation_id == check.invocation && item.activation == check.activation && matches!(&item.evidence, InvocationEvidence::Outcome {evidence,..} if evidence == &check.outcome)) { return Err(ExecutionContentError::Invalid("check lost its actual invocation result")); }
        let stable = before.content.tree_sha256.is_some()
            && before.content.tree_sha256 == after.content.tree_sha256;
        if check.candidate_sha256 != after.content.tree_sha256
            || check.passed != (stable && check.exit_code == Some(0))
        {
            return Err(ExecutionContentError::Invalid(
                "check verdict differs from its actual candidate",
            ));
        }
        Ok(())
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
        for record in &self.data.records {
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
    {
        return Err(ExecutionContentError::Capacity);
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
        let valid_digest = |value: Option<&str>| {
            value.is_some_and(|value| {
                value.len() == 64
                    && value
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            })
        };
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
