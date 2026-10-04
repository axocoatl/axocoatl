//! Host-run checks after a turn's required Agents finish: a Session team's
//! required checks. Each command runs between two repository captures of the
//! exact accepted candidate, and one readiness review records whether all
//! passed.
use super::*;
use axocoatl_session::execution_content::{
    ActivationRepositorySnapshot, ConditionProcessStatus, RepositorySnapshotPhase,
};
use axocoatl_session::turn_checks::{admitted_check_definitions, group_of, CheckGroup};
use axocoatl_session::turn_review::review_node;

/// One run per check, epoch and turn; an existing run is reconciled, never
/// replayed.
fn check_run_id(
    turn: &LogicalTurnId,
    epoch: &ExecutionEpochId,
    index: usize,
) -> Result<ConditionRunId> {
    ConditionRunId::new(format!(
        "required-check-{:x}",
        Sha256::digest(serde_json::to_vec(&(turn, epoch, index)).map_err(error)?)
    ))
    .map_err(error)
}

impl DispatchState {
    /// The required checks this turn runs, if any, re-read from the admitted
    /// graph.
    pub(super) fn turn_required_checks(&self) -> Result<Option<Vec<Vec<String>>>> {
        let snapshot = self.canonical.snapshot(&self.turn_id).map_err(error)?;
        let Some(graph) = snapshot.contract().graph() else {
            return Ok(None);
        };
        required_checks(graph, &self.content)
    }

    /// Why the grant that pays for this turn's required checks cannot spend
    /// `needed` more invocations on them at `now_ms`, in words for the
    /// person, or `None` when it can. `lead` opens the sentence.
    pub(super) fn check_payment_shortfall(
        &self,
        needed: u32,
        now_ms: u64,
        lead: &str,
    ) -> Result<Option<String>> {
        const FINISH: &str = "use Finish partial result to finish without them";
        let Some(payer) = self.authority.required_check_payer().map_err(error)? else {
            return Ok(Some(format!(
                "{lead}: no Agent of this turn may pay for them. You can {FINISH}."
            )));
        };
        let snapshot = self.canonical.snapshot(&self.turn_id).map_err(error)?;
        let name = agent_name(&self.content, snapshot.contract().graph(), &payer.holder);
        let reason = if payer.closed {
            format!("{lead}: this turn's authority is closed.")
        } else if payer.revoked {
            format!("{lead}: {name}'s authority for this turn was revoked. You can {FINISH}.")
        } else if now_ms >= payer.expires_at_ms {
            format!(
                "{lead}: {name}'s budget expired. You can {FINISH}, and set a later budget \
                 expiry in Team and budget for later turns."
            )
        } else if payer.invocations_left < needed {
            let remedy = if payer.delegating {
                format!("Raise its invocation limit with Review current authority, or {FINISH}.")
            } else {
                format!(
                    "You can {FINISH}, and raise its invocation limit in Team and budget for \
                     later turns."
                )
            };
            format!(
                "{lead}: {name}'s budget has {} left; they need {}. {remedy}",
                invocations(payer.invocations_left),
                invocations(needed)
            )
        } else {
            return Ok(None);
        };
        Ok(Some(reason))
    }

