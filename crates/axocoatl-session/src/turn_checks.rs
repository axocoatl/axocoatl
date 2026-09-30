//! Host-run checks of a turn: exact commands the host runs after the required
//! Agents finish, between two repository captures, and one readiness review.
//!
//! A group's conditions share one identity prefix in the turn graph: `{P}0`
//! captures the repository before, `{P}1..={P}n` run the n commands, `{P}n+1`
//! captures it after, and the review `{P}ready` records whether every command
//! passed on an unchanged tree. Required checks of a Session team use
//! [`REQUIRED_CHECK_PREFIX`]; standing work keeps its own prefix until the
//! inbox is removed.
use serde::Serialize;

use crate::execution_content::{
    ConditionProcessStatus, ExecutionContentError, ExecutionContentStore,
    RepositoryCheckDefinition, REPOSITORY_SNAPSHOT_COMMAND,
};
use crate::execution_store::DurableTurnSnapshot;
use crate::turn_contract::{
    ConditionEffectResolution, ConditionId, ConditionKind, ConditionOutcome, ConditionRunId,
    EffectDisposition, EvidenceRef, LogicalTurnState, TurnGraphSnapshot, MAX_COMPLETION_CONDITIONS,
};

/// Condition identities of a Session team's required checks. Conditions are
/// already scoped to their turn, so the prefix names no turn.
pub const REQUIRED_CHECK_PREFIX: &str = "required-check:";

const STANDING_PREFIX: &str = "standing:";

/// One group of host-run checks in a turn graph, named by its prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckGroup {
    prefix: String,
}

impl CheckGroup {
    /// The required checks of a Session team.
    pub fn required() -> Self {
        Self {
            prefix: REQUIRED_CHECK_PREFIX.into(),
        }
    }
    /// The checks armed on one standing-work receipt.
    pub fn standing(receipt: &str) -> Self {
        Self {
            prefix: format!("{STANDING_PREFIX}{receipt}:"),
        }
    }
    pub fn prefix(&self) -> &str {
        &self.prefix
    }
    /// Condition `index` of the group: 0 and n+1 are the captures.
    pub fn condition_id(&self, index: usize) -> String {
        format!("{}{index}", self.prefix)
    }
    pub fn ready_id(&self) -> String {
        format!("{}ready", self.prefix)
    }
    pub fn contains(&self, id: &ConditionId) -> bool {
        id.as_str().starts_with(&self.prefix)
    }
}

/// The check group a turn graph carries and its number of commands. It is
/// recognized from its readiness review and the repository checks
/// `{P}0..={P}n+1` before it; a graph without both carries no group.
pub fn group_of(graph: &TurnGraphSnapshot) -> Option<(CheckGroup, usize)> {
    graph.conditions.iter().find_map(|condition| {
        if !matches!(condition.kind, ConditionKind::Review { .. }) {
            return None;
        }
        let prefix = condition.condition_id.as_str().strip_suffix("ready")?;
        if prefix != REQUIRED_CHECK_PREFIX
            && !(prefix.starts_with(STANDING_PREFIX)
                && prefix.len() > STANDING_PREFIX.len() + 1
                && prefix.ends_with(':'))
        {
            return None;
        }
        let group = CheckGroup {
            prefix: prefix.into(),
        };
        let definitions = (0..)
            .take_while(|index| {
                let id = group.condition_id(*index);
                graph.conditions.iter().any(|condition| {
                    condition.condition_id.as_str() == id
                        && matches!(condition.kind, ConditionKind::RepositoryCheck { .. })
                })
            })
            .count();
        definitions
            .checked_sub(2)
            .filter(|checks| *checks > 0)
            .map(|checks| (group, checks))
    })
}

/// The exact definitions of a group: a capture, each command, and a capture.
/// The foreground command lifetime and capture ceilings apply; this adds no
/// tool capability, cost or token grant.
pub fn check_definitions(
    checks: &[Vec<String>],
) -> Result<Vec<RepositoryCheckDefinition>, ExecutionContentError> {
    if checks.is_empty() {
        return Ok(Vec::new());
    }
    if checks.len().saturating_add(3) > MAX_COMPLETION_CONDITIONS {
        return Err(ExecutionContentError::Capacity);
    }
    let capture = RepositoryCheckDefinition {
        argv: vec!["sh".into(), "-c".into(), REPOSITORY_SNAPSHOT_COMMAND.into()],
        timeout_ms: 180_000,
        stdout_bytes: 768 * 1024,
        stderr_bytes: 256 * 1024,
    };
    let mut definitions = vec![capture.clone()];
    for argv in checks {
        check_command(argv)?;
        definitions.push(RepositoryCheckDefinition {
            argv: argv.clone(),
            timeout_ms: 180_000,
            stdout_bytes: 768 * 1024,
            stderr_bytes: 256 * 1024,
        });
    }
    definitions.push(capture);
    Ok(definitions)
}

