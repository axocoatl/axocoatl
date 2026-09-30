use axocoatl_session::control_authority::*;
use axocoatl_session::invocation_audit::*;
use axocoatl_session::turn_contract::*;
use std::sync::{Arc, Barrier};
use tempfile::TempDir;

fn activation(node: &str, generation: u32, epoch: &str) -> ActivationRef {
    ActivationRef {
        session_id: SessionId::new("session-qa").unwrap(),
        turn_id: LogicalTurnId::new("turn-build-184").unwrap(),
        execution_epoch_id: ExecutionEpochId::new(epoch).unwrap(),
        node_id: TurnNodeId::new(node).unwrap(),
        generation,
        activation_id: ActivationId::new(format!("{node}-{generation}-{epoch}")).unwrap(),
    }
}

fn profile() -> ExecutionProfile {
    ExecutionProfile {
        definition: "qa-runner".into(),
        provider: "test-provider".into(),
        model: "test-model".into(),
        isolation: "session-podman".into(),
        tools: vec!["shell".into(), "read_file".into()],
        write_scope: None,
    }
}

fn grant() -> AuthorityGrant {
    AuthorityGrant {
        id: "grant-qa".into(),
        revision: 1,
        issuer_evidence: EvidenceRef::new("human-approval-1").unwrap(),
        holder: TurnNodeId::new("supervisor").unwrap(),
        descendants: vec![
            TurnNodeId::new("tester").unwrap(),
            TurnNodeId::new("reviewer").unwrap(),
        ],
        allow_stop_descendants: true,
        delegation: None,
        profiles: vec![profile()],
        conditions: vec![],
        limits: GrantLimits {
            activations: 10,
            invocations: 10,
            tokens: 1000,
            cost_microunits: 100,
        },
        expires_at_ms: 1000,
    }
}

fn authority(dir: &TempDir) -> ControlAuthority {
    ControlAuthority::open(
        dir.path(),
        SessionId::new("session-qa").unwrap(),
        LogicalTurnId::new("turn-build-184").unwrap(),
    )
    .unwrap()
}

fn audit(dir: &TempDir) -> InvocationAudit {
    InvocationAudit::open(
        dir.path(),
        InvocationAuditOwner {
            workspace_id: "workspace-client-a".into(),
            session_id: SessionId::new("session-qa").unwrap(),
        },
    )
    .unwrap()
}

#[cfg(unix)]
fn owned_session(dir: &TempDir) -> axocoatl_session::execution_store::SessionExecutionStore {
    use axocoatl_session::execution_ownership::LegacyFormatOwnership;
    use axocoatl_session::execution_store::{ExecutionStoreOwner, SessionExecutionStore};
    SessionExecutionStore::open(
        Arc::new(
            LegacyFormatOwnership::acquire(dir.path())
                .unwrap()
                .upgrade()
                .unwrap(),
        ),
        ExecutionStoreOwner {
            workspace_id: "workspace-client-a".into(),
            session_id: SessionId::new("session-qa").unwrap(),
        },
    )
    .unwrap()
}

#[cfg(unix)]
fn owned_authority(
    session: &axocoatl_session::execution_store::SessionExecutionStore,
) -> ControlAuthority {
    use axocoatl_session::execution_namespace::ExecutionComponent;
    ControlAuthority::open_owned(
        session
            .component_namespace(ExecutionComponent::ControlAuthority {
                turn_id: LogicalTurnId::new("turn-build-184").unwrap(),
            })
            .unwrap(),
    )
    .unwrap()
}

#[cfg(unix)]
fn owned_audit(
    session: &axocoatl_session::execution_store::SessionExecutionStore,
) -> InvocationAudit {
    use axocoatl_session::execution_namespace::ExecutionComponent;
    InvocationAudit::open_owned(
        session
            .component_namespace(ExecutionComponent::InvocationAudit)
            .unwrap(),
    )
    .unwrap()
}

fn ready(gate: &ControlAuthority, node: &str) -> ActivationLease {
    gate.install_grant(grant(), gate.revision().unwrap())
        .unwrap();
    gate.register_activation(
        activation(node, 1, "epoch-1"),
        "grant-qa",
        profile(),
        gate.revision().unwrap(),
        100,
    )
    .unwrap()
}

fn prepare(gate: &ControlAuthority, lease: &ActivationLease, id: &str) -> DispatchPreparation {
    gate.prepare_dispatch(
        lease,
        InvocationId::new(id).unwrap(),
        "shell".into(),
        DispatchReservation {
            tokens: 600,
            cost_microunits: 60,
        },
        100,
    )
    .unwrap()
}

fn intent(
    prepared: &DispatchPreparation,
    id: &str,
    target: ActivationRef,
) -> InvocationIntentCommand {
    InvocationIntentCommand {
        command_id: CommandId::new(format!("intent-{id}")).unwrap(),
        expected_revision: 0,
        intent: InvocationIntent {
            invocation_id: InvocationId::new(id).unwrap(),
            activation: target,
            dispatch_scope: prepared.dispatch_scope().into(),
            tool_name: "shell".into(),
            arguments: ProtectedArguments {
                evidence_ref: EvidenceRef::new(format!("args-{id}")).unwrap(),
                sha256: "a".repeat(64),
                byte_len: 128,
            },
            redacted_preview: "run repository checks".into(),
            authority: InvocationAuthority {
                grant_id: "grant-qa".into(),
                grant_revision: 1,
                approval_ref: None,
            },
            replay_policy: InvocationReplayPolicy::ManualOnly,
            provider_replay: ProviderReplayIdentity {
                adapter_id: "native".into(),
                adapter_version: "test-1".into(),
                provider_run_ref: None,
                native_call_id: None,
                response_group_id: None,
            },
        },
    }
}

