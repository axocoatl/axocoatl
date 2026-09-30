//! Contract regressions; these do not establish delegated controller execution.
use axocoatl_session::control_authority::*;
use axocoatl_session::control_command::{BlockerResponse, ControlParameters, FinishMode};
use axocoatl_session::turn_contract::*;
use tempfile::TempDir;

const LEGACY: &str = include_str!("fixtures/operation-grant/legacy-v1.json");
const OPERATIONS: &str = include_str!("fixtures/operation-grant/operation-v1.json");

fn policy() -> AuthorityGrant {
    serde_json::from_str(OPERATIONS).unwrap()
}

fn authority(dir: &TempDir) -> ControlAuthority {
    ControlAuthority::open(
        dir.path(),
        SessionId::new("session-qa").unwrap(),
        LogicalTurnId::new("turn-build-184").unwrap(),
    )
    .unwrap()
}

fn target() -> ActivationRef {
    ActivationRef {
        session_id: SessionId::new("session-qa").unwrap(),
        turn_id: LogicalTurnId::new("turn-build-184").unwrap(),
        execution_epoch_id: ExecutionEpochId::new("epoch-1").unwrap(),
        node_id: TurnNodeId::new("tester").unwrap(),
        generation: 1,
        activation_id: ActivationId::new("tester-1").unwrap(),
    }
}

#[test]
fn old_retained_policy_bytes_and_explicit_stop_survive_without_new_rights() {
    let mut legacy: AuthorityGrant = serde_json::from_str(LEGACY).unwrap();
    assert!(legacy.delegation.is_none());
    assert_eq!(serde_json::to_string(&legacy).unwrap(), LEGACY.trim());
    assert!(legacy.permits_operation(DelegatedOperation::StopActivation, &target().node_id));
    for op in [
        DelegatedOperation::RetryActivation,
        DelegatedOperation::AddAgent,
        DelegatedOperation::FinishNormally,
        DelegatedOperation::ResumeMachineBlocker,
    ] {
        assert!(!legacy.permits_operation(op, &target().node_id));
    }
    legacy.allow_stop_descendants = false;
    assert!(!legacy.permits_operation(DelegatedOperation::StopActivation, &target().node_id));
}

#[test]
fn exact_policy_roundtrip_keeps_limits_required_checks_and_no_default_blockers() {
    let policy = policy();
    assert_eq!(serde_json::to_string(&policy).unwrap(), OPERATIONS.trim());
    let delegated = policy.delegation.as_ref().unwrap();
    assert!(delegated.machine_blockers.is_empty());
    assert_eq!(delegated.required_conditions.len(), 1);
    assert!(policy.permits_operation(DelegatedOperation::RetryActivation, &target().node_id));
    assert!(!policy.permits_operation(DelegatedOperation::StopActivation, &policy.holder));
    assert!(!policy.permits_operation(
        DelegatedOperation::RetryActivation,
        &TurnNodeId::new("unrelated").unwrap()
    ));
}

#[test]
fn exact_owner_mixed_legacy_authority_and_unknown_schema_are_rejected_without_mutation() {
    let dir = TempDir::new().unwrap();
    let gate = authority(&dir);
    let mut foreign = policy();
    foreign.delegation.as_mut().unwrap().scope.turn_id = LogicalTurnId::new("other-turn").unwrap();
    assert!(gate.install_grant(foreign, 0).is_err());
    let mut mixed = policy();
    mixed.allow_stop_descendants = true;
    assert!(gate.install_grant(mixed, 0).is_err());
    let mut future = policy();
    future.delegation.as_mut().unwrap().schema_version = 2;
    assert!(gate.install_grant(future, 0).is_err());
    assert_eq!(gate.revision().unwrap(), 0);
}

