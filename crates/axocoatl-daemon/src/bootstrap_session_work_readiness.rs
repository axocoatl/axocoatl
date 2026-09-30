use super::*;
use axocoatl_session::execution_content::{
    ConditionProcessStatus, ExecutionContentStore, RepositorySnapshotPhase,
};
use axocoatl_session::execution_store::DurableTurnSnapshot;
use axocoatl_session::turn_checks::{project_check, CheckGroup, TurnCheckView};
use axocoatl_session::turn_contract::{ConditionId, ConditionOutcome, EvidenceRef};

#[derive(Serialize)]
pub struct SessionWorkReadiness {
    pub state: String,
    pub candidate_sha256: Option<String>,
    pub evidence: Option<EvidenceRef>,
    pub checks: Vec<TurnCheckView>,
    pub reason: Option<String>,
}

impl AxocoatlDaemon {
    pub(super) fn work_readiness(
        &self,
        receipt: &TeamWorkReceipt,
        binding: Option<&ArmedTeamWorkBinding>,
        current: Option<&ArmedTeamWorkBinding>,
    ) -> Result<SessionWorkReadiness, DaemonError> {
        let mut view = SessionWorkReadiness {
            state: "unmet".into(),
            candidate_sha256: None,
            evidence: None,
            checks: binding
                .map(|binding| {
                    binding
                        .required_checks
                        .iter()
                        .map(|argv| TurnCheckView::pending(argv.clone()))
                        .collect()
                })
                .unwrap_or_default(),
            reason: Some("The candidate has no complete recorded check results".into()),
        };
        let Some(binding) = binding else {
            view.reason = Some("The original work binding is unavailable".into());
            return Ok(view);
        };
        if binding.required_checks.is_empty() {
            view.reason = Some(
                "No required checks were configured; an Agent answer does not establish readiness"
                    .into(),
            );
            return Ok(view);
        }
        let token = self
            .session_dispatch_lifecycles
            .session_team_token(&receipt.request.binding.session_id)?;
        self.session_dispatch_lifecycles.with_session_team_stores(
            &token,
            |canonical, content, _| {
                let id = LogicalTurnId::new(&receipt.turn_id).map_err(work_error)?;
                if canonical.turn(&id).map_err(work_error)?.is_some() {
                    let snapshot = canonical.snapshot(&id).map_err(work_error)?;
                    project(&snapshot, content, receipt, binding, &mut view)?;
                }
                Ok(())
            },
        )?;
        if current.is_none_or(|current| current.binding != binding.binding) {
            view.state = "stale".into();
            view.reason = Some("The source binding changed; these results cover only the recorded candidate and checks".into());
        }
        Ok(view)
    }
}