fn record_outcome(audit: &mut InvocationAudit, command: &InvocationIntentCommand) {
    audit
        .record_evidence(InvocationEvidenceCommand {
            command_id: CommandId::new(format!(
                "outcome-{}",
                command.intent.invocation_id.as_str()
            ))
            .unwrap(),
            expected_revision: 1,
            invocation_id: command.intent.invocation_id.clone(),
            activation: command.intent.activation.clone(),
            authority: command.intent.authority.clone(),
            evidence: InvocationFinalEvidence::Outcome {
                outcome: InvocationOutcome::Failed,
                result: ProtectedArguments {
                    evidence_ref: EvidenceRef::new("check-results").unwrap(),
                    sha256: "b".repeat(64),
                    byte_len: 64,
                },
                redacted_preview: "checks failed after writing report".into(),
                source: InvocationOutcomeSource::Executor,
                authority_ref: EvidenceRef::new("executor-observation").unwrap(),
            },
        })
        .unwrap();
}

#[test]
fn durable_intent_is_necessary_but_stop_and_revocation_still_win() {
    let root = TempDir::new().unwrap();
    let audit_root = TempDir::new().unwrap();
    let gate = authority(&root);
    let mut audit = audit(&audit_root);
    let lease = ready(&gate, "tester");
    let prepared = prepare(&gate, &lease, "invocation-1");
    let command = intent(
        &prepared,
        "invocation-1",
        activation("tester", 1, "epoch-1"),
    );
    let receipt = audit.record_intent(command).unwrap();
    gate.stop_activation(receipt.activation(), gate.revision().unwrap())
        .unwrap();
    assert!(gate
        .claim_dispatch(prepared, &receipt, &audit, 100)
        .is_err());
    assert_eq!(gate.usage("grant-qa").unwrap().invocations, 0);
    assert_eq!(
        audit
            .invocation(receipt.invocation_id())
            .unwrap()
            .unwrap()
            .disposition(),
        EffectDisposition::OutcomeUnknown
    );
    let next = gate
        .register_activation(
            activation("tester", 2, "epoch-1"),
            "grant-qa",
            profile(),
            gate.revision().unwrap(),
            100,
        )
        .unwrap();
    let prepared = prepare(&gate, &next, "invocation-2");
    let receipt = audit
        .record_intent(intent(
            &prepared,
            "invocation-2",
            activation("tester", 2, "epoch-1"),
        ))
        .unwrap();
    gate.revoke_grant("grant-qa", gate.revision().unwrap())
        .unwrap();
    assert!(gate
        .claim_dispatch(prepared, &receipt, &audit, 100)
        .is_err());
    assert!(gate
        .install_grant(grant(), gate.revision().unwrap())
        .is_err());
}

#[test]
fn charges_survive_restart_and_unknown_claim_blocks_new_epoch() {
    let root = TempDir::new().unwrap();
    let audit_root = TempDir::new().unwrap();
    let gate = authority(&root);
    let mut audit = audit(&audit_root);
    let lease = ready(&gate, "tester");
    let prepared = prepare(&gate, &lease, "invocation-1");
    let command = intent(
        &prepared,
        "invocation-1",
        activation("tester", 1, "epoch-1"),
    );
    let receipt = audit.record_intent(command.clone()).unwrap();
    gate.claim_dispatch(prepared, &receipt, &audit, 100)
        .unwrap();
    drop(gate);
    let gate = authority(&root);
    assert_eq!(
        gate.usage("grant-qa").unwrap(),
        GrantUsage {
            activations: 1,
            invocations: 1,
            tokens: 600,
            cost_microunits: 60
        }
    );
    assert!(gate
        .register_activation(
            activation("reviewer", 1, "epoch-2"),
            "grant-qa",
            profile(),
            gate.revision().unwrap(),
            100
        )
        .is_err());
    assert!(gate
        .prepare_dispatch(
            &lease,
            InvocationId::new("new-id").unwrap(),
            "shell".into(),
            DispatchReservation {
                tokens: 1,
                cost_microunits: 1
            },
            100
        )
        .is_err());
    assert!(gate
        .settle_dispatch(receipt.invocation_id(), &audit, gate.revision().unwrap())
        .is_err());
    record_outcome(&mut audit, &command);
    gate.settle_dispatch(receipt.invocation_id(), &audit, gate.revision().unwrap())
        .unwrap();
    let next = gate
        .register_activation(
            activation("tester", 2, "epoch-2"),
            "grant-qa",
            profile(),
            gate.revision().unwrap(),
            100,
        )
        .unwrap();
    assert!(gate
        .prepare_dispatch(
            &next,
            InvocationId::new("invocation-2").unwrap(),
            "shell".into(),
            DispatchReservation {
                tokens: 600,
                cost_microunits: 60
            },
            100
        )
        .is_err());
    assert_eq!(gate.usage("grant-qa").unwrap().tokens, 600);
}

#[test]
fn an_acknowledgement_and_claim_are_never_replay_authority() {
    let root = TempDir::new().unwrap();
    let audit_root = TempDir::new().unwrap();
    let gate = authority(&root);
    let mut audit = audit(&audit_root);
    let lease = ready(&gate, "tester");
    let first = prepare(&gate, &lease, "invocation-1");
    let second = prepare(&gate, &lease, "invocation-1");
    let command = intent(&first, "invocation-1", activation("tester", 1, "epoch-1"));
    let receipt = audit.record_intent(command.clone()).unwrap();
    let repeated = audit.record_intent(command).unwrap();
    gate.claim_dispatch(first, &receipt, &audit, 100).unwrap();
    assert!(gate.claim_dispatch(second, &repeated, &audit, 100).is_err());
    assert_eq!(gate.usage("grant-qa").unwrap().invocations, 1);
}

