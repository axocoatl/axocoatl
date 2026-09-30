use super::*;
use crate::bootstrap::native_turn::{NativeFirstTurnRequest, NativeStandingWork};
use axocoatl_session::execution_content::{ConditionProcessStatus, RepositorySnapshotPhase};
use axocoatl_session::team_work::{standing_check_definitions, standing_condition_id};

impl DispatchState {
    pub(super) fn standing_work(&self) -> Result<Option<NativeStandingWork>> {
        let Some((_, admission)) = self
            .content
            .turn_admission(&self.canonical, &self.turn_id)
            .map_err(error)?
        else {
            return Ok(None);
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&admission.source) else {
            return Ok(None);
        };
        if value
            .get("standing_work")
            .is_none_or(serde_json::Value::is_null)
        {
            return Ok(None);
        }
        let request: NativeFirstTurnRequest = serde_json::from_value(value).map_err(error)?;
        Ok(request.standing_work)
    }
}

impl SessionDispatchController {
    /// Producer metadata selects a candidate; only the owned observation proves
    /// that this execution is actually using that candidate.
    pub(crate) fn validate_standing_candidate(&self, _activation: &ActivationRef) -> Result<()> {
        let state = self.lock()?;
        let Some(work) = state.standing_work()? else {
            return Ok(());
        };
        let snapshot = state.canonical.snapshot(&state.turn_id).map_err(error)?;
        let mut observations = Vec::new();
        for item in snapshot.contract().activations() {
            observations.extend(
                state
                    .content
                    .repository_snapshots(&snapshot, &item.activation)
                    .map_err(error)?,
            );
        }
        let verified = observations
            .iter()
            .filter(|item| item.content.phase == RepositorySnapshotPhase::Before)
            .any(|item| {
                let before = &item.content;
                if before.tree_sha256.is_none() {
                    return false;
                }
                match work.subject.kind.as_str() {
                    "commit" | "git_commit" => {
                        before.head.as_deref() == Some(work.subject.version.as_str())
                            && before.patch_bytes == 0
                    }
                    "tree" | "tree_sha256" | "build" | "artifact" | "release_candidate" => {
                        before.tree_sha256.as_deref() == Some(work.subject.version.as_str())
                    }
                    // A signal names no producer candidate. The host rechecked
                    // the signaled source bytes immediately before admission;
                    // this execution's own Before capture is its starting tree.
                    "signal_field" => true,
                    _ => false,
                }
            });
        if !verified {
            return Err(error("The declared candidate is not verified in this Session checkout; a commit must match a clean checkout, and build/artifact versions must match the captured tree SHA-256"));
        }
        Ok(())
    }