    /// Record the readiness review of this turn's required checks from
    /// `proof`, unless the current review already records exactly it.
    /// Whether anything was recorded.
    fn record_check_readiness(
        &mut self,
        epoch: ExecutionEpochId,
        activations: Vec<ActivationRef>,
        proof: String,
        passed: bool,
    ) -> Result<bool> {
        let snapshot = self.canonical.snapshot(&self.turn_id).map_err(error)?;
        let condition_id = ConditionId::new(CheckGroup::required().ready_id()).map_err(error)?;
        if let Some(existing) = snapshot.contract().current_condition(&condition_id) {
            if matches!(self.content.resolve_activation_evidence(&existing.evidence).map_err(error)?, ActivationEvidenceContent::Guidance {text} if text == &proof)
            {
                return Ok(false);
            }
        }
        let command = format!(
            "required-check-ready-{:x}",
            Sha256::digest(proof.as_bytes())
        );
        let evidence = self
            .content
            .retain_activation_evidence(ActivationEvidenceContent::Guidance { text: proof })
            .map_err(error);
        let evidence = self.fail_closed(evidence)?.reference().clone();
        let appended = self.append(
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
        self.fail_closed(appended)?;
        Ok(true)
    }
}

/// Whether the latest epoch runs condition `id`: a first epoch runs every
/// condition, a continuation only those it selected.
pub(super) fn epoch_runs(contract: &TurnContract, id: &ConditionId) -> bool {
    contract
        .epochs()
        .last()
        .and_then(|epoch| epoch.continuation.as_ref())
        .is_none_or(|plan| plan.condition_runs.contains(id))
}

/// `count` invocations, in words.
fn invocations(count: u32) -> String {
    if count == 1 {
        "1 invocation".into()
    } else {
        format!("{count} invocations")
    }
}

/// The name the person gave the Agent of `node`, or its node id.
pub(super) fn agent_name(
    content: &ExecutionContentStore,
    graph: Option<&TurnGraphSnapshot>,
    node: &TurnNodeId,
) -> String {
    graph
        .and_then(|graph| graph.nodes.iter().find(|item| item.node_id == *node))
        .and_then(
            |item| match content.resolve_activation_evidence(&item.definition.snapshot) {
                Ok(ActivationEvidenceContent::Definition { configuration, .. }) => {
                    serde_json::from_str::<serde_json::Value>(configuration)
                        .ok()?
                        .get("name")?
                        .as_str()
                        .filter(|name| !name.is_empty())
                        .map(str::to_owned)
                }
                _ => None,
            },
        )
        .unwrap_or_else(|| node.as_str().to_owned())
}

/// The required checks an admitted graph carries, read from their retained
/// definitions. Fails closed unless those definitions are exactly what the
/// commands define.
pub(super) fn required_checks(
    graph: &TurnGraphSnapshot,
    content: &ExecutionContentStore,
) -> Result<Option<Vec<Vec<String>>>> {
    let Some((group, count)) = group_of(graph) else {
        return Ok(None);
    };
    if group != CheckGroup::required() {
        return Ok(None);
    }
    let mut definitions = Vec::new();
    for index in 0..count + 2 {
        let id = group.condition_id(index);
        let Some(ConditionKind::RepositoryCheck { definition }) = graph
            .conditions
            .iter()
            .find(|condition| condition.condition_id.as_str() == id)
            .map(|condition| &condition.kind)
        else {
            return Err(error("A required check condition is missing"));
        };
        definitions.push(
            content
                .resolve_repository_check_definition(definition)
                .map_err(error)?
                .clone(),
        );
    }
    let checks: Vec<_> = definitions[1..=count]
        .iter()
        .map(|definition| definition.argv.clone())
        .collect();
    if admitted_check_definitions(graph, content, &group, &checks).map_err(error)? != definitions {
        return Err(error(
            "Required checks differ from the commands this turn was admitted with",
        ));
    }
    Ok(Some(checks))
}

impl SessionDispatchController {
    /// Install every grant the turn was admitted with. Installing an
    /// identical grant again changes nothing.
    pub(crate) fn install_admission_grants(&self) -> Result<()> {
        let grants = {
            let state = self.lock()?;
            let (_, admission) = state
                .content
                .turn_admission(&state.canonical, &state.turn_id)
                .map_err(error)?
                .ok_or_else(|| error("This turn has no canonical admission"))?;
            admission
                .nodes
                .iter()
                .map(|node| {
                    match state
                        .content
                        .resolve_activation_evidence(&node.grant.evidence)
                        .map_err(error)?
                    {
                        ActivationEvidenceContent::Grant { policy } => Ok(policy.clone()),
                        _ => Err(error("An admitted grant is unavailable")),
                    }
                })
                .collect::<Result<Vec<_>>>()?
        };
        for grant in grants {
            self.install_grant(grant)?;
        }
        Ok(())
    }

    /// Before the first driver handoff, install the admitted grants and
    /// record which one pays for the turn's required checks. The authority
    /// refuses unless that grant is installed. A turn without required
    /// checks, or whose driver has already started, is unchanged.
    pub(crate) fn authorize_required_checks(&self, repository: &EvidenceRef) -> Result<()> {
        let admitted = {
            let state = self.lock()?;
            state.ready()?;
            let snapshot = state.canonical.snapshot(&state.turn_id).map_err(error)?;
            if snapshot.contract().state() != Some(LogicalTurnState::Running)
                || state.driver.is_some()
                || !snapshot
                    .contract()
                    .graph()
                    .and_then(group_of)
                    .is_some_and(|(group, _)| group == CheckGroup::required())
                || state
                    .content
                    .turn_driver_handed_off(&state.canonical, &state.turn_id)
                    .map_err(error)?
            {
                return Ok(());
            }
            state
                .content
                .turn_admission(&state.canonical, &state.turn_id)
                .map_err(error)?
                .is_some()
        };
        if admitted {
            self.install_admission_grants()?;
        }
        let mut state = self.lock()?;
        state.ready()?;
        let snapshot = state.canonical.snapshot(&state.turn_id).map_err(error)?;
        let owner = state
            .repository_owners
            .get(repository)
            .ok_or_else(|| error("Required checks have no actual repository owner"))?;
        repository::validate_retained_repository(&state, owner, repository)?;
        let backend = owner.backend().to_owned();
        let result = state
            .authority
            .authorize_required_checks(&snapshot, &state.content, repository, &backend)
            .map_err(|failure| {
                error(format!(
                    "The required checks of this turn cannot be authorized: {failure}"
                ))
            });
        state.fail_closed(result)
    }

