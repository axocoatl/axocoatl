//! Host-enforced review. When a turn's graph carries a required reviewer, the
//! one turn driver starts it after the required Agents finish and the required
//! checks pass, in a fresh conversation whose input is the request, each
//! required Agent's final answer and the change the turn made. Its verdict
//! becomes the review condition. Only an approval of the exact result it was
//! shown passes it; a request for changes sends the findings back to the lead
//! in a new epoch, as a person's Revise would, until the approved rounds run
//! out and the person decides.
use super::*;
use axocoatl_session::execution_content::{ActivationRepositorySnapshot, RepositorySnapshotPhase};
use axocoatl_session::turn_checks::{group_of, CheckGroup};
use axocoatl_session::turn_review::{
    binding_failure, bounded, describe_change, parse_verdict, review_condition_id,
    review_criterion, review_node, ReviewCriterion, ReviewProof, ReviewVerdict, MAX_ANSWER_BYTES,
};
use base64::Engine;

/// What the reviewer is shown: the exact result it judges.
struct ReviewCandidate {
    /// The repository tree the Agents left, when it was captured.
    tree: Option<String>,
    prompt: String,
}

/// One review round to start: what the reviewer is shown and where.
struct ReviewRound {
    repository: RepositoryInput,
    /// The retained prompt naming the exact result.
    prompt: EvidenceRef,
    /// The reviewer generation that runs it.
    round: u32,
    /// The host's note asking again for a verdict the previous round's
    /// answer did not carry, shown beside that answer; `None` for a round
    /// about a new result.
    reask: Option<EvidenceRef>,
}

/// A digest naming one host step of this turn's review.
fn step_digest(value: &impl serde::Serialize) -> Result<String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(value).map_err(error)?)
    ))
}

const UNREADABLE: &str = "The reviewer's answer had no single VERDICT: APPROVE or VERDICT: \
     CHANGES line, so it approves nothing. Continue runs the review again.";
const UNREADABLE_AGAIN: &str = "The reviewer's answer had no single VERDICT: APPROVE or VERDICT: \
     CHANGES line, even after the host asked it again, so it approves nothing. Continue runs \
     the review again.";
/// What the host tells a reviewer whose answer carried no readable verdict
/// when it asks again, in the next round, about the same result.
const REASK: &str = "Your previous answer did not contain a verdict line. Reply with exactly \
     `VERDICT: APPROVE` or `VERDICT: CHANGES` on its own line, followed by findings.";
const NOT_CAPTURED: &str = "The repository could not be captured after the Agents finished, so \
     the reviewer would not see the exact result. Continue runs the review again.";

impl DispatchState {
    /// Whether `node` is this turn's required reviewer.
    pub(super) fn is_review_node(&self, node: &TurnNodeId) -> Result<bool> {
        let snapshot = self.canonical.snapshot(&self.turn_id).map_err(error)?;
        Ok(snapshot
            .contract()
            .graph()
            .and_then(review_node)
            .is_some_and(|reviewer| reviewer.node_id == *node))
    }

    /// Whether `node` runs its template in a conversation of its own: a
    /// delegated helper or the required reviewer.
    pub(super) fn owns_instance_conversation(&self, node: &TurnNodeId) -> Result<bool> {
        Ok(self.native_child_origin(node)?.is_some() || self.is_review_node(node)?)
    }

    /// The final answer an accepted activation recorded.
    fn accepted_answer(
        &self,
        snapshot: &DurableTurnSnapshot,
        activation: &ActivationRef,
    ) -> Result<String> {
        let reservation = self
            .content
            .activation_output_reservation(snapshot, activation)
            .map_err(error)?
            .ok_or_else(|| error("an accepted answer has no output reservation"))?;
        let output = self
            .content
            .activation_output_settlement(&reservation)
            .map_err(error)?
            .ok_or_else(|| error("an accepted answer is missing"))?;
        Ok(output.content().output.text.clone())
    }

    /// The capture `phase` of `item`: `None` when its profile cannot capture
    /// the repository, `Some(None)` when it could but no capture recorded a
    /// tree.
    fn review_capture(
        &self,
        snapshot: &DurableTurnSnapshot,
        item: &ContractActivation,
        phase: RepositorySnapshotPhase,
    ) -> Result<Option<Option<ActivationRepositorySnapshot>>> {
        let ActivationEvidenceContent::Definition { profile, .. } = &self
            .content
            .resolve_activation_evidence(&item.input.definition.snapshot)
            .map_err(error)?
        else {
            return Err(error("a reviewed Agent's definition is missing"));
        };
        if !matches!(item.input.repository, RepositoryInput::Recorded { .. })
            || super::repository_snapshot::capture_tool(profile).is_none()
        {
            return Ok(None);
        }
        Ok(Some(
            self.content
                .repository_snapshots(snapshot, &item.activation)
                .map_err(error)?
                .into_iter()
                .rev()
                .map(|capture| capture.content)
                .find(|capture| capture.phase == phase)
                .filter(|capture| capture.unavailable.is_none() && capture.tree_sha256.is_some()),
        ))
    }