    /// The one turn driver runs checks only after the complete required Agent
    /// frontier is accepted. Every command uses the existing independent check
    /// authority, exact accepted inputs, and owned repository supervisor.
    pub(crate) async fn drive_standing_checks(&self) -> Result<bool> {
        let dispatch = {
            let mut state = self.lock()?;
            state.ready()?;
            let snapshot = state.canonical.snapshot(&state.turn_id).map_err(error)?;
            let contract = snapshot.contract();
            if contract.state() != Some(LogicalTurnState::Running)
                || contract.stop_requested().is_some()
                || contract.has_unknown_effects()
                || contract
                    .activations()
                    .iter()
                    .any(|item| item.state == ActivationState::Running)
                || state.execution_admission_closed
            {
                return Ok(false);
            }
            let Some(work) = state.standing_work()? else {
                return Ok(false);
            };
            let definitions = standing_check_definitions(&work.required_checks).map_err(error)?;
            if definitions.is_empty() {
                return Ok(false);
            }
            let Some(activations) =
                standing_check_activations(contract, &work.receipt_id, definitions.len())?
            else {
                return Ok(false);
            };
            let accepted = contract.current_accepted_activations();
            let mut repository = None;
            for activation in &activations {
                let item = accepted
                    .iter()
                    .find(|item| item.activation == *activation)
                    .ok_or_else(|| error("Standing check accepted input disappeared"))?;
                let RepositoryInput::Recorded {
                    snapshot: reference,
                } = &item.input.repository
                else {
                    return Ok(false);
                };
                if repository
                    .as_ref()
                    .is_some_and(|previous| previous != reference)
                {
                    return Err(error(
                        "Standing checks require one exact candidate repository",
                    ));
                }
                repository = Some(reference.clone());
            }
            let Some(repository) = repository else {
                return Ok(false);
            };
            let epoch = contract
                .epochs()
                .last()
                .ok_or_else(|| error("Standing work has no epoch"))?
                .id
                .clone();
            let owner = state
                .repository_owners
                .get(&repository)
                .ok_or_else(|| error("Standing checks have no retained repository owner"))?
                .clone();
            repository::validate_retained_repository(&state, &owner, &repository)?;
            let mut selected = None;
            let mut results = Vec::new();
            let mut observations = Vec::new();
            let mut command_candidates = Vec::new();
            for (index, definition) in definitions.iter().enumerate() {
                let condition_id = ConditionId::new(standing_condition_id(&work.receipt_id, index))
                    .map_err(error)?;
                let definition_ref = state
                    .content
                    .retained_check_definition(definition)
                    .map_err(error)?
                    .ok_or_else(|| error("Standing check definition is unavailable"))?;
                if let Some(observation) = contract.current_condition(&condition_id) {
                    let recorded = contract.condition_runs().iter().rev().find(|item| item.run.condition_id == condition_id && item.run.activations == activations && matches!(&item.resolution, Some(ConditionEffectResolution::OutcomeRecorded {evidence}) if evidence == &observation.evidence));
                    let Some(recorded) = recorded else {
                        return Ok(false);
                    };
                    let arguments = state
                        .content
                        .condition_arguments(&snapshot, &recorded.run.run_id)
                        .map_err(error)?
                        .ok_or_else(|| error("Standing check arguments are unavailable"))?;
                    if arguments.definition() != definition {
                        return Err(error("Standing check differs from the armed definition"));
                    }
                    let result = state
                        .content
                        .condition_result(&arguments)
                        .map_err(error)?
                        .ok_or_else(|| error("Standing check result is unavailable"))?;
                    if index > 0 && index + 1 < definitions.len() {
                        command_candidates.push(
                            state
                                .content
                                .check_candidate(
                                    &snapshot,
                                    &recorded.run,
                                    &standing_condition_id(&work.receipt_id, 0),
                                    &standing_condition_id(
                                        &work.receipt_id,
                                        work.required_checks.len() + 1,
                                    ),
                                )
                                .map_err(error)?,
                        );
                    }
                    results.push((
                        condition_id.clone(),
                        observation.evidence.clone(),
                        observation.outcome,
                    ));
                    if index == 0 || index == definitions.len() - 1 {
                        let phase = if index == 0 {
                            RepositorySnapshotPhase::BeforeCheck { index: 0 }
                        } else {
                            RepositorySnapshotPhase::AfterCheck { index: 0 }
                        };
                        let previous = state
                            .content
                            .repository_snapshots(&snapshot, &activations[0])
                            .map_err(error)?
                            .into_iter()
                            .find(|item| {
                                item.content.phase == phase
                                    && item.content.condition_run.as_ref()
                                        == Some(&recorded.run.run_id)
                            });
                        let capture = match previous {
                            Some(previous) => previous,
                            None => {
                                let mut capture = repository_snapshot::empty_observation(
                                    &activations[0],
                                    phase,
                                    &repository,
                                );
                                capture.condition_run = Some(recorded.run.run_id.clone());
                                capture.outcome = Some(result.reference().clone());
                                let bytes = result.stdout().retained_bytes().map_err(error)?;
                                let value = serde_json::json!({"exit_code":match result.status() {ConditionProcessStatus::Exited {code} => Some(*code), _ => None}, "stdout":String::from_utf8_lossy(&bytes), "stdout_truncated":result.stdout().is_truncated()});
                                if let Err(failure) =
                                    repository_snapshot::parse_capture(&value, &mut capture)
                                {
                                    capture.tree_sha256 = None;
                                    capture.unavailable = Some(failure.to_string());
                                }
                                let retained = state
                                    .content
                                    .retain_repository_snapshot(&snapshot, capture)
                                    .map_err(error);
                                state.fail_closed(retained)?;
                                return Ok(true);
                            }
                        };
                        if index == 0 && capture.content.tree_sha256.is_none() {
                            return Ok(false);
                        }
                        observations.push(capture);
                    }
                    continue;
                }
                if contract
                    .epochs()
                    .last()
                    .and_then(|epoch| epoch.continuation.as_ref())
                    .is_some_and(|plan| !plan.condition_runs.contains(&condition_id))
                {
                    continue;
                }
                let run_id = ConditionRunId::new(format!(
                    "standing-{:x}",
                    Sha256::digest(
                        serde_json::to_vec(&(&work.receipt_id, &epoch, index)).map_err(error)?
                    )
                ))
                .map_err(error)?;
                // Existing intent means reconcile its real result, never replay.
                if contract.condition_run(&run_id).is_some() {
                    return Ok(false);
                }
                let Some(grant_id) = state
                    .authority
                    .team_work_condition_grant(&definition_ref, &repository, now_ms()?)
                    .map_err(error)?
                else {
                    return Ok(false);
                };
                let policy = state.authority.grant_policy(&grant_id).map_err(error)?;
                let reference = state
                    .content
                    .retain_activation_evidence(ActivationEvidenceContent::Grant {
                        policy: policy.clone(),
                    })
                    .map_err(error);
                let reference = state.fail_closed(reference)?.reference().clone();
                let grant = GrantSnapshotRef {
                    grant_id: GrantId::new(grant_id).map_err(error)?,
                    revision: policy.revision,
                    evidence: reference,
                };
                selected = Some((
                    owner.clone(),
                    ConditionRunRef {
                        session_id: snapshot.owner().session_id.clone(),
                        turn_id: snapshot.turn_id().clone(),
                        epoch_id: epoch.clone(),
                        condition_id,
                        run_id,
                        activations: activations.clone(),
                    },
                    repository.clone(),
                    grant,
                ));
                break;
            }
            if selected.is_none() {
                let condition_id = ConditionId::new(format!("standing:{}:ready", work.receipt_id))
                    .map_err(error)?;
                if contract
                    .epochs()
                    .last()
                    .and_then(|epoch| epoch.continuation.as_ref())
                    .is_some_and(|plan| !plan.condition_runs.contains(&condition_id))
                {
                    return Ok(false);
                }
                if observations.len() != 2 {
                    return Ok(false);
                }
                let before = &observations[0];
                let after = &observations[1];
                let candidate = after
                    .content
                    .tree_sha256
                    .clone()
                    .map(|tree| (tree, after.content.head.clone()));
                let commands_match = command_candidates.len() == work.required_checks.len()
                    && command_candidates
                        .iter()
                        .all(|observed| observed.is_some() && observed == &candidate);
                let passed = results.len() == definitions.len()
                    && commands_match
                    && results
                        .iter()
                        .all(|(_, _, outcome)| *outcome == ConditionOutcome::Passed)
                    && before.content.tree_sha256.is_some()
                    && before.content.tree_sha256 == after.content.tree_sha256
                    && before.content.head == after.content.head;
                let proof = serde_json::json!({"kind":"standing_candidate_readiness","receipt_id":work.receipt_id,"subject":work.subject,"binding":work.binding,"required_checks":work.required_checks,"activations":activations,"before":before.reference,"after":after.reference,"candidate_sha256":after.content.tree_sha256,"checks":results,"check_candidates":command_candidates,"passed":passed}).to_string();
                if let Some(existing) = contract.current_condition(&condition_id) {
                    if matches!(state.content.resolve_activation_evidence(&existing.evidence).map_err(error)?, ActivationEvidenceContent::Guidance {text} if text == &proof)
                    {
                        return Ok(false);
                    }
                }
                let command = format!("standing-ready-{:x}", Sha256::digest(proof.as_bytes()));
                let evidence = state
                    .content
                    .retain_activation_evidence(ActivationEvidenceContent::Guidance { text: proof })
                    .map_err(error);
                let evidence = state.fail_closed(evidence)?.reference().clone();
                let appended = state.append(
                    &command,
                    TurnContractEvent::RecordCondition {
                        epoch_id: epoch,
                        condition_id,
                        activations,
                        outcome: if passed {
                            ConditionOutcome::Passed
                        } else {
                            ConditionOutcome::Failed
                        },
                        evidence,
                    },
                );
                state.fail_closed(appended)?;
                return Ok(true);
            }
            selected
        };
        if let Some((owner, run, repository, grant)) = dispatch {
            self.start_repository_check(owner, run, repository, grant)
                .await?
                .finish()
                .await?;
            return Ok(true);
        }
        Ok(false)
    }
}