#[test]
fn reloaded_unclaimed_intent_does_not_fit_a_new_live_scope() {
    let root = TempDir::new().unwrap();
    let audit_root = TempDir::new().unwrap();
    let gate = authority(&root);
    let mut audit = audit(&audit_root);
    let lease = ready(&gate, "tester");
    let prepared = prepare(&gate, &lease, "invocation-1");
    let command = intent(
        &prepared,
        "invocation-1",
        activation("tester", 1, "epoch-1"),
    );
    let receipt = audit.record_intent(command.clone()).unwrap();
    drop(gate);
    let gate = authority(&root);
    assert!(gate
        .claim_dispatch(prepared, &receipt, &audit, 100)
        .is_err());
    let next = gate
        .register_activation(
            activation("tester", 2, "epoch-2"),
            "grant-qa",
            profile(),
            gate.revision().unwrap(),
            100,
        )
        .unwrap();
    let prepared = prepare(&gate, &next, "invocation-1");
    let mut new_command = command;
    new_command.intent.dispatch_scope = prepared.dispatch_scope().into();
    assert!(audit.record_intent(new_command).is_err());
}

#[test]
fn foreign_resolved_and_retargeted_receipts_are_rejected_without_charge() {
    let root = TempDir::new().unwrap();
    let audit_root = TempDir::new().unwrap();
    let other_root = TempDir::new().unwrap();
    let gate = authority(&root);
    let mut journal = audit(&audit_root);
    let other = audit(&other_root);
    let lease = ready(&gate, "tester");
    let prepared = prepare(&gate, &lease, "invocation-1");
    let command = intent(
        &prepared,
        "invocation-1",
        activation("tester", 1, "epoch-1"),
    );
    let receipt = journal.record_intent(command.clone()).unwrap();
    assert!(gate
        .claim_dispatch(prepared, &receipt, &other, 100)
        .is_err());
    let prepared = prepare(&gate, &lease, "invocation-1");
    record_outcome(&mut journal, &command);
    assert!(gate
        .claim_dispatch(prepared, &receipt, &journal, 100)
        .is_err());
    let prepared = prepare(&gate, &lease, "invocation-2");
    let wrong = journal
        .record_intent(intent(
            &prepared,
            "invocation-2",
            activation("reviewer", 1, "epoch-1"),
        ))
        .unwrap();
    assert!(gate
        .claim_dispatch(prepared, &wrong, &journal, 100)
        .is_err());
    assert_eq!(gate.usage("grant-qa").unwrap().invocations, 0);
}

#[cfg(unix)]
#[test]
fn owned_dispatch_requires_the_same_canonical_journal_even_when_all_logical_ids_match() {
    let root = TempDir::new().unwrap();
    let foreign_root = TempDir::new().unwrap();
    let session = owned_session(&root);
    let foreign_session = owned_session(&foreign_root);
    assert_eq!(session.owner(), foreign_session.owner());
    assert_ne!(
        session.identity().unwrap(),
        foreign_session.identity().unwrap()
    );
    let gate = owned_authority(&session);
    let mut journal = owned_audit(&session);
    let mut foreign = owned_audit(&foreign_session);
    let lease = ready(&gate, "tester");
    let prepared = prepare(&gate, &lease, "invocation-1");
    let command = intent(
        &prepared,
        "invocation-1",
        activation("tester", 1, "epoch-1"),
    );
    // This is a real durable receipt with the exact current scope and payload,
    // but admitting it would hide this Session's dispatch from its own audit.
    let foreign_receipt = foreign.record_intent(command.clone()).unwrap();
    assert!(matches!(
        gate.claim_dispatch(prepared, &foreign_receipt, &foreign, 100),
        Err(AuthorityError::Denied)
    ));
    assert_eq!(gate.usage("grant-qa").unwrap().invocations, 0);

    let prepared = prepare(&gate, &lease, "invocation-1");
    let receipt = journal.record_intent(command.clone()).unwrap();
    gate.claim_dispatch(prepared, &receipt, &journal, 100)
        .unwrap();
    record_outcome(&mut foreign, &command);
    assert!(matches!(
        gate.settle_dispatch(
            &InvocationId::new("invocation-1").unwrap(),
            &foreign,
            gate.revision().unwrap()
        ),
        Err(AuthorityError::Denied)
    ));
    record_outcome(&mut journal, &command);
    gate.settle_dispatch(
        &InvocationId::new("invocation-1").unwrap(),
        &journal,
        gate.revision().unwrap(),
    )
    .unwrap();
    assert_eq!(gate.usage("grant-qa").unwrap().invocations, 1);
}

#[cfg(unix)]
#[test]
fn owned_and_isolated_authority_audit_pairs_cannot_authorize_dispatch() {
    for owned_gate in [true, false] {
        let root = TempDir::new().unwrap();
        let isolated = TempDir::new().unwrap();
        let session = owned_session(&root);
        let gate = if owned_gate {
            owned_authority(&session)
        } else {
            authority(&isolated)
        };
        let mut journal = if owned_gate {
            audit(&isolated)
        } else {
            owned_audit(&session)
        };
        let lease = ready(&gate, "tester");
        let prepared = prepare(&gate, &lease, "invocation-1");
        let receipt = journal
            .record_intent(intent(
                &prepared,
                "invocation-1",
                activation("tester", 1, "epoch-1"),
            ))
            .unwrap();
        assert!(matches!(
            gate.claim_dispatch(prepared, &receipt, &journal, 100),
            Err(AuthorityError::Denied)
        ));
        assert_eq!(gate.usage("grant-qa").unwrap().invocations, 0);
    }
}