fn project(
    snapshot: &DurableTurnSnapshot,
    content: &ExecutionContentStore,
    receipt: &TeamWorkReceipt,
    binding: &ArmedTeamWorkBinding,
    view: &mut SessionWorkReadiness,
) -> Result<(), DaemonError> {
    let contract = snapshot.contract();
    let definitions =
        axocoatl_session::team_work::standing_check_definitions(&binding.required_checks)
            .map_err(work_error)?;
    let mut actual_runs = Vec::new();
    let mut command_candidates = Vec::new();
    for (index, definition) in definitions.iter().enumerate() {
        let id = ConditionId::new(axocoatl_session::team_work::standing_condition_id(
            &receipt.receipt_id,
            index,
        ))
        .map_err(work_error)?;
        if index > 0 && index + 1 < definitions.len() {
            view.checks[index - 1] =
                project_check(snapshot, content, &id, definition).map_err(work_error)?;
        }
        let Some(observation) = contract.current_condition(&id) else {
            actual_runs.push(None);
            continue;
        };
        let actual = contract.condition_runs().iter().rev().find(|run| run.run.condition_id == id && matches!(&run.resolution, Some(axocoatl_session::turn_contract::ConditionEffectResolution::OutcomeRecorded {evidence}) if evidence == &observation.evidence));
        let Some(actual) = actual else {
            actual_runs.push(None);
            continue;
        };
        let Some(arguments) = content
            .condition_arguments(snapshot, &actual.run.run_id)
            .map_err(work_error)?
        else {
            actual_runs.push(None);
            continue;
        };
        if arguments.definition() != definition {
            return Err(work_error(
                "Recorded work check differs from its original binding",
            ));
        }
        let Some(result) = content.condition_result(&arguments).map_err(work_error)? else {
            actual_runs.push(None);
            continue;
        };
        let passed = observation.outcome == ConditionOutcome::Passed
            && result.status() == &(ConditionProcessStatus::Exited { code: 0 });
        if index > 0 && index + 1 < definitions.len() {
            let check = &mut view.checks[index - 1];
            // Never display an older passing observation over a later recorded run.
            if check.run_id.as_ref() != Some(&actual.run.run_id) {
                actual_runs.push(None);
                continue;
            }
            let group = CheckGroup::standing(&receipt.receipt_id);
            let candidate = content
                .check_candidate(
                    snapshot,
                    &actual.run,
                    &group.condition_id(0),
                    &group.condition_id(binding.required_checks.len() + 1),
                )
                .map_err(work_error)?;
            check.candidate_sha256 = candidate.as_ref().map(|(tree, _)| tree.clone());
            command_candidates.push((index - 1, candidate));
        }
        actual_runs.push(Some((&actual.run, passed)));
    }
    let Some(Some((before_run, true))) = actual_runs.first() else {
        return Ok(());
    };
    let Some(Some((after_run, true))) = actual_runs.last() else {
        return Ok(());
    };
    let Some(activation) = before_run.activations.first() else {
        return Ok(());
    };
    let captures = content
        .repository_snapshots(snapshot, activation)
        .map_err(work_error)?;
    let before = captures.iter().find(|capture| {
        capture.content.condition_run.as_ref() == Some(&before_run.run_id)
            && capture.content.phase == (RepositorySnapshotPhase::BeforeCheck { index: 0 })
    });
    let after = captures.iter().find(|capture| {
        capture.content.condition_run.as_ref() == Some(&after_run.run_id)
            && capture.content.phase == (RepositorySnapshotPhase::AfterCheck { index: 0 })
    });
    let (Some(before), Some(after)) = (before, after) else {
        return Ok(());
    };
    view.candidate_sha256 = after.content.tree_sha256.clone();
    let stable = before.content.tree_sha256.is_some()
        && before.content.tree_sha256 == after.content.tree_sha256
        && before.content.head == after.content.head;
    let candidate = after
        .content
        .tree_sha256
        .clone()
        .map(|tree| (tree, after.content.head.clone()));
    let commands_match = command_candidates.len() == binding.required_checks.len()
        && command_candidates
            .iter()
            .all(|(_, observed)| observed.is_some() && observed == &candidate);
    for (index, observed) in &command_candidates {
        if observed.is_none() || observed != &candidate {
            view.checks[*index].state = "stale".into();
        }
    }
    let id =
        ConditionId::new(format!("standing:{}:ready", receipt.receipt_id)).map_err(work_error)?;
    let Some(review) = contract.current_condition(&id) else {
        return Ok(());
    };
    view.evidence = Some(review.evidence.clone());
    if !stable {
        view.reason = Some("The repository changed while required checks ran; the recorded result does not establish readiness".into());
        return Ok(());
    }
    if commands_match
        && review.outcome == ConditionOutcome::Passed
        && actual_runs
            .iter()
            .all(|run| run.as_ref().is_some_and(|(_, passed)| *passed))
        && contract.state() == Some(LogicalTurnState::Completed)
    {
        view.state = "ready".into();
        view.reason = None;
    } else if view.checks.iter().any(|check| check.state == "stale") {
        view.reason = Some("Some commands do not have matching before/after evidence for this candidate; explicitly select those checks to run again".into());
    } else if view.checks.iter().any(|check| check.state == "failed") {
        view.reason = Some("At least one required command failed on the recorded candidate".into());
    } else {
        view.reason = Some(
            "The recorded checks are available, but the turn still has unmet completion conditions"
                .into(),
        );
    }
    Ok(())
}