/// One check's argv as a shell command. It bounds the arguments and rejects
/// NUL; it does not restrict what the command is.
pub fn check_command(argv: &[String]) -> Result<String, ExecutionContentError> {
    if argv.is_empty()
        || argv[0].is_empty()
        || argv.len() > 64
        || argv
            .iter()
            .any(|arg| arg.len() > 4096 || arg.contains('\0'))
    {
        return Err(ExecutionContentError::Invalid("invalid check arguments"));
    }
    Ok(format!(
        "exec {}",
        argv.iter()
            .map(|arg| format!("'{}'", arg.replace('\'', "'\"'\"'")))
            .collect::<Vec<_>>()
            .join(" ")
    ))
}

/// The readiness criterion of a Session team's required checks.
pub fn readiness_text(checks: &[Vec<String>]) -> String {
    serde_json::json!({"kind":"required_check_readiness","required_checks":checks,"rule":"all exact checks pass and the captured repository tree remains unchanged"}).to_string()
}

/// What the latest run of one check shows.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TurnCheckView {
    pub argv: Vec<String>,
    pub state: String,
    pub run_id: Option<ConditionRunId>,
    pub process_status: Option<ConditionProcessStatus>,
    pub effect_disposition: Option<EffectDisposition>,
    pub primary_exit: Option<ConditionProcessStatus>,
    pub quiescent: Option<bool>,
    pub reason: Option<String>,
    pub evidence: Option<EvidenceRef>,
    pub candidate_sha256: Option<String>,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

impl TurnCheckView {
    /// A check with no recorded run.
    pub fn pending(argv: Vec<String>) -> Self {
        Self {
            argv,
            state: "pending".into(),
            run_id: None,
            process_status: None,
            effect_disposition: None,
            primary_exit: None,
            quiescent: None,
            reason: None,
            evidence: None,
            candidate_sha256: None,
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            stdout_truncated: false,
            stderr_truncated: false,
        }
    }
}

/// Read the latest exact run even when it has no readiness observation. A retained
/// process outcome and permission to declare the candidate ready are separate facts.
pub fn project_check(
    snapshot: &DurableTurnSnapshot,
    content: &ExecutionContentStore,
    id: &ConditionId,
    definition: &RepositoryCheckDefinition,
) -> Result<TurnCheckView, ExecutionContentError> {
    let mut check = TurnCheckView::pending(definition.argv.clone());
    let contract = snapshot.contract();
    let Some(actual) = contract
        .condition_runs()
        .iter()
        .rev()
        .find(|run| &run.run.condition_id == id)
    else {
        if contract.state() == Some(LogicalTurnState::Finished)
            && contract
                .stop_requested()
                .and_then(|intent| intent.partial_finish.as_ref())
                .is_some_and(|selection| selection.missing_condition_ids.contains(id))
        {
            check.state = "skipped".into();
            check.reason =
                Some("Not run; explicitly left unmet by the recorded partial Finish".into());
        }
        return Ok(check);
    };
    check.run_id = Some(actual.run.run_id.clone());
    check.effect_disposition = Some(actual.disposition());
    check.state = "outcome_unknown".into();
    check.reason = Some("No durable process outcome is recorded. The check may still be running; its effects are not safe to replay automatically".into());
    let Some(arguments) = content.condition_arguments(snapshot, &actual.run.run_id)? else {
        return Ok(check);
    };
    if arguments.definition() != definition {
        return Err(ExecutionContentError::Invalid(
            "recorded check differs from its definition",
        ));
    }
    let Some(result) = content.condition_result(&arguments)? else {
        return Ok(check);
    };
    let current_pass = contract.current_condition(id).is_some_and(|observation|
        observation.outcome == ConditionOutcome::Passed && observation.evidence == *result.reference()
        && matches!(&actual.resolution, Some(ConditionEffectResolution::OutcomeRecorded { evidence }) if evidence == result.reference()));
    check.state = match result.status() {
        ConditionProcessStatus::NotDispatched => "not_dispatched",
        ConditionProcessStatus::Exited { code: 0 } if current_pass => "passed",
        ConditionProcessStatus::Exited { code: 0 } => "unverified",
        ConditionProcessStatus::Exited { .. } => "failed",
        ConditionProcessStatus::Signalled { .. } => "signalled",
        ConditionProcessStatus::TimedOut => "timed_out",
        ConditionProcessStatus::Interrupted => "interrupted",
        ConditionProcessStatus::LaunchFailed { .. } => "launch_failed",
        ConditionProcessStatus::Uncertain { .. } => "outcome_unknown",
    }
    .into();
    check.reason = match result.status() {
        ConditionProcessStatus::LaunchFailed { message }
        | ConditionProcessStatus::Uncertain { message } => Some(message.clone()),
        ConditionProcessStatus::Exited { code: 0 } if !current_pass => Some(
            "The process exited successfully, but has no current passing readiness observation"
                .into(),
        ),
        _ => None,
    };
    check.process_status = Some(result.status().clone());
    check.primary_exit = result
        .supervision()
        .and_then(|evidence| evidence.primary_exit.clone());
    check.quiescent = result.supervision().map(|evidence| evidence.quiescent);
    check.evidence = Some(result.reference().clone());
    check.exit_code = match result.status() {
        ConditionProcessStatus::Exited { code } => Some(*code),
        _ => None,
    };
    (check.stdout, check.stdout_truncated) = output_preview(
        &result.stdout().retained_bytes()?,
        result.stdout().is_truncated(),
    );
    (check.stderr, check.stderr_truncated) = output_preview(
        &result.stderr().retained_bytes()?,
        result.stderr().is_truncated(),
    );
    Ok(check)
}