#[test]
fn narrowed_grant_cannot_add_operation_budget_template_or_remove_completion_obligation() {
    let dir = TempDir::new().unwrap();
    let gate = authority(&dir);
    let original = policy();
    gate.install_grant(original.clone(), 0).unwrap();
    let mut attempts = vec![];
    let mut increased = original.clone();
    increased.limits.tokens += 1;
    attempts.push(increased);
    let mut relaxed = original.clone();
    relaxed
        .delegation
        .as_mut()
        .unwrap()
        .required_conditions
        .clear();
    attempts.push(relaxed);
    let mut relaxed = original.clone();
    relaxed
        .delegation
        .as_mut()
        .unwrap()
        .completion_criteria
        .clear();
    attempts.push(relaxed);
    let mut expanded = original.clone();
    expanded
        .delegation
        .as_mut()
        .unwrap()
        .operations
        .push(DelegatedOperationPermission {
            operation: DelegatedOperation::FinishNormally,
            targets: DelegatedTargetScope::Nodes {
                nodes: vec![original.holder.clone()],
            },
        });
    attempts.push(expanded);
    let mut template = original.clone();
    template.delegation.as_mut().unwrap().templates[0].snapshot =
        EvidenceRef::new("new-definition-snapshot").unwrap();
    attempts.push(template);
    let mut resource = original.clone();
    resource.delegation.as_mut().unwrap().resource_policy =
        EvidenceRef::new("relaxed-resources").unwrap();
    attempts.push(resource);
    let mut absent = original.clone();
    absent.delegation = None;
    attempts.push(absent);
    for mut attempt in attempts {
        attempt.revision = 2;
        assert!(gate.narrow_grant(attempt, 1).is_err());
        assert_eq!(gate.revision().unwrap(), 1);
    }
    let mut narrowed = original.clone();
    narrowed.revision = 2;
    narrowed.delegation.as_mut().unwrap().operations.clear();
    narrowed.limits.tokens = 0;
    gate.narrow_grant(narrowed.clone(), 1).unwrap();
    assert_eq!(gate.grant_policy(&original.id).unwrap(), narrowed);
}

#[test]
fn future_subtree_growth_is_explicit_and_cannot_expand_through_narrowing() {
    let mut original: AuthorityGrant = serde_json::from_str(include_str!(
        "fixtures/operation-grant/explicit-future-subtree-v1.json"
    ))
    .unwrap();
    let dir = TempDir::new().unwrap();
    let gate = authority(&dir);
    gate.install_grant(original.clone(), 0).unwrap();
    // A pure NodeId lookup is deliberately insufficient for canonical topology.
    assert!(!original.permits_operation(DelegatedOperation::StopActivation, &target().node_id));
    original.revision = 2;
    for operation in &mut original.delegation.as_mut().unwrap().operations {
        if let DelegatedTargetScope::Subtree {
            include_future_descendants,
            ..
        } = &mut operation.targets
        {
            *include_future_descendants = false;
        }
    }
    gate.narrow_grant(original.clone(), 1).unwrap();
    original.revision = 3;
    if let DelegatedTargetScope::Subtree {
        include_future_descendants,
        ..
    } = &mut original.delegation.as_mut().unwrap().operations[0].targets
    {
        *include_future_descendants = true;
    }
    assert!(gate.narrow_grant(original, 2).is_err());
}

#[test]
fn new_contract_cannot_enter_legacy_dispatch_and_revocation_survives_restart() {
    let dir = TempDir::new().unwrap();
    let gate = authority(&dir);
    let policy = policy();
    gate.install_grant(policy.clone(), 0).unwrap();
    assert!(gate
        .register_activation(target(), &policy.id, policy.profiles[0].clone(), 1, 100)
        .is_err());
    assert_eq!(gate.usage(&policy.id).unwrap(), GrantUsage::default());
    assert_eq!(
        gate.grant_status(&policy.id).unwrap().revoked_at_revision,
        None
    );
    gate.revoke_grant(&policy.id, 1).unwrap();
    drop(gate);
    let reopened = authority(&dir);
    let status = reopened.grant_status(&policy.id).unwrap();
    assert_eq!(status.policy, policy);
    assert_eq!(status.revoked_at_revision, Some(2));
    assert!(reopened
        .install_grant(policy, status.authority_revision)
        .is_err());
}

#[test]
fn human_only_actions_have_no_serialized_delegated_permission() {
    for bad in [
        include_str!("fixtures/operation-grant/reject-force_partial_finish.json"),
        include_str!("fixtures/operation-grant/reject-keep_way.json"),
        include_str!("fixtures/operation-grant/reject-expand_grant.json"),
        include_str!("fixtures/operation-grant/reject-approve_human_blocker.json"),
        include_str!("fixtures/operation-grant/reject-update_global_definition.json"),
    ] {
        assert!(serde_json::from_str::<AuthorityGrant>(bad).is_err());
    }
    let evidence = EvidenceRef::new("caller-named-approval").unwrap();
    let force = ControlParameters::FinishTurn {
        mode: FinishMode::ForcePartial {
            approval: evidence.clone(),
            missing_conditions: vec![],
            stop_activations: vec![],
            selected_activations: vec![],
            missing_condition_ids: vec![],
        },
    };
    assert_eq!(force.delegated_operation(), None);
    let approval = ControlParameters::ResumeBlocked {
        activation: target(),
        blocker_id: evidence.clone(),
        response: BlockerResponse::Approval { approval: evidence },
    };
    assert_eq!(approval.delegated_operation(), None);
}