#[test]
fn only_the_granted_supervisor_can_stop_exact_descendants() {
    let root = TempDir::new().unwrap();
    let gate = authority(&root);
    let supervisor = ready(&gate, "supervisor");
    let worker = ready(&gate, "tester");
    let tester = activation("tester", 1, "epoch-1");
    let leader = activation("supervisor", 1, "epoch-1");
    assert!(gate
        .stop_descendant(&worker, &leader, gate.revision().unwrap(), 100)
        .is_err());
    assert!(gate
        .stop_descendant(&supervisor, &leader, gate.revision().unwrap(), 100)
        .is_err());
    let mut wrong = tester.clone();
    wrong.turn_id = LogicalTurnId::new("other-turn").unwrap();
    assert!(gate
        .stop_descendant(&supervisor, &wrong, gate.revision().unwrap(), 100)
        .is_err());
    gate.stop_descendant(&supervisor, &tester, gate.revision().unwrap(), 100)
        .unwrap();
    assert!(gate
        .prepare_dispatch(
            &worker,
            InvocationId::new("invocation-1").unwrap(),
            "shell".into(),
            DispatchReservation {
                tokens: 1,
                cost_microunits: 1
            },
            100
        )
        .is_err());
    gate.register_activation(
        activation("tester", 2, "epoch-1"),
        "grant-qa",
        profile(),
        gate.revision().unwrap(),
        100,
    )
    .unwrap();
    assert!(gate
        .stop_activation(&tester, gate.revision().unwrap())
        .is_err());
}

#[test]
fn a_different_audit_cannot_settle_an_unknown_claim_with_reused_identities() {
    let root = TempDir::new().unwrap();
    let first_root = TempDir::new().unwrap();
    let second_root = TempDir::new().unwrap();
    let gate = authority(&root);
    let mut first = audit(&first_root);
    let mut second = audit(&second_root);
    let lease = ready(&gate, "tester");
    let prepared = prepare(&gate, &lease, "invocation-1");
    let command = intent(
        &prepared,
        "invocation-1",
        activation("tester", 1, "epoch-1"),
    );
    let receipt = first.record_intent(command.clone()).unwrap();
    gate.claim_dispatch(prepared, &receipt, &first, 100)
        .unwrap();
    gate.stop_activation(receipt.activation(), gate.revision().unwrap())
        .unwrap();
    let mut substituted = command.clone();
    substituted.intent.arguments.sha256 = "c".repeat(64);
    second.record_intent(substituted.clone()).unwrap();
    record_outcome(&mut second, &substituted);
    assert!(gate
        .settle_dispatch(receipt.invocation_id(), &second, gate.revision().unwrap())
        .is_err());
    assert!(gate
        .register_activation(
            activation("tester", 2, "epoch-2"),
            "grant-qa",
            profile(),
            gate.revision().unwrap(),
            100
        )
        .is_err());
    record_outcome(&mut first, &command);
    gate.settle_dispatch(receipt.invocation_id(), &first, gate.revision().unwrap())
        .unwrap();
}

