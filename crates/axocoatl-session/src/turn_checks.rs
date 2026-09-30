//! Host-run checks of a turn: exact commands the host runs after the required
//! Agents finish, between two repository captures, and one readiness review.
//!
//! A group's conditions share one identity prefix in the turn graph: `{P}0`
//! captures the repository before, `{P}1..={P}n` run the n commands, `{P}n+1`
//! captures it after, and the review `{P}ready` records whether every command
//! passed on an unchanged tree. Required checks of a Session team use
//! [`REQUIRED_CHECK_PREFIX`].
use serde::Serialize;

use crate::execution_content::{
    ConditionProcessStatus, ExecutionContentError, ExecutionContentStore,
    RepositoryCheckDefinition, REPOSITORY_SNAPSHOT_COMMAND, REPOSITORY_SNAPSHOT_COMMAND_V1,
};
use crate::execution_store::DurableTurnSnapshot;
use crate::turn_contract::{
    ConditionEffectResolution, ConditionId, ConditionKind, ConditionOutcome, ConditionRunId,
    EffectDisposition, EvidenceRef, LogicalTurnState, TurnGraphSnapshot, MAX_COMPLETION_CONDITIONS,
};

/// Condition identities of a Session team's required checks. Conditions are
/// already scoped to their turn, so the prefix names no turn.
pub const REQUIRED_CHECK_PREFIX: &str = "required-check:";

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
        if prefix != REQUIRED_CHECK_PREFIX {
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

/// Capture commands a check group may carry, newest first. A turn keeps the
/// capture it was admitted with.
const CAPTURE_COMMANDS: [&str; 2] = [REPOSITORY_SNAPSHOT_COMMAND, REPOSITORY_SNAPSHOT_COMMAND_V1];

/// The exact definitions of a group: a capture, each command, and a capture.
/// The foreground command lifetime and capture ceilings apply; this adds no
/// tool capability, cost or token grant.
pub fn check_definitions(
    checks: &[Vec<String>],
) -> Result<Vec<RepositoryCheckDefinition>, ExecutionContentError> {
    check_definitions_with(checks, REPOSITORY_SNAPSHOT_COMMAND)
}

/// The definitions of `checks` in `group` as `graph` was admitted with them:
/// the current form, or the form with an earlier capture command that a turn
/// admitted before it changed still carries. Any other capture fails closed.
pub fn admitted_check_definitions(
    graph: &TurnGraphSnapshot,
    content: &ExecutionContentStore,
    group: &CheckGroup,
    checks: &[Vec<String>],
) -> Result<Vec<RepositoryCheckDefinition>, ExecutionContentError> {
    let first = group.condition_id(0);
    let Some(recorded) = graph
        .conditions
        .iter()
        .find_map(|condition| match &condition.kind {
            ConditionKind::RepositoryCheck { definition }
                if condition.condition_id.as_str() == first =>
            {
                Some(definition)
            }
            _ => None,
        })
    else {
        return check_definitions(checks);
    };
    let recorded = content.resolve_repository_check_definition(recorded)?;
    for capture in CAPTURE_COMMANDS {
        let definitions = check_definitions_with(checks, capture)?;
        if definitions.first() == Some(recorded) {
            return Ok(definitions);
        }
    }
    Err(ExecutionContentError::Invalid(
        "a check group's capture is not one this version can run",
    ))
}

fn check_definitions_with(
    checks: &[Vec<String>],
    capture: &str,
) -> Result<Vec<RepositoryCheckDefinition>, ExecutionContentError> {
    if checks.is_empty() {
        return Ok(Vec::new());
    }
    if checks.len().saturating_add(3) > MAX_COMPLETION_CONDITIONS {
        return Err(ExecutionContentError::Capacity);
    }
    let capture = RepositoryCheckDefinition {
        argv: vec!["sh".into(), "-c".into(), capture.into()],
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

/// Invocations one pass of `checks` required checks spends from the paying
/// grant: the Before capture, each command and the After capture.
pub fn check_pass_invocations(checks: usize) -> u32 {
    u32::try_from(checks).unwrap_or(u32::MAX).saturating_add(2)
}

/// Invocations the paying Agent keeps for its turn's `checks` required checks:
/// one pass, and one more for a Continue, which runs every check again.
/// Nothing without checks.
pub fn check_allowance(checks: usize) -> u32 {
    if checks == 0 {
        return 0;
    }
    check_pass_invocations(checks).saturating_mul(2)
}

/// The smallest invocation limit of the Agent that pays for `checks`
/// required checks: their allowance, the captures of its own changes before
/// and after it runs, and one model call to answer.
pub fn payer_minimum_invocations(checks: usize) -> u32 {
    check_allowance(checks).saturating_add(3)
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

    /// An empty content store of its own; the directory must outlive it.
    fn content_store(root: &std::path::Path) -> ExecutionContentStore {
        use crate::execution_namespace::ExecutionComponent;
        use crate::execution_ownership::LegacyFormatOwnership;
        use crate::execution_store::{ExecutionStoreOwner, SessionExecutionStore};
        let canonical = SessionExecutionStore::open(
            std::sync::Arc::new(
                LegacyFormatOwnership::acquire(root)
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
        ExecutionContentStore::open_owned(
            canonical
                .component_namespace(ExecutionComponent::ExecutionContent)
                .unwrap(),
        )
        .unwrap()
    }

    /// A graph whose group `group` carries `definitions`, retained in `content`.
    fn graph_with(
        content: &mut ExecutionContentStore,
        group: &CheckGroup,
        definitions: Vec<RepositoryCheckDefinition>,
    ) -> TurnGraphSnapshot {
        let conditions = definitions
            .into_iter()
            .enumerate()
            .map(|(index, definition)| CompletionCondition {
                condition_id: ConditionId::new(group.condition_id(index)).unwrap(),
                kind: ConditionKind::RepositoryCheck {
                    definition: content
                        .retain_repository_check_definition(definition)
                        .unwrap()
                        .reference()
                        .clone(),
                },
                nodes: vec![TurnNodeId::new("node").unwrap()],
            })
            .collect();
        TurnGraphSnapshot {
            snapshot_id: GraphSnapshotId::new("graph").unwrap(),
            revision: 1,
            nodes: vec![],
            dependencies: vec![],
            conditions,
        }
    }

    /// A turn admitted with the earlier capture command keeps it, so its
    /// graph still loads and its checks still run; an unknown capture fails
    /// closed and a graph without the group gets the current form.
    #[test]
    fn admitted_checks_keep_the_capture_they_were_admitted_with() {
        let root = tempfile::tempdir().unwrap();
        let mut content = content_store(root.path());
        let checks = vec![vec!["cargo".to_string(), "test".to_string()]];
        let group = CheckGroup::required();
        let current = check_definitions(&checks).unwrap();
        let earlier = check_definitions_with(&checks, REPOSITORY_SNAPSHOT_COMMAND_V1).unwrap();
        assert_ne!(current, earlier);
        assert_eq!(current[0].argv[2], REPOSITORY_SNAPSHOT_COMMAND);
        for definitions in [current.clone(), earlier] {
            let graph = graph_with(&mut content, &group, definitions.clone());
            assert_eq!(
                admitted_check_definitions(&graph, &content, &group, &checks).unwrap(),
                definitions
            );
        }
        let mut unknown = current.clone();
        unknown[0].argv[2] = "true".into();
        let graph = graph_with(&mut content, &group, unknown);
        assert!(admitted_check_definitions(&graph, &content, &group, &checks).is_err());
        let other = CheckGroup {
            prefix: "other-check:".into(),
        };
        let graph = graph_with(&mut content, &other, current.clone());
        assert_eq!(
            admitted_check_definitions(&graph, &content, &group, &checks).unwrap(),
            current
        );
    }

    #[test]
    fn check_definitions_fit_the_actual_condition_store_bound() {
        let root = tempfile::tempdir().unwrap();
        let mut content = content_store(root.path());
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
        assert!(!group.contains(&ConditionId::new("other:ready").unwrap()));
        // A review alone, or captures without a command, is not a group.
        let mut partial = graph.clone();
        partial
            .conditions
            .retain(|condition| condition.condition_id.as_str() != "required-check:1");
        assert_eq!(group_of(&partial), None);
    }
}