    /// The one turn driver runs checks only after the complete required Agent
    /// frontier is accepted. Every command uses the independent check
    /// authority, exact accepted inputs, and owned repository supervisor.
    pub(crate) async fn drive_turn_checks(&self) -> Result<bool> {
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
            let Some(checks) = state.turn_required_checks()? else {
                return Ok(false);
            };
            let group = CheckGroup::required();
            let definitions = match contract.graph() {
                Some(graph) => admitted_check_definitions(graph, &state.content, &group, &checks),
                None => axocoatl_session::turn_checks::check_definitions(&checks),
            }
            .map_err(error)?;
            if definitions.is_empty() {
                return Ok(false);
            }
            let Some(activations) = check_activations(contract, &group, definitions.len())? else {
                return Ok(false);
            };
            let accepted = contract.current_accepted_activations();
            let mut repository = None;
            for activation in &activations {
                let item = accepted
                    .iter()
                    .find(|item| item.activation == *activation)
                    .ok_or_else(|| error("A checked accepted input disappeared"))?;
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
                    return Err(error("Checks require one exact candidate repository"));
                }
                repository = Some(reference.clone());
            }
            let Some(repository) = repository else {
                return Ok(false);
            };
            let epoch = contract
                .epochs()
                .last()
                .ok_or_else(|| error("Checked turn has no epoch"))?
                .id
                .clone();
            let owner = state
                .repository_owners
                .get(&repository)
                .ok_or_else(|| error("Checks have no retained repository owner"))?
                .clone();
            repository::validate_retained_repository(&state, &owner, &repository)?;
            let before_id = group.condition_id(0);
            let after_id = group.condition_id(checks.len() + 1);
            let mut selected = None;
            let mut results = Vec::new();
            let mut observations = Vec::new();
            let mut command_candidates = Vec::new();
            let mut before_run = None;
            for (index, definition) in definitions.iter().enumerate() {
                let condition_id = ConditionId::new(group.condition_id(index)).map_err(error)?;
                let definition_ref = state
                    .content
                    .retained_check_definition(definition)
                    .map_err(error)?
                    .ok_or_else(|| error("Check definition is unavailable"))?;
                if let Some(observation) = contract.current_condition(&condition_id) {
                    let recorded = contract.condition_runs().iter().rev().find(|item| item.run.condition_id == condition_id && item.run.activations == activations && matches!(&item.resolution, Some(ConditionEffectResolution::OutcomeRecorded {evidence}) if evidence == &observation.evidence));
                    let Some(recorded) = recorded else {
                        return Ok(false);
                    };
                    if index == 0 {
                        before_run = Some(recorded.run.run_id.clone());
                    }
                    let arguments = state
                        .content
                        .condition_arguments(&snapshot, &recorded.run.run_id)
                        .map_err(error)?
                        .ok_or_else(|| error("Check arguments are unavailable"))?;
                    if arguments.definition() != definition {
                        return Err(error("A check differs from its admitted definition"));
                    }
                    let result = state
                        .content
                        .condition_result(&arguments)
                        .map_err(error)?
                        .ok_or_else(|| error("Check result is unavailable"))?;
                    if index > 0 && index + 1 < definitions.len() {
                        command_candidates.push(
                            state
                                .content
                                .check_candidate(&snapshot, &recorded.run, &before_id, &after_id)
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
                                if let Err(failure) = repository_snapshot::parse_capture(
                                    &value,
                                    &mut capture,
                                    &repository_snapshot::CaptureMode::Observe,
                                ) {
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
                        // No command runs on a tree the host could not see.
                        if index == 0 && capture.content.tree_sha256.is_none() {
                            let ready = ConditionId::new(group.ready_id()).map_err(error)?;
                            if !epoch_runs(contract, &ready) {
                                return Ok(false);
                            }
                            let proof = serde_json::json!({"kind":"required_check_readiness","turn_id":snapshot.turn_id(),"required_checks":checks,"activations":activations,"before":capture.reference,"passed":false,"reason":NOT_CAPTURED}).to_string();
                            return state.record_check_readiness(epoch, activations, proof, false);
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
                let run_id = check_run_id(snapshot.turn_id(), &epoch, index)?;
                // Existing intent means reconcile its real result, never replay.
                if contract.condition_run(&run_id).is_some() {
                    return Ok(false);
                }
                // Nothing of a pass is spent unless the paying grant can pay
                // for the rest of it; otherwise the turn records why.
                let needed = (index..definitions.len())
                    .filter(|later| {
                        ConditionId::new(group.condition_id(*later)).is_ok_and(|id| {
                            contract.current_condition(&id).is_none() && epoch_runs(contract, &id)
                        })
                    })
                    .count();
                let now = now_ms()?;
                let unpaid = state.check_payment_shortfall(
                    u32::try_from(needed).unwrap_or(u32::MAX),
                    now,
                    "Required checks could not run",
                )?;
                let grant_id = match unpaid {
                    Some(_) => None,
                    None => state
                        .authority
                        .required_check_grant(&definition_ref, &repository, now)
                        .map_err(error)?,
                };
                let Some(grant_id) = grant_id else {
                    let ready = ConditionId::new(group.ready_id()).map_err(error)?;
                    if !epoch_runs(contract, &ready) {
                        return Ok(false);
                    }
                    let reason = unpaid.unwrap_or_else(|| {
                        "Required checks could not run: no Agent of this turn may pay for them \
                         now. You can use Finish partial result to finish without them."
                            .into()
                    });
                    // The epoch keeps one epoch's review distinct from an
                    // identical one in a later epoch.
                    let proof = serde_json::json!({"kind":"required_check_readiness","turn_id":snapshot.turn_id(),"epoch_id":epoch,"required_checks":checks,"activations":activations,"passed":false,"reason":reason}).to_string();
                    return state.record_check_readiness(epoch, activations, proof, false);
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
                let condition_id = ConditionId::new(group.ready_id()).map_err(error)?;
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
                let outcomes: Vec<_> = results.iter().map(|(_, _, outcome)| *outcome).collect();
                // Work accepted after the checks began is not what they ran on.
                let current: Vec<_> = contract
                    .current_accepted_activations()
                    .iter()
                    .map(|item| item.activation.clone())
                    .collect();
                let reviewer = contract
                    .graph()
                    .and_then(review_node)
                    .map(|node| &node.node_id);
                let failure = readiness_failure(
                    definitions.len(),
                    &outcomes,
                    &command_candidates,
                    checks.len(),
                    &before.content,
                    &after.content,
                )
                .or_else(|| {
                    before_run
                        .as_ref()
                        .zip(state.canonical.turn_records(snapshot.turn_id()).ok())
                        .is_some_and(|(before_run, records)| {
                            accepted_after_capture(
                                &records,
                                snapshot.turn_id(),
                                before_run,
                                &current,
                                reviewer,
                            )
                        })
                        .then_some(ACCEPTED_AFTER_CAPTURE)
                });
                let passed = failure.is_none();
                let mut proof = serde_json::json!({"kind":"required_check_readiness","turn_id":snapshot.turn_id(),"required_checks":checks,"activations":activations,"before":before.reference,"after":after.reference,"candidate_sha256":after.content.tree_sha256,"checks":results,"check_candidates":command_candidates,"passed":passed});
                if let Some(reason) = failure {
                    proof["reason"] = reason.into();
                }
                return state.record_check_readiness(epoch, activations, proof.to_string(), passed);
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

/// Why the checks are not ready, in words for the person.
const NOT_CAPTURED: &str = "The repository could not be captured around the checks, so they \
     establish nothing. Continue runs them again.";
const NOT_ALL_RUN: &str = "Some checks have no result on the current tree. Continue runs them \
     all again.";
const CHECK_FAILED: &str = "A check failed. Fix the cause, then Continue to run the checks again.";
const CHANGED_FILES: &str = "A check changed files, so the repository after the checks is not the \
     one they ran on. Continue runs them again on the changed tree.";
const OLDER_TREE: &str = "Some checks ran on an older tree than the current one. Continue runs \
     them all again.";
const ACCEPTED_AFTER_CAPTURE: &str = "An Agent finished after the checks captured the repository, \
     so they did not run on its result. Continue runs them again.";

/// Why recorded results do not establish readiness, or `None` when they do:
/// every capture and command passed, each command ran on the candidate its
/// group captured, and the captured tree and HEAD did not change while the
/// commands ran.
fn readiness_failure(
    definitions: usize,
    outcomes: &[ConditionOutcome],
    command_candidates: &[Option<(String, Option<String>)>],
    checks: usize,
    before: &ActivationRepositorySnapshot,
    after: &ActivationRepositorySnapshot,
) -> Option<&'static str> {
    use ConditionOutcome::Passed;
    if outcomes.len() != definitions || definitions != checks + 2 {
        return Some(NOT_ALL_RUN);
    }
    if before.tree_sha256.is_none()
        || after.tree_sha256.is_none()
        || outcomes[0] != Passed
        || outcomes[checks + 1] != Passed
    {
        return Some(NOT_CAPTURED);
    }
    if outcomes[1..=checks]
        .iter()
        .any(|outcome| *outcome != Passed)
    {
        return Some(CHECK_FAILED);
    }
    if before.tree_sha256 != after.tree_sha256 || before.head != after.head {
        return Some(CHANGED_FILES);
    }
    let candidate = after
        .tree_sha256
        .clone()
        .map(|tree| (tree, after.head.clone()));
    let commands_match = command_candidates.len() == checks
        && command_candidates
            .iter()
            .all(|observed| observed.is_some() && observed == &candidate);
    (!commands_match).then_some(OLDER_TREE)
}

/// Whether recorded results establish readiness (see [`readiness_failure`]).
#[cfg(test)]
fn readiness_passed(
    definitions: usize,
    outcomes: &[ConditionOutcome],
    command_candidates: &[Option<(String, Option<String>)>],
    checks: usize,
    before: &ActivationRepositorySnapshot,
    after: &ActivationRepositorySnapshot,
) -> bool {
    readiness_failure(
        definitions,
        outcomes,
        command_candidates,
        checks,
        before,
        after,
    )
    .is_none()
}

/// Whether one of the `current` accepted activations was accepted after the
/// intent of the checks' Before capture `before` in this turn's `records`:
/// its changes may be missing from what the checks ran on. The turn's
/// required `reviewer` never counts: it is read-only, the host starts it only
/// once the checks pass, and its verdict binds only when its own captures saw
/// the tree the checks passed on, so accepting it changes nothing they ran on.
fn accepted_after_capture(
    records: &[TurnContractEnvelope],
    turn: &LogicalTurnId,
    before: &ConditionRunId,
    current: &[ActivationRef],
    reviewer: Option<&TurnNodeId>,
) -> bool {
    let mut captured = false;
    for record in records.iter().filter(|record| record.turn_id == *turn) {
        match &record.event {
            TurnContractEvent::RecordConditionIntent { run, .. } if run.run_id == *before => {
                captured = true;
            }
            TurnContractEvent::AcceptActivation { activation, .. }
                if captured
                    && current.contains(activation)
                    && reviewer != Some(&activation.node_id) =>
            {
                return true;
            }
            _ => {}
        }
    }
    false
}

/// Required Agents must all settle before checking, but they do not redefine a
/// condition's inputs. The canonical graph preserves each approved check scope
/// through Add and replaces its exact member only through ReplaceFuture.
pub(super) fn check_activations(
    contract: &TurnContract,
    group: &CheckGroup,
    definition_count: usize,
) -> Result<Option<Vec<ActivationRef>>> {
    let graph = contract
        .graph()
        .ok_or_else(|| error("Checked turn has no admitted graph"))?;
    let accepted = contract.current_accepted_activations();
    if graph.nodes.iter().filter(|node| node.required).any(|node| {
        !accepted
            .iter()
            .any(|item| item.activation.node_id == node.node_id)
    }) {
        return Ok(None);
    }
    let first_id = group.condition_id(0);
    let first = graph
        .conditions
        .iter()
        .find(|condition| condition.condition_id.as_str() == first_id)
        .ok_or_else(|| error("Check scope is unavailable"))?;
    if first.nodes.is_empty() {
        return Err(error("Check scope is empty"));
    }
    for id in (0..definition_count)
        .map(|index| group.condition_id(index))
        .chain(std::iter::once(group.ready_id()))
    {
        let condition = graph
            .conditions
            .iter()
            .find(|condition| condition.condition_id.as_str() == id)
            .ok_or_else(|| error("Check condition is unavailable"))?;
        if condition.nodes != first.nodes {
            return Err(error("Checks no longer share their exact candidate scope"));
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
#[path = "session_dispatch_turn_checks_tests.rs"]
mod tests;