#[test]
fn narrowing_preserves_history_usage_and_invalidates_prepared_dispatch() {
    let root = TempDir::new().unwrap();
    let audit_root = TempDir::new().unwrap();
    let gate = authority(&root);
    let mut audit = audit(&audit_root);
    let lease = ready(&gate, "tester");
    let prepared = prepare(&gate, &lease, "invocation-1");
    let receipt = audit
        .record_intent(intent(
            &prepared,
            "invocation-1",
            activation("tester", 1, "epoch-1"),
        ))
        .unwrap();
    let mut policy = grant();
    policy.revision = 2;
    policy.limits.tokens = 400;
    policy.profiles[0].tools = vec!["read_file".into()];
    gate.narrow_grant(policy.clone(), gate.revision().unwrap())
        .unwrap();
    assert!(gate
        .claim_dispatch(prepared, &receipt, &audit, 100)
        .is_err());
    let mut expansion = policy;
    expansion.revision = 3;
    expansion.limits.tokens = 401;
    assert!(gate
        .narrow_grant(expansion, gate.revision().unwrap())
        .is_err());
    drop(gate);
    let reopened = authority(&root);
    assert_eq!(reopened.usage("grant-qa").unwrap().activations, 1);
    let data: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root.path().join("control-authority.v1.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(data["grants"][0]["previous_policies"][0]["revision"], 1);
    assert_eq!(data["grants"][0]["policy"]["revision"], 2);
}

#[test]
fn current_expiry_profile_and_budget_are_rechecked_at_dispatch() {
    let root = TempDir::new().unwrap();
    let audit_root = TempDir::new().unwrap();
    let gate = authority(&root);
    let mut audit = audit(&audit_root);
    let lease = ready(&gate, "tester");
    let prepared = prepare(&gate, &lease, "invocation-1");
    let receipt = audit
        .record_intent(intent(
            &prepared,
            "invocation-1",
            activation("tester", 1, "epoch-1"),
        ))
        .unwrap();
    assert!(gate
        .claim_dispatch(prepared, &receipt, &audit, 1000)
        .is_err());
    assert!(gate
        .prepare_dispatch(
            &lease,
            InvocationId::new("x").unwrap(),
            "undeclared-tool".into(),
            DispatchReservation {
                tokens: 0,
                cost_microunits: 0
            },
            100
        )
        .is_err());
    let mut unsupported = profile();
    unsupported.model = "more-expensive".into();
    assert!(gate
        .register_activation(
            activation("reviewer", 1, "epoch-1"),
            "grant-qa",
            unsupported,
            gate.revision().unwrap(),
            100
        )
        .is_err());
    let one = prepare(&gate, &lease, "one");
    let two = prepare(&gate, &lease, "two");
    let first = audit
        .record_intent(intent(&one, "one", activation("tester", 1, "epoch-1")))
        .unwrap();
    let second = audit
        .record_intent(intent(&two, "two", activation("tester", 1, "epoch-1")))
        .unwrap();
    gate.claim_dispatch(one, &first, &audit, 100).unwrap();
    assert!(gate.claim_dispatch(two, &second, &audit, 100).is_err());
}

#[test]
fn stop_and_claim_have_a_single_order_under_concurrency() {
    let root = TempDir::new().unwrap();
    let audit_root = TempDir::new().unwrap();
    let gate = Arc::new(authority(&root));
    let mut audit = audit(&audit_root);
    let lease = ready(&gate, "tester");
    let prepared = prepare(&gate, &lease, "invocation-1");
    let receipt = audit
        .record_intent(intent(
            &prepared,
            "invocation-1",
            activation("tester", 1, "epoch-1"),
        ))
        .unwrap();
    let barrier = Barrier::new(2);
    std::thread::scope(|scope| {
        let claim = scope.spawn(|| {
            barrier.wait();
            gate.claim_dispatch(prepared, &receipt, &audit, 100)
        });
        barrier.wait();
        let revision = gate.revision().unwrap();
        if gate
            .stop_activation(receipt.activation(), revision)
            .is_err()
        {
            gate.stop_activation(receipt.activation(), gate.revision().unwrap())
                .unwrap();
        }
        let claimed = claim.join().unwrap().is_ok();
        assert_eq!(
            gate.usage("grant-qa").unwrap().invocations,
            u32::from(claimed)
        );
    });
    assert!(gate
        .prepare_dispatch(
            &lease,
            InvocationId::new("later").unwrap(),
            "shell".into(),
            DispatchReservation {
                tokens: 1,
                cost_microunits: 1
            },
            100
        )
        .is_err());
}

#[test]
fn late_failed_outcome_settles_after_closure_without_reopening_or_refund() {
    let root = TempDir::new().unwrap();
    let audit_root = TempDir::new().unwrap();
    let gate = authority(&root);
    let mut audit = audit(&audit_root);
    let lease = ready(&gate, "tester");
    let prepared = prepare(&gate, &lease, "invocation-1");
    let command = intent(
        &prepared,
        "invocation-1",
        activation("tester", 1, "epoch-1"),
    );
    let receipt = audit.record_intent(command.clone()).unwrap();
    gate.claim_dispatch(prepared, &receipt, &audit, 100)
        .unwrap();
    gate.close_dispatch(gate.revision().unwrap()).unwrap();
    record_outcome(&mut audit, &command);
    gate.settle_dispatch(receipt.invocation_id(), &audit, gate.revision().unwrap())
        .unwrap();
    assert_eq!(gate.usage("grant-qa").unwrap().tokens, 600);
    assert!(gate
        .register_activation(
            activation("tester", 2, "epoch-2"),
            "grant-qa",
            profile(),
            gate.revision().unwrap(),
            100
        )
        .is_err());
}

#[test]
fn owner_mismatch_unknown_schema_and_counter_tampering_fail_closed() {
    let root = TempDir::new().unwrap();
    let gate = authority(&root);
    ready(&gate, "tester");
    drop(gate);
    assert!(ControlAuthority::open(
        root.path(),
        SessionId::new("another-session").unwrap(),
        LogicalTurnId::new("turn-build-184").unwrap()
    )
    .is_err());
    let path = root.path().join("control-authority.v1.json");
    let bytes = std::fs::read(&path).unwrap();
    let mut data: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    data["schema_version"] = 99.into();
    std::fs::write(&path, serde_json::to_vec(&data).unwrap()).unwrap();
    assert!(ControlAuthority::open(
        root.path(),
        SessionId::new("session-qa").unwrap(),
        LogicalTurnId::new("turn-build-184").unwrap()
    )
    .is_err());
    data["schema_version"] = 1.into();
    data["grants"][0]["usage"]["activations"] = 0.into();
    std::fs::write(&path, serde_json::to_vec(&data).unwrap()).unwrap();
    assert!(ControlAuthority::open(
        root.path(),
        SessionId::new("session-qa").unwrap(),
        LogicalTurnId::new("turn-build-184").unwrap()
    )
    .is_err());
}

#[test]
#[cfg(unix)]
fn competing_writer_and_failed_stop_persistence_cannot_leave_dispatch_enabled() {
    use std::os::unix::fs::symlink;
    let root = TempDir::new().unwrap();
    let gate = authority(&root);
    let lease = ready(&gate, "tester");
    assert!(ControlAuthority::open(
        root.path(),
        SessionId::new("session-qa").unwrap(),
        LogicalTurnId::new("turn-build-184").unwrap()
    )
    .is_err());
    let path = root.path().join("control-authority.v1.json");
    let bytes = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    symlink(root.path().join("forbidden"), &path).unwrap();
    assert!(gate
        .stop_activation(
            &activation("tester", 1, "epoch-1"),
            gate.revision().unwrap()
        )
        .is_err());
    assert!(matches!(
        gate.prepare_dispatch(
            &lease,
            InvocationId::new("later").unwrap(),
            "shell".into(),
            DispatchReservation {
                tokens: 1,
                cost_microunits: 1
            },
            100
        ),
        Err(AuthorityError::RecoveryRequired)
    ));
    drop(gate);
    std::fs::remove_file(&path).unwrap();
    std::fs::write(&path, bytes).unwrap();
    let recovered = authority(&root);
    assert!(recovered
        .prepare_dispatch(
            &lease,
            InvocationId::new("later").unwrap(),
            "shell".into(),
            DispatchReservation {
                tokens: 1,
                cost_microunits: 1
            },
            100
        )
        .is_err());
}

#[test]
fn counter_consistent_but_impossible_generation_history_is_rejected() {
    let root = TempDir::new().unwrap();
    let gate = authority(&root);
    ready(&gate, "tester");
    drop(gate);
    let path = root.path().join("control-authority.v1.json");
    let original: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    for (generation, prior_stopped) in [(1, true), (2, false)] {
        let mut data = original.clone();
        data["activations"][0]["stopped"] = prior_stopped.into();
        let mut duplicate = data["activations"][0].clone();
        duplicate["activation"]["activation_id"] = "different-activation-id".into();
        duplicate["activation"]["generation"] = generation.into();
        data["activations"].as_array_mut().unwrap().push(duplicate);
        data["grants"][0]["usage"]["activations"] = 2.into();
        std::fs::write(&path, serde_json::to_vec(&data).unwrap()).unwrap();
        assert!(ControlAuthority::open(
            root.path(),
            SessionId::new("session-qa").unwrap(),
            LogicalTurnId::new("turn-build-184").unwrap()
        )
        .is_err());
    }
}

fn ready_provider(gate: &ControlAuthority, node: &str) -> ActivationLease {
    gate.install_grant(grant(), gate.revision().unwrap())
        .unwrap();
    gate.register_provider_activation(
        activation(node, 1, "epoch-1"),
        "grant-qa",
        profile(),
        gate.revision().unwrap(),
        100,
    )
    .unwrap()
}

fn provider_intent(id: &str) -> ProviderCallIntent {
    ProviderCallIntent {
        call_id: id.into(),
        provider: "test-provider".into(),
        model: "test-model".into(),
        request_sha256: "b".repeat(64),
        request_bytes: 128,
        reservation: DispatchReservation {
            tokens: 100,
            cost_microunits: 10,
        },
        max_response_bytes: 1024,
    }
}
fn provider_outcome() -> ProviderCallOutcome {
    ProviderCallOutcome {
        kind: ProviderCallTerminal::Completed,
        usage: axocoatl_core::MeasuredTokenUsage::known(
            axocoatl_core::TokenUsageStats::new(12, 8).with_reasoning(3),
        ),
        cost_microunits: Some(2),
        cost_known: true,
    }
}

#[test]
fn provider_coverage_requires_explicit_gated_registration_and_exact_current_profile_bounds() {
    let root = TempDir::new().unwrap();
    let gate = authority(&root);
    let old = ready(&gate, "tester");
    assert!(gate
        .provider_usage(&activation("tester", 1, "epoch-1"))
        .is_err());
    assert!(gate
        .claim_provider_call(&old, provider_intent("old-ungated"), 100)
        .is_err());
    let lease = ready_provider(&gate, "reviewer");
    let usage = gate
        .provider_usage(&activation("reviewer", 1, "epoch-1"))
        .unwrap();
    assert_eq!(
        usage.tokens,
        axocoatl_core::MeasuredTokenUsage::known(Default::default())
    );
    assert_eq!(usage.calls, 0);
    assert!(usage.cost_known);
    assert!(gate
        .provider_usage(&activation("missing", 1, "epoch-1"))
        .is_err());
    for case in [
        "provider",
        "model",
        "digest",
        "zero-tokens",
        "zero-request",
        "large-request",
        "zero-response",
        "large-response",
        "expired",
    ] {
        let mut intent = provider_intent(case);
        match case {
            "provider" => intent.provider = "other".into(),
            "model" => intent.model = "other".into(),
            "digest" => intent.request_sha256 = "bad".into(),
            "zero-tokens" => intent.reservation.tokens = 0,
            "zero-request" => intent.request_bytes = 0,
            "large-request" => intent.request_bytes = MAX_PROVIDER_REQUEST_BYTES + 1,
            "zero-response" => intent.max_response_bytes = 0,
            "large-response" => intent.max_response_bytes = MAX_PROVIDER_RESPONSE_BYTES + 1,
            "expired" => {}
            _ => unreachable!(),
        }
        assert!(
            gate.claim_provider_call(&lease, intent, if case == "expired" { 1000 } else { 100 })
                .is_err(),
            "{case}"
        );
    }
    assert_eq!(gate.usage("grant-qa").unwrap().invocations, 0);
}

#[test]
fn lost_provider_claim_ack_is_unknown_after_reopen_and_never_reauthorizes_dispatch() {
    let root = TempDir::new().unwrap();
    let gate = authority(&root);
    let lease = ready_provider(&gate, "tester");
    // Intentionally discard the successful acknowledgement, as a controller
    // restart can do after durable publication and before dispatch observation.
    let _lost = gate
        .claim_provider_call(&lease, provider_intent("call-1"), 100)
        .unwrap();
    assert!(gate
        .claim_provider_call(&lease, provider_intent("call-1"), 100)
        .is_err());
    drop(gate);
    let gate = authority(&root);
    let usage = gate
        .provider_usage(&activation("tester", 1, "epoch-1"))
        .unwrap();
    assert_eq!(usage.calls, 1);
    assert_eq!(usage.unsettled_calls, 1);
    assert!(!usage.tokens.complete);
    assert!(!usage.cost_known);
    assert_eq!(gate.usage("grant-qa").unwrap().tokens, 100);
    assert!(gate
        .claim_provider_call(&lease, provider_intent("call-1"), 100)
        .is_err());
    assert!(gate
        .register_provider_activation(
            activation("tester", 2, "epoch-1"),
            "grant-qa",
            profile(),
            gate.revision().unwrap(),
            100
        )
        .is_err());
    assert!(gate
        .register_provider_activation(
            activation("reviewer", 1, "epoch-2"),
            "grant-qa",
            profile(),
            gate.revision().unwrap(),
            100
        )
        .is_err());
    let receipt = gate.provider_settlement_receipt("call-1").unwrap();
    let mut observed = provider_outcome();
    observed.kind = ProviderCallTerminal::Interrupted;
    observed.usage.complete = false;
    observed.cost_known = false;
    gate.reconcile_provider_call(&receipt, &observed).unwrap();
    gate.reconcile_provider_call(&receipt, &observed).unwrap();
    assert!(gate
        .reconcile_provider_call(&receipt, &provider_outcome())
        .is_err());
    let next = gate
        .register_provider_activation(
            activation("tester", 2, "epoch-1"),
            "grant-qa",
            profile(),
            gate.revision().unwrap(),
            100,
        )
        .unwrap();
    assert!(gate
        .claim_provider_call(&next, provider_intent("call-1"), 100)
        .is_err());
    let usage = gate
        .provider_usage(&activation("tester", 1, "epoch-1"))
        .unwrap();
    assert_eq!(usage.tokens.usage, observed.usage.usage);
    assert!(!usage.tokens.complete);
    assert_eq!(usage.unsettled_calls, 0);
    assert_eq!(gate.usage("grant-qa").unwrap().cost_microunits, 10);
}

#[test]
fn provider_and_tool_claims_charge_the_same_grant_without_refunding_measured_usage() {
    let root = TempDir::new().unwrap();
    let audit_root = TempDir::new().unwrap();
    let gate = authority(&root);
    let mut audit = audit(&audit_root);
    let lease = ready_provider(&gate, "tester");
    let prepared = prepare(&gate, &lease, "tool-1");
    let receipt = audit
        .record_intent(intent(
            &prepared,
            "tool-1",
            activation("tester", 1, "epoch-1"),
        ))
        .unwrap();
    gate.claim_dispatch(prepared, &receipt, &audit, 100)
        .unwrap();
    let mut excessive = provider_intent("over-budget");
    excessive.reservation.tokens = 401;
    assert!(matches!(
        gate.claim_provider_call(&lease, excessive, 100),
        Err(AuthorityError::Capacity)
    ));
    let mut exact = provider_intent("provider-1");
    exact.reservation = DispatchReservation {
        tokens: 400,
        cost_microunits: 40,
    };
    let claim = gate.claim_provider_call(&lease, exact, 100).unwrap();
    assert_eq!(claim.activation(), &activation("tester", 1, "epoch-1"));
    assert_eq!(claim.intent().request_sha256, "b".repeat(64));
    gate.settle_provider_call(&claim, &provider_outcome())
        .unwrap();
    assert_eq!(
        gate.usage("grant-qa").unwrap(),
        GrantUsage {
            activations: 1,
            invocations: 2,
            tokens: 1000,
            cost_microunits: 100
        }
    );
    assert!(matches!(
        gate.claim_provider_call(&lease, provider_intent("provider-2"), 100),
        Err(AuthorityError::Capacity)
    ));
    let usage = gate.provider_usage(claim.activation()).unwrap();
    assert_eq!(usage.tokens, provider_outcome().usage);
    assert_eq!(usage.cost_microunits, 2);
}

#[test]
fn provider_stop_race_has_one_durable_order_and_all_late_observations_are_retained() {
    for _ in 0..8 {
        let root = TempDir::new().unwrap();
        let gate = Arc::new(authority(&root));
        let lease = ready_provider(&gate, "tester");
        let barrier = Arc::new(Barrier::new(2));
        let dispatch_gate = gate.clone();
        let dispatch_barrier = barrier.clone();
        let worker = std::thread::spawn(move || {
            dispatch_barrier.wait();
            dispatch_gate.claim_provider_call(&lease, provider_intent("race"), 100)
        });
        barrier.wait();
        // Retrying an optimistic revision conflict still serializes under the
        // same gate; either the one claim or Stop is durably first.
        loop {
            let revision = gate.revision().unwrap();
            match gate.stop_activation(&activation("tester", 1, "epoch-1"), revision) {
                Ok(()) => break,
                Err(AuthorityError::Invalid("stale authority revision")) => continue,
                other => panic!("unexpected Stop result: {other:?}"),
            }
        }
        let result = worker.join().unwrap();
        if let Ok(claim) = result {
            assert_eq!(gate.usage("grant-qa").unwrap().invocations, 1);
            gate.close_dispatch(gate.revision().unwrap()).unwrap();
            let mut outcome = provider_outcome();
            outcome.kind = ProviderCallTerminal::Failed;
            gate.settle_provider_call(&claim, &outcome).unwrap();
            gate.settle_provider_call(&claim, &outcome).unwrap();
            assert_eq!(
                gate.provider_call(claim.call_id())
                    .unwrap()
                    .unwrap()
                    .outcome,
                Some(outcome)
            );
            assert_eq!(gate.usage("grant-qa").unwrap().tokens, 100);
        } else {
            assert_eq!(gate.usage("grant-qa").unwrap().invocations, 0);
            assert!(gate.provider_call("race").unwrap().is_none());
        }
    }
}

#[test]
fn observed_provider_overruns_are_retained_and_prevent_additional_dispatch() {
    let root = TempDir::new().unwrap();
    let gate = authority(&root);
    let lease = ready_provider(&gate, "tester");
    let claim = gate
        .claim_provider_call(&lease, provider_intent("overrun"), 100)
        .unwrap();
    let mut observed = provider_outcome();
    observed.usage.usage.input_tokens = 1000;
    observed.cost_microunits = Some(50);
    gate.settle_provider_call(&claim, &observed).unwrap();
    assert_eq!(
        gate.provider_usage(claim.activation()).unwrap().tokens,
        observed.usage
    );
    assert_eq!(gate.usage("grant-qa").unwrap().tokens, 100);
    assert!(gate
        .claim_provider_call(&lease, provider_intent("next"), 100)
        .is_err());
    assert!(gate
        .prepare_dispatch(
            &lease,
            InvocationId::new("tool-after-overrun").unwrap(),
            "shell".into(),
            DispatchReservation {
                tokens: 1,
                cost_microunits: 1
            },
            100
        )
        .is_err());
    drop(gate);
    let gate = authority(&root);
    assert_eq!(
        gate.provider_usage(claim.activation()).unwrap().tokens,
        observed.usage
    );
    assert!(gate
        .register_provider_activation(
            activation("tester", 2, "epoch-1"),
            "grant-qa",
            profile(),
            gate.revision().unwrap(),
            100
        )
        .is_err());
}

#[test]
#[cfg(unix)]
fn provider_settlement_cannot_cross_owned_journals_and_reopen_validates_counters_and_coverage() {
    let root = TempDir::new().unwrap();
    let foreign_root = TempDir::new().unwrap();
    let session = owned_session(&root);
    let foreign_session = owned_session(&foreign_root);
    let gate = owned_authority(&session);
    let foreign = owned_authority(&foreign_session);
    let lease = ready_provider(&gate, "tester");
    let other_lease = ready_provider(&foreign, "tester");
    let claim = gate
        .claim_provider_call(&lease, provider_intent("same-id"), 100)
        .unwrap();
    foreign
        .claim_provider_call(&other_lease, provider_intent("same-id"), 100)
        .unwrap();
    assert!(foreign
        .settle_provider_call(&claim, &provider_outcome())
        .is_err());
    assert!(foreign
        .provider_call("same-id")
        .unwrap()
        .unwrap()
        .outcome
        .is_none());
    drop(gate);
    for case in ["count", "provider", "gated", "grant", "unknown-field"] {
        let root = TempDir::new().unwrap();
        let gate = authority(&root);
        let lease = ready_provider(&gate, "tester");
        gate.claim_provider_call(&lease, provider_intent("corrupt"), 100)
            .unwrap();
        drop(gate);
        let path = root.path().join("control-authority.v1.json");
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        match case {
            "count" => value["grants"][0]["usage"]["invocations"] = 0.into(),
            "provider" => value["provider_calls"][0]["intent"]["provider"] = "foreign".into(),
            "gated" => value["activations"][0]["provider_gated"] = false.into(),
            "grant" => value["provider_calls"][0]["grant_revision"] = 2.into(),
            "unknown-field" => value["provider_calls"][0]["unsupported"] = true.into(),
            _ => unreachable!(),
        }
        let bytes = serde_json::to_vec(&value).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        assert!(
            ControlAuthority::open(
                root.path(),
                SessionId::new("session-qa").unwrap(),
                LogicalTurnId::new("turn-build-184").unwrap()
            )
            .is_err(),
            "{case}"
        );
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }
}

#[cfg(unix)]
#[test]
fn historical_provider_usage_requires_existing_closed_gates_and_preserves_journal() {
    use axocoatl_session::execution_namespace::ExecutionComponent;
    let root = TempDir::new().unwrap();
    let session = owned_session(&root);
    let turn = LogicalTurnId::new("turn-build-184").unwrap();
    let namespace = || {
        session
            .component_namespace(ExecutionComponent::ControlAuthority {
                turn_id: turn.clone(),
            })
            .unwrap()
    };
    assert!(ControlAuthority::read_provider_usage_owned(namespace(), &[]).is_err());
    assert!(!namespace().is_file("control-authority.v1.json").unwrap());
    let gate = owned_authority(&session);
    let lease = ready_provider(&gate, "tester");
    let target = activation("tester", 1, "epoch-1");
    let claim = gate
        .claim_provider_call(&lease, provider_intent("historical"), 100)
        .unwrap();
    gate.settle_provider_call(&claim, &provider_outcome())
        .unwrap();
    drop(gate);
    let before = namespace()
        .read_limited("control-authority.v1.json", 8 * 1024 * 1024)
        .unwrap();
    assert!(ControlAuthority::read_provider_usage_owned(
        namespace(),
        std::slice::from_ref(&target)
    )
    .is_err());
    assert_eq!(
        namespace()
            .read_limited("control-authority.v1.json", 8 * 1024 * 1024)
            .unwrap(),
        before
    );
    // Ordinary recovery explicitly closes old generation gates. Historical
    // inspection itself must not perform that transition.
    let gate = owned_authority(&session);
    let expected = gate.provider_usage(&target).unwrap();
    drop(gate);
    let before = namespace()
        .read_limited("control-authority.v1.json", 8 * 1024 * 1024)
        .unwrap();
    let actual = ControlAuthority::read_provider_usage_owned(namespace(), &[target]).unwrap();
    assert_eq!(actual, expected);
    assert_eq!(
        namespace()
            .read_limited("control-authority.v1.json", 8 * 1024 * 1024)
            .unwrap(),
        before
    );
}