    /// The result `activations` left, as the reviewer is shown it, or why
    /// it cannot be shown exactly.
    fn review_candidate(
        &self,
        snapshot: &DurableTurnSnapshot,
        activations: &[&ContractActivation],
    ) -> Result<std::result::Result<ReviewCandidate, &'static str>> {
        let contract = snapshot.contract();
        let graph = contract
            .graph()
            .ok_or_else(|| error("a reviewed turn has no graph"))?;
        // The tree the checks passed on, or else the one the last Agent to
        // finish left.
        let mut after = None;
        let mut capturable = false;
        if let Some((group, _)) =
            group_of(graph).filter(|(group, _)| *group == CheckGroup::required())
        {
            let id = ConditionId::new(group.ready_id()).map_err(error)?;
            let observation = contract
                .current_condition(&id)
                .ok_or_else(|| error("the required checks have no current readiness"))?;
            let ActivationEvidenceContent::Guidance { text } = &self
                .content
                .resolve_activation_evidence(&observation.evidence)
                .map_err(error)?
            else {
                return Err(error("the readiness proof is not retained guidance"));
            };
            let proof: serde_json::Value = serde_json::from_str(text).map_err(error)?;
            let reference = proof
                .get("after")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| error("the readiness proof names no capture"))?;
            capturable = true;
            for item in activations {
                after = after.or(self
                    .content
                    .repository_snapshots(snapshot, &item.activation)
                    .map_err(error)?
                    .into_iter()
                    .find(|capture| capture.reference.as_str() == reference)
                    .map(|capture| capture.content)
                    .filter(|capture| capture.tree_sha256.is_some()));
            }
        } else {
            let records = self.canonical.turn_records(&self.turn_id).map_err(error)?;
            let last = records.iter().rev().find_map(|record| match &record.event {
                TurnContractEvent::AcceptActivation { activation, .. }
                    if record.turn_id == self.turn_id =>
                {
                    activations
                        .iter()
                        .find(|item| item.activation == *activation)
                        .copied()
                }
                _ => None,
            });
            if let Some(last) = last {
                if let Some(capture) =
                    self.review_capture(snapshot, last, RepositorySnapshotPhase::After)?
                {
                    capturable = true;
                    after = capture;
                }
            }
        }
        if capturable && after.is_none() {
            return Ok(Err(NOT_CAPTURED));
        }
        // What the tree held when the turn's first Agent started.
        let mut before = None;
        let scope: Vec<_> = activations
            .iter()
            .map(|item| &item.activation.node_id)
            .collect();
        if let Some(first) = contract
            .activations()
            .iter()
            .find(|item| scope.contains(&&item.activation.node_id))
        {
            before = self
                .review_capture(snapshot, first, RepositorySnapshotPhase::Before)?
                .flatten();
        }
        let patch = |capture: &ActivationRepositorySnapshot| {
            capture
                .patch_base64
                .as_deref()
                .and_then(|encoded| {
                    base64::engine::general_purpose::STANDARD
                        .decode(encoded)
                        .ok()
                })
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        };
        let change = match &after {
            None => "No repository capture was taken: the reviewed Agents cannot run commands \
                     or change files, so there is no change to show."
                .to_owned(),
            Some(after) => {
                let Some(after_patch) = patch(after) else {
                    return Ok(Err(NOT_CAPTURED));
                };
                let moved = before
                    .as_ref()
                    .is_some_and(|before| before.head != after.head);
                let earlier = before.as_ref().filter(|_| !moved).and_then(patch);
                let mut change =
                    describe_change(earlier.as_deref(), &after_patch, after.patch_complete);
                if moved {
                    change = format!(
                        "HEAD moved during the turn, so this is every uncommitted change against \
                         the new HEAD, including any made before the turn.\n{change}"
                    );
                }
                change
            }
        };
        let tree = after.as_ref().and_then(|after| after.tree_sha256.clone());
        let mut prompt = format!(
            "You are the required reviewer of this turn. Its request is above. Review the \
             result below against that request: the final answer of each Agent that did the \
             work and the change the turn made to the repository. Check the change against \
             every contract and edge case the project's instructions, docs and tests \
             describe, one by one. You cannot change files; you may read them.\n\nAnswer in \
             this form. The first line is exactly one of:\n\
             VERDICT: APPROVE\nVERDICT: CHANGES\nThen list each finding on its own line as \
             path:line: what is wrong and what to change. Approve only when nothing must \
             change.\n\nRepository tree reviewed: {}\n",
            tree.as_deref().unwrap_or("not captured")
        );
        for item in activations {
            let name = super::turn_checks::agent_name(
                &self.content,
                Some(graph),
                &item.activation.node_id,
            );
            let answer = self.accepted_answer(snapshot, &item.activation)?;
            prompt.push_str(&format!(
                "\n## Final answer of {name}\n{}\n",
                bounded(&answer, MAX_ANSWER_BYTES)
            ));
        }
        prompt.push_str(&format!("\n## Change\n{change}"));
        Ok(Ok(ReviewCandidate { tree, prompt }))
    }

    /// Record the review condition over `activations` from `proof`. The
    /// caller records at most one verdict per epoch: none once the condition
    /// has a current observation.
    fn record_review(
        &mut self,
        epoch: &ExecutionEpochId,
        activations: Vec<ActivationRef>,
        proof: serde_json::Value,
        passed: bool,
    ) -> Result<()> {
        let text = proof.to_string();
        let evidence = self
            .content
            .retain_activation_evidence(ActivationEvidenceContent::Guidance { text })
            .map_err(error);
        let evidence = self.fail_closed(evidence)?.reference().clone();
        let command = format!(
            "host-review-verdict-{}",
            &step_digest(&(epoch, &evidence))?[..48]
        );
        let appended = self.append(
            &command,
            TurnContractEvent::RecordCondition {
                epoch_id: epoch.clone(),
                condition_id: review_condition_id(),
                activations,
                outcome: if passed {
                    ConditionOutcome::Passed
                } else {
                    ConditionOutcome::Failed
                },
                evidence,
            },
        );
        self.fail_closed(appended)
    }

    /// The continuation that sends `findings` of review round `round` to the
    /// turn's lead: a Revise of its accepted result in a new epoch that runs
    /// every condition again, as a person's Revise of a paused turn does. The
    /// reason there is none: more than one Agent's result was reviewed, or
    /// the lead cannot take another instruction or run again.
    fn review_revision(
        &mut self,
        snapshot: &DurableTurnSnapshot,
        criterion: &ReviewCriterion,
        round: u32,
        findings: &str,
        activations: &[ActivationRef],
    ) -> Result<std::result::Result<ContinuationPlan, String>> {
        let contract = snapshot.contract();
        let graph = contract
            .graph()
            .ok_or_else(|| error("a reviewed turn has no graph"))?;
        let required = |node: &TurnNodeId| {
            graph
                .nodes
                .iter()
                .any(|item| item.node_id == *node && item.required)
        };
        let leads: Vec<_> = graph
            .nodes
            .iter()
            .filter(|node| {
                node.required
                    && !graph
                        .dependencies
                        .iter()
                        .any(|edge| edge.parent == node.node_id && required(&edge.child))
            })
            .collect();
        let [lead] = leads.as_slice() else {
            return Ok(Err(
                "More than one Agent's result was reviewed, so the host does not choose which \
                 one revises. Revise one of them with the findings, change the files yourself \
                 and Continue the review, or Finish."
                    .into(),
            ));
        };
        let previous = contract
            .activations()
            .iter()
            .rev()
            .find(|item| item.activation.node_id == lead.node_id)
            .filter(|item| item.state == ActivationState::Accepted && item.output.is_some())
            .ok_or_else(|| error("the reviewed lead has no accepted result"))?;
        if previous
            .input
            .guidance
            .len()
            .saturating_add(previous.input.attachments.len())
            .saturating_add(previous.input.parents.len())
            >= MAX_INPUT_REFERENCES
        {
            return Ok(Err(
                "The lead's input has no room for another instruction, so the host cannot send \
                 it the findings. Finish, or start a new turn with them."
                    .into(),
            ));
        }
        let instruction = format!(
            "The required reviewer ({}) asked for changes in review round {round} of {}:\n\n\
             {findings}\n\nAddress each finding, then give your final answer again. The host \
             runs the required checks and the review again on your new result.",
            criterion.template_id, criterion.max_rounds
        );
        let instruction = self
            .content
            .retain_activation_evidence(ActivationEvidenceContent::Guidance { text: instruction })
            .map_err(error);
        let instruction = self.fail_closed(instruction)?.reference().clone();
        let source = contract
            .epochs()
            .last()
            .ok_or_else(|| error("a reviewed turn has no epoch"))?
            .id
            .clone();
        let digest = step_digest(&(
            "host-review-revision-v1",
            snapshot.journal_id(),
            &source,
            round,
        ))?;
        let epoch =
            ExecutionEpochId::new(format!("epoch-review-{}", &digest[..48])).map_err(error)?;
        let mut input = previous.input.clone();
        input.manifest_id =
            InputManifestId::new(format!("input-review-{}", &digest[..48])).map_err(error)?;
        input.activation.activation_id =
            ActivationId::new(format!("activation-review-{}", &digest[..48])).map_err(error)?;
        input.activation.execution_epoch_id = epoch.clone();
        input.activation.generation = previous
            .activation
            .generation
            .checked_add(1)
            .ok_or_else(|| error("activation generation overflow"))?;
        input.guidance.push(instruction.clone());
        input.revision_context = Some(RevisionContext {
            activation: previous.activation.clone(),
            output: previous
                .output
                .clone()
                .ok_or_else(|| error("the reviewed lead has no output"))?,
        });
        if let Err(refused) = self.validate_control_input(snapshot, &input) {
            return Ok(Err(format!(
                "The lead cannot run again, so the host cannot send it the findings: {}. \
                 Revise it yourself after fixing that, or Finish.",
                refused
                    .to_string()
                    .strip_prefix("Session dispatch: ")
                    .unwrap_or(&refused.to_string())
            )));
        }
        let invalidate = self.human_revision_invalidation(&previous.activation)?;
        let selections = super::host_controls::revision_selections(
            contract,
            &previous.activation,
            &input,
            &invalidate,
            &instruction,
            &instruction,
        )?;
        let plan = ContinuationPlan {
            source_epoch_id: source.clone(),
            epoch_id: epoch,
            selections,
            condition_runs: graph
                .conditions
                .iter()
                .map(|condition| condition.condition_id.clone())
                .collect(),
        };
        // The verdict, the pause and the continuation must all apply; a
        // continuation the contract would refuse leaves the turn needing
        // attention with the reason instead.
        let mut preview = contract.clone();
        for (operation, event) in [
            (
                "verdict",
                TurnContractEvent::RecordCondition {
                    epoch_id: source.clone(),
                    condition_id: review_condition_id(),
                    activations: activations.to_vec(),
                    outcome: ConditionOutcome::Failed,
                    evidence: instruction.clone(),
                },
            ),
            ("pause", TurnContractEvent::PauseEpoch { epoch_id: source }),
            (
                "continue",
                TurnContractEvent::Continue { plan: plan.clone() },
            ),
        ] {
            let envelope = TurnContractEnvelope {
                schema_version: TURN_CONTRACT_SCHEMA_VERSION,
                command_id: CommandId::new(format!("preview-review-{operation}")).map_err(error)?,
                expected_revision: preview.revision(),
                session_id: snapshot.owner().session_id.clone(),
                turn_id: snapshot.turn_id().clone(),
                event,
            };
            if let Err(refused) = preview.apply(&envelope) {
                return Ok(Err(format!(
                    "The host could not send the findings to the lead: {refused}. Revise it \
                     yourself with them, or Finish."
                )));
            }
        }
        Ok(Ok(plan))
    }

    /// Continue in a new epoch that prepares the reviewer's first activation,
    /// `input`: a continuation left the never-started reviewer blocked in the
    /// current one, and only a new epoch can start it. Everything else keeps
    /// its accepted result or stays blocked by `blocker`; only the review runs.
    /// The reason when the contract would refuse it.
    fn unblock_review(
        &mut self,
        snapshot: &DurableTurnSnapshot,
        input: ActivationInputManifest,
        blocker: &EvidenceRef,
    ) -> Result<std::result::Result<(), String>> {
        let contract = snapshot.contract();
        let graph = contract
            .graph()
            .ok_or_else(|| error("a reviewed turn has no graph"))?;
        let source = contract
            .epochs()
            .last()
            .ok_or_else(|| error("a reviewed turn has no epoch"))?
            .id
            .clone();
        let accepted = contract.current_accepted_activations();
        let mut selections = Vec::with_capacity(graph.nodes.len());
        for node in &graph.nodes {
            if node.node_id == input.activation.node_id {
                selections.push(ContinuationSelection::PrepareUnmaterialized {
                    input: Box::new(input.clone()),
                });
                continue;
            }
            selections.push(
                match contract
                    .activations()
                    .iter()
                    .rev()
                    .find(|item| item.activation.node_id == node.node_id)
                {
                    Some(item)
                        if accepted
                            .iter()
                            .any(|current| current.activation == item.activation) =>
                    {
                        ContinuationSelection::RetainAccepted {
                            activation: item.activation.clone(),
                        }
                    }
                    Some(item) => ContinuationSelection::LeaveBlocked {
                        activation: item.activation.clone(),
                        blocker: blocker.clone(),
                    },
                    None => ContinuationSelection::LeaveUnmaterializedBlocked {
                        node_id: node.node_id.clone(),
                        blocker: blocker.clone(),
                    },
                },
            );
        }
        let plan = ContinuationPlan {
            source_epoch_id: source.clone(),
            epoch_id: input.activation.execution_epoch_id.clone(),
            selections,
            condition_runs: vec![review_condition_id()],
        };
        let digest = step_digest(&("host-review-unblock-v1", &plan.epoch_id))?;
        let events = [
            (
                format!("host-review-pause-{}", &digest[..48]),
                TurnContractEvent::PauseEpoch { epoch_id: source },
            ),
            (
                format!("host-review-continue-{}", &digest[..48]),
                TurnContractEvent::Continue { plan },
            ),
        ];
        let mut preview = contract.clone();
        for (command, event) in &events {
            let envelope = TurnContractEnvelope {
                schema_version: TURN_CONTRACT_SCHEMA_VERSION,
                command_id: CommandId::new(command.as_str()).map_err(error)?,
                expected_revision: preview.revision(),
                session_id: snapshot.owner().session_id.clone(),
                turn_id: snapshot.turn_id().clone(),
                event: event.clone(),
            };
            if let Err(refused) = preview.apply(&envelope) {
                return Ok(Err(format!(
                    "The reviewer was left blocked and the host could not start it: {refused}. \
                     Select the review and Continue, or Finish."
                )));
            }
        }
        for (command, event) in events {
            let appended = self.append(&command, event);
            self.fail_closed(appended)?;
        }
        self.changed.notify_waiters();
        Ok(Ok(()))
    }

    /// Start review round `round` in `epoch`: the reviewer's first
    /// activation, or a revision of its previous accepted one with the new
    /// result, or with the same result and its previous answer when the host
    /// asks again for a verdict. The driver runs it like any other admitted
    /// work. The reason when it cannot start.
    fn start_review_round(
        &mut self,
        snapshot: &DurableTurnSnapshot,
        reviewer: &GraphNode,
        previous: Option<&ContractActivation>,
        next: ReviewRound,
        criterion: &ReviewCriterion,
    ) -> Result<std::result::Result<(), String>> {
        let ReviewRound {
            repository,
            prompt,
            round,
            reask,
        } = next;
        let current = snapshot
            .contract()
            .epochs()
            .last()
            .ok_or_else(|| error("a reviewed turn has no epoch"))?;
        // A person's continuation that did not select the never-started
        // reviewer left it blocked in this epoch.
        let blocked = previous.is_none()
            && current.continuation.as_ref().is_some_and(|plan| {
                plan.selections.iter().any(|selection| {
                    matches!(selection, ContinuationSelection::LeaveUnmaterializedBlocked { node_id, .. }
                        if *node_id == reviewer.node_id)
                })
            });
        let epoch = if blocked {
            ExecutionEpochId::new(format!(
                "epoch-review-{}",
                &step_digest(&(
                    "host-review-unblock-epoch-v1",
                    snapshot.journal_id(),
                    &current.id
                ))?[..48]
            ))
            .map_err(error)?
        } else {
            current.id.clone()
        };
        let request = snapshot
            .request_ref()
            .ok_or_else(|| error("the turn request is unavailable"))?
            .clone();
        let round_text = self
            .content
            .retain_activation_evidence(ActivationEvidenceContent::Guidance {
                text: format!(
                    "This is review round {round} of {}. Judge only the result shown here.",
                    criterion.max_rounds
                ),
            })
            .map_err(error);
        let round_text = self.fail_closed(round_text)?.reference().clone();
        let digest = step_digest(&("host-review-round-v1", snapshot.journal_id(), &epoch, round))?;
        let activation = ActivationRef {
            session_id: snapshot.owner().session_id.clone(),
            turn_id: snapshot.turn_id().clone(),
            execution_epoch_id: epoch,
            node_id: reviewer.node_id.clone(),
            generation: round,
            activation_id: ActivationId::new(format!("review-activation-{}", &digest[..48]))
                .map_err(error)?,
        };
        let manifest_id =
            InputManifestId::new(format!("review-input-{}", &digest[..48])).map_err(error)?;
        let mut guidance = vec![request, prompt, round_text.clone()];
        guidance.extend(reask.clone());
        let event = match previous {
            Some(previous) => {
                let mut input = previous.input.clone();
                input.manifest_id = manifest_id;
                input.activation = activation;
                input.guidance = guidance;
                input.repository = repository;
                // Asked again, the reviewer reads the answer that had no
                // verdict; a round about a new result starts clean.
                input.revision_context = match (&reask, &previous.output) {
                    (Some(_), Some(output)) => Some(RevisionContext {
                        activation: previous.activation.clone(),
                        output: output.clone(),
                    }),
                    _ => None,
                };
                TurnContractEvent::ReviseAccepted {
                    previous: previous.activation.clone(),
                    input: Box::new(input),
                    invalidated_descendants: vec![],
                    evidence: reask.unwrap_or(round_text),
                }
            }
            None => {
                let (_, admission) = self
                    .content
                    .turn_admission(&self.canonical, &self.turn_id)
                    .map_err(error)?
                    .ok_or_else(|| error("a reviewed turn has no admission"))?;
                let admitted = admission
                    .nodes
                    .iter()
                    .find(|node| node.node_id == reviewer.node_id)
                    .ok_or_else(|| error("the reviewer has no admitted budget"))?;
                let input = ActivationInputManifest {
                    manifest_id,
                    activation,
                    definition: reviewer.definition.clone(),
                    conversation_id: reviewer.conversation_id.clone(),
                    starting_savepoint: reviewer.starting_savepoint.clone(),
                    parents: vec![],
                    guidance,
                    attachments: admitted.attachments.clone(),
                    repository,
                    budget: admitted.budget.clone(),
                    grant: Some(admitted.grant.clone()),
                    revision_context: None,
                };
                if blocked {
                    return self.unblock_review(snapshot, input, &round_text);
                }
                TurnContractEvent::StartActivation {
                    input: Box::new(input),
                }
            }
        };
        let appended = self.append(&format!("host-review-round-{}", &digest[..48]), event);
        self.fail_closed(appended)?;
        self.changed.notify_waiters();
        Ok(Ok(()))
    }

    /// One step of the review, once the required Agents are accepted and the
    /// required checks passed. Whether anything was recorded or started.
    fn drive_review(&mut self) -> Result<bool> {
        self.ready()?;
        let snapshot = self.canonical.snapshot(&self.turn_id).map_err(error)?;
        let contract = snapshot.contract();
        if contract.state() != Some(LogicalTurnState::Running)
            || contract.stop_requested().is_some()
            || contract.has_unknown_effects()
            || contract
                .activations()
                .iter()
                .any(|item| item.state == ActivationState::Running)
            || self.execution_admission_closed
        {
            return Ok(false);
        }
        let Some(graph) = contract.graph() else {
            return Ok(false);
        };
        let Some(reviewer) = review_node(graph).cloned() else {
            return Ok(false);
        };
        let Some(criterion) = review_criterion(graph, &self.content).map_err(error)? else {
            return Ok(false);
        };
        let id = review_condition_id();
        if contract.current_condition(&id).is_some()
            || !super::turn_checks::epoch_runs(contract, &id)
        {
            return Ok(false);
        }
        let epoch = contract
            .epochs()
            .last()
            .ok_or_else(|| error("a reviewed turn has no epoch"))?
            .id
            .clone();
        // Every required Agent is accepted, and the checks passed on the
        // result.
        let accepted = contract.current_accepted_activations();
        if graph.nodes.iter().filter(|node| node.required).any(|node| {
            !accepted
                .iter()
                .any(|item| item.activation.node_id == node.node_id)
        }) {
            return Ok(false);
        }
        if let Some((group, _)) =
            group_of(graph).filter(|(group, _)| *group == CheckGroup::required())
        {
            let ready = ConditionId::new(group.ready_id()).map_err(error)?;
            if !contract.condition_satisfied(&ready) {
                return Ok(false);
            }
        }
        let scope = &graph
            .conditions
            .iter()
            .find(|condition| condition.condition_id == id)
            .ok_or_else(|| error("the review condition is missing"))?
            .nodes;
        let mut reviewed = Vec::with_capacity(scope.len());
        for node in scope {
            let Some(item) = accepted
                .iter()
                .find(|item| item.activation.node_id == *node)
            else {
                return Ok(false);
            };
            reviewed.push(*item);
        }
        let activations: Vec<_> = reviewed
            .iter()
            .map(|item| item.activation.clone())
            .collect();
        let candidate = match self.review_candidate(&snapshot, &reviewed)? {
            Ok(candidate) => candidate,
            Err(reason) => {
                let proof = serde_json::json!({"kind":"required_review","turn_id":snapshot.turn_id(),
                    "epoch_id":epoch,"activations":activations,"round":0,"max_rounds":criterion.max_rounds,
                    "verdict":ReviewVerdict::Unreadable,"findings":"","passed":false,"reason":reason});
                self.record_review(&epoch, activations, proof, false)?;
                return Ok(true);
            }
        };
        let prompt = self
            .content
            .retain_activation_evidence(ActivationEvidenceContent::Guidance {
                text: candidate.prompt.clone(),
            })
            .map_err(error);
        let prompt = self.fail_closed(prompt)?.reference().clone();
        let latest = contract
            .activations()
            .iter()
            .rev()
            .find(|item| item.activation.node_id == reviewer.node_id)
            .cloned();
        let repository = reviewed
            .first()
            .map(|item| item.input.repository.clone())
            .unwrap_or(RepositoryInput::Unavailable);
        let Some(latest) = latest else {
            let first = ReviewRound {
                repository,
                prompt,
                round: 1,
                reask: None,
            };
            if let Err(reason) =
                self.start_review_round(&snapshot, &reviewer, None, first, &criterion)?
            {
                let proof = serde_json::json!({"kind":"required_review","turn_id":snapshot.turn_id(),
                    "epoch_id":epoch,"activations":activations,"round":0,"max_rounds":criterion.max_rounds,
                    "verdict":ReviewVerdict::Unreadable,"findings":"","passed":false,"reason":reason,
                    "candidate_sha256":candidate.tree});
                self.record_review(&epoch, activations, proof, false)?;
            }
            return Ok(true);
        };
        let current_round = latest.activation.execution_epoch_id == epoch
            && latest.input.guidance.contains(&prompt);
        let next_round = latest
            .activation
            .generation
            .checked_add(1)
            .ok_or_else(|| error("review round overflow"));
        match latest.state {
            ActivationState::Running | ActivationState::Unstarted => Ok(false),
            ActivationState::Accepted if current_round => {
                let reask =
                    self.record_verdict(&snapshot, &criterion, &latest, candidate, activations)?;
                if let Some(note) = reask {
                    // The same prompt, so the next answer is bound to the
                    // same result.
                    let next = ReviewRound {
                        repository,
                        prompt,
                        round: next_round?,
                        reask: Some(note),
                    };
                    self.start_review_round(&snapshot, &reviewer, Some(&latest), next, &criterion)?
                        .map_err(error)?;
                }
                Ok(true)
            }
            ActivationState::Accepted => {
                let next = ReviewRound {
                    repository,
                    prompt,
                    round: next_round?,
                    reask: None,
                };
                self.start_review_round(&snapshot, &reviewer, Some(&latest), next, &criterion)?
                    .map_err(error)?;
                Ok(true)
            }
            ActivationState::Failed
            | ActivationState::Interrupted
            | ActivationState::Superseded => {
                let reason = if latest.activation.execution_epoch_id == epoch {
                    "The reviewer did not finish, so nothing approved this result. Select the \
                     reviewer and Continue to run it again, or Finish."
                } else {
                    "The reviewer did not finish in an earlier epoch and was not restarted. \
                     Select the reviewer and Continue to run it again, or Finish."
                };
                let proof = serde_json::json!({"kind":"required_review","turn_id":snapshot.turn_id(),
                    "epoch_id":epoch,"reviewer":latest.activation,"activations":activations,
                    "round":latest.activation.generation,"max_rounds":criterion.max_rounds,
                    "verdict":ReviewVerdict::Unreadable,"findings":"","passed":false,"reason":reason,
                    "candidate_sha256":candidate.tree});
                self.record_review(&epoch, activations, proof, false)?;
                Ok(true)
            }
        }
    }

    /// Record the verdict of the accepted reviewer `item`, which was shown
    /// `candidate`, and send a request for changes back to the lead while
    /// rounds remain. An answer without a readable verdict about the tree it
    /// was shown records nothing while a round remains and the host has not
    /// yet asked this reviewer again about this result: the note to ask again
    /// with in the next round is returned instead.
    fn record_verdict(
        &mut self,
        snapshot: &DurableTurnSnapshot,
        criterion: &ReviewCriterion,
        item: &ContractActivation,
        candidate: ReviewCandidate,
        activations: Vec<ActivationRef>,
    ) -> Result<Option<EvidenceRef>> {
        let epoch = item.activation.execution_epoch_id.clone();
        let round = item.activation.generation;
        let answer = self.accepted_answer(snapshot, &item.activation)?;
        let (verdict, findings) = parse_verdict(&answer);
        let seen = |phase| -> Result<Option<Option<String>>> {
            Ok(self
                .review_capture(snapshot, item, phase)?
                .map(|capture| capture.and_then(|capture| capture.tree_sha256)))
        };
        let before = seen(RepositorySnapshotPhase::Before)?;
        let after = seen(RepositorySnapshotPhase::After)?;
        let unbound = binding_failure(
            candidate.tree.as_deref(),
            before.as_ref().map(Option::as_deref),
            after.as_ref().map(Option::as_deref),
        );
        let mut asked_again = false;
        if verdict == ReviewVerdict::Unreadable && unbound.is_none() {
            let note = self
                .content
                .retain_activation_evidence(ActivationEvidenceContent::Guidance {
                    text: REASK.to_owned(),
                })
                .map_err(error);
            let note = self.fail_closed(note)?.reference().clone();
            asked_again = item.input.guidance.contains(&note);
            if !asked_again && round < criterion.max_rounds {
                return Ok(Some(note));
            }
        }
        let passed = verdict == ReviewVerdict::Approve && unbound.is_none();
        let mut plan = None;
        let reason = match (verdict, unbound) {
            (_, Some(reason)) => Some(reason.to_owned()),
            (ReviewVerdict::Unreadable, None) if asked_again => Some(UNREADABLE_AGAIN.to_owned()),
            (ReviewVerdict::Unreadable, None) => Some(UNREADABLE.to_owned()),
            (ReviewVerdict::Approve, None) => None,
            (ReviewVerdict::Changes, None) if round >= criterion.max_rounds => Some(format!(
                "The reviewer asked for changes in round {round} of {}, the last round the host \
                 runs. Read the findings, then Revise the lead with them, change the files \
                 yourself and Continue the review, or Finish.",
                criterion.max_rounds
            )),
            (ReviewVerdict::Changes, None) => {
                match self.review_revision(snapshot, criterion, round, &findings, &activations)? {
                    Ok(continuation) => {
                        plan = Some(continuation);
                        None
                    }
                    Err(reason) => Some(reason),
                }
            }
        };
        let proof = ReviewProof {
            kind: "required_review".into(),
            round,
            max_rounds: criterion.max_rounds,
            verdict,
            findings,
            passed,
            reason,
            candidate_sha256: candidate.tree.clone(),
            continued: plan.is_some(),
        };
        let mut value = serde_json::to_value(&proof).map_err(error)?;
        value["turn_id"] = serde_json::to_value(snapshot.turn_id()).map_err(error)?;
        value["epoch_id"] = serde_json::to_value(&epoch).map_err(error)?;
        value["reviewer"] = serde_json::to_value(&item.activation).map_err(error)?;
        value["activations"] = serde_json::to_value(&activations).map_err(error)?;
        value["reviewed_sha256"] = serde_json::to_value(before.flatten()).map_err(error)?;
        self.record_review(&epoch, activations, value, passed)?;
        let Some(plan) = plan else {
            return Ok(None);
        };
        // The host's own continuation: pause the epoch the review failed in
        // and continue in a new one that revises the lead.
        let digest = step_digest(&("host-review-continue-v1", &plan.epoch_id))?;
        let paused = self.append(
            &format!("host-review-pause-{}", &digest[..48]),
            TurnContractEvent::PauseEpoch {
                epoch_id: epoch.clone(),
            },
        );
        self.fail_closed(paused)?;
        let continued = self.append(
            &format!("host-review-continue-{}", &digest[..48]),
            TurnContractEvent::Continue { plan },
        );
        self.fail_closed(continued)?;
        self.changed.notify_waiters();
        Ok(None)
    }
}

impl SessionDispatchController {
    /// The turn driver's review step after the required checks: start the
    /// next round of the required reviewer, or record its verdict. Whether
    /// anything changed.
    pub(crate) fn drive_turn_review(&self) -> Result<bool> {
        self.lock()?.drive_review()
    }
}