fn output_preview(bytes: &[u8], truncated: bool) -> (String, bool) {
    const LIMIT: usize = 4096;
    let text = String::from_utf8_lossy(bytes);
    let mut end = text.len().min(LIMIT);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_owned(), truncated || end < text.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turn_contract::{CompletionCondition, GraphSnapshotId, TurnNodeId};

    #[test]
    fn command_output_preview_is_bounded_and_preserves_truncation() {
        let text = "é".repeat(3000);
        let (preview, truncated) = output_preview(text.as_bytes(), false);
        assert_eq!(preview.len(), 4096);
        assert!(truncated);
        assert_eq!(output_preview(b"ok", true), ("ok".into(), true));
    }

    #[test]
    fn check_definitions_fit_the_actual_condition_store_bound() {
        use crate::execution_namespace::ExecutionComponent;
        use crate::execution_ownership::LegacyFormatOwnership;
        use crate::execution_store::{ExecutionStoreOwner, SessionExecutionStore};
        let root = tempfile::tempdir().unwrap();
        let canonical = SessionExecutionStore::open(
            std::sync::Arc::new(
                LegacyFormatOwnership::acquire(root.path())
                    .unwrap()
                    .upgrade()
                    .unwrap(),
            ),
            ExecutionStoreOwner {
                workspace_id: "checks".into(),
                session_id: crate::turn_contract::SessionId::new("session").unwrap(),
            },
        )
        .unwrap();
        let mut content = ExecutionContentStore::open_owned(
            canonical
                .component_namespace(ExecutionComponent::ExecutionContent)
                .unwrap(),
        )
        .unwrap();
        let commands = vec![vec!["cargo".into(), "test".into(), "--quiet".into()]];
        let definitions = check_definitions(&commands).unwrap();
        assert_eq!(definitions.len(), 3);
        assert_eq!(definitions[1].argv, commands[0]);
        for definition in definitions {
            assert_eq!(
                definition.stdout_bytes + definition.stderr_bytes,
                1024 * 1024
            );
            // Full 512 KiB patch in base64, 8 KiB manifest prefix and metadata.
            assert!(definition.stdout_bytes >= (512 * 1024 * 4 / 3) + 16384);
            content
                .retain_repository_check_definition(definition)
                .unwrap();
        }
        let too_many = vec![vec!["true".to_string()]; MAX_COMPLETION_CONDITIONS - 2];
        assert!(matches!(
            check_definitions(&too_many),
            Err(ExecutionContentError::Capacity)
        ));
        assert!(check_definitions(&[vec![]]).is_err());
    }

    #[test]
    fn check_group_ids_fit_identity_bounds() {
        let most = MAX_COMPLETION_CONDITIONS - 3;
        let group = CheckGroup::required();
        for index in 0..=most + 1 {
            ConditionId::new(group.condition_id(index)).unwrap();
        }
        ConditionId::new(group.ready_id()).unwrap();
        // The longest graph a group can join still names every condition.
        let node = TurnNodeId::new("node").unwrap();
        let definition = EvidenceRef::new("definition").unwrap();
        let mut conditions: Vec<_> = (0..=most + 1)
            .map(|index| CompletionCondition {
                condition_id: ConditionId::new(group.condition_id(index)).unwrap(),
                kind: ConditionKind::RepositoryCheck {
                    definition: definition.clone(),
                },
                nodes: vec![node.clone()],
            })
            .collect();
        conditions.push(CompletionCondition {
            condition_id: ConditionId::new(group.ready_id()).unwrap(),
            kind: ConditionKind::Review {
                criterion: definition.clone(),
            },
            nodes: vec![node.clone()],
        });
        assert_eq!(conditions.len(), MAX_COMPLETION_CONDITIONS);
        let graph = TurnGraphSnapshot {
            snapshot_id: GraphSnapshotId::new("graph").unwrap(),
            revision: 1,
            nodes: vec![],
            dependencies: vec![],
            conditions,
        };
        assert_eq!(group_of(&graph), Some((group.clone(), most)));
        assert!(group.contains(&ConditionId::new("required-check:ready").unwrap()));
        assert!(!group.contains(&ConditionId::new("standing:r:ready").unwrap()));
        let standing = CheckGroup::standing("receipt");
        assert_eq!(standing.condition_id(0), "standing:receipt:0");
        assert_eq!(standing.ready_id(), "standing:receipt:ready");
        // A review alone, or captures without a command, is not a group.
        let mut partial = graph.clone();
        partial
            .conditions
            .retain(|condition| condition.condition_id.as_str() != "required-check:1");
        assert_eq!(group_of(&partial), None);
    }
}