/// Required Agents must all settle before checking, but they do not redefine a
/// condition's inputs. The canonical graph preserves each approved check scope
/// through Add and replaces its exact member only through ReplaceFuture.
fn standing_check_activations(
    contract: &TurnContract,
    receipt_id: &str,
    definition_count: usize,
) -> Result<Option<Vec<ActivationRef>>> {
    let graph = contract
        .graph()
        .ok_or_else(|| error("Standing work has no admitted graph"))?;
    let accepted = contract.current_accepted_activations();
    if graph.nodes.iter().filter(|node| node.required).any(|node| {
        !accepted
            .iter()
            .any(|item| item.activation.node_id == node.node_id)
    }) {
        return Ok(None);
    }
    let first_id = standing_condition_id(receipt_id, 0);
    let first = graph
        .conditions
        .iter()
        .find(|condition| condition.condition_id.as_str() == first_id)
        .ok_or_else(|| error("Standing check scope is unavailable"))?;
    if first.nodes.is_empty() {
        return Err(error("Standing check scope is empty"));
    }
    for id in (0..definition_count)
        .map(|index| standing_condition_id(receipt_id, index))
        .chain(std::iter::once(format!("standing:{receipt_id}:ready")))
    {
        let condition = graph
            .conditions
            .iter()
            .find(|condition| condition.condition_id.as_str() == id)
            .ok_or_else(|| error("Standing check condition is unavailable"))?;
        if condition.nodes != first.nodes {
            return Err(error(
                "Standing checks no longer share their exact candidate scope",
            ));
        }
    }
    Ok(first
        .nodes
        .iter()
        .map(|node| {
            accepted
                .iter()
                .find(|item| item.activation.node_id == *node)
                .map(|item| item.activation.clone())
        })
        .collect::<Option<Vec<_>>>())
}

#[cfg(test)]
#[path = "session_dispatch_standing_checks_tests.rs"]
mod tests;
