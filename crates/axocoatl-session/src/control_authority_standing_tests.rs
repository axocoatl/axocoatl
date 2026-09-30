use super::*;
use crate::execution_content::{
    ActivationOutputContent, ConditionOutputCapture, ConditionProcessStatus, ExecutionUsage,
    OutputKind,
};
use crate::execution_ownership::LegacyFormatOwnership;
use crate::execution_store::ExecutionStoreOwner;
use crate::team_work::{TeamWorkGrantAllocation, TeamWorkGrantReference};
use crate::turn_contract::*;
use std::sync::Arc;

#[test]
fn standing_check_authority_follows_only_canonical_replacement_and_survives_reopen() {
    let source: serde_json::Value = serde_json::from_str(include_str!(
        "../tests/fixtures/turn_contract/check_only_recovery_preserves_accepted_generations.json"
    ))
    .unwrap();
    let mut begin: TurnContractEnvelope =
        serde_json::from_value(source["steps"][0]["envelope"].clone()).unwrap();
    let mut start: TurnContractEnvelope =
        serde_json::from_value(source["steps"][1]["envelope"].clone()).unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut canonical = SessionExecutionStore::open(
        Arc::new(
            LegacyFormatOwnership::acquire(root.path())
                .unwrap()
                .upgrade()
                .unwrap(),
        ),
        ExecutionStoreOwner {
            workspace_id: "standing-replacement".into(),
            session_id: begin.session_id.clone(),
        },
    )
    .unwrap();
    let mut content = ExecutionContentStore::open_owned(
        canonical
            .component_namespace(ExecutionComponent::ExecutionContent)
            .unwrap(),
    )
    .unwrap();
    let checks = vec![vec![
        "sh".into(),
        "-c".into(),
        "test -f reviewed.txt".into(),
    ]];
    let definitions = crate::team_work::standing_check_definitions(&checks).unwrap();
    let references = definitions
        .iter()
        .map(|definition| {
            content
                .retain_repository_check_definition(definition.clone())
                .unwrap()
                .reference()
                .clone()
        })
        .collect::<Vec<_>>();
    let repository = content
        .retain_activation_evidence(ActivationEvidenceContent::Repository {
            description: "owned exact candidate".into(),
            revision: None,
        })
        .unwrap()
        .reference()
        .clone();
    let TurnContractEvent::Begin { graph, .. } = &mut begin.event else {
        panic!()
    };
    let original = graph.nodes[0].node_id.clone();
    graph.conditions = references
        .iter()
        .enumerate()
        .map(|(index, definition)| CompletionCondition {
            condition_id: ConditionId::new(crate::team_work::standing_condition_id(
                "work-replacement",
                index,
            ))
            .unwrap(),
            kind: ConditionKind::RepositoryCheck {
                definition: definition.clone(),
            },
            nodes: vec![original.clone()],
        })
        .collect();
    let readiness = ConditionId::new("standing:work-replacement:ready").unwrap();
    graph.conditions.push(CompletionCondition {
        condition_id: readiness.clone(),
        kind: ConditionKind::Review {
            criterion: EvidenceRef::new("exact-candidate-readiness").unwrap(),
        },
        nodes: vec![original.clone()],
    });
    canonical.append(begin.clone()).unwrap();
    let policy = AuthorityGrant {
        id: "standing-grant".into(),
        revision: 1,
        issuer_evidence: EvidenceRef::new("armed-source").unwrap(),
        holder: original.clone(),
        descendants: vec![],
        allow_stop_descendants: false,
        delegation: None,
        profiles: vec![ExecutionProfile {
            definition: "shared-coder".into(),
            provider: "local".into(),
            model: "finite".into(),
            isolation: "owned-check".into(),
            tools: vec!["bash".into()],
        }],
        conditions: vec![],
        limits: GrantLimits {
            activations: 4,
            invocations: 10,
            tokens: 1000,
            cost_microunits: 0,
        },
        expires_at_ms: 1000,
    };
    let namespace = || {
        canonical
            .component_namespace(ExecutionComponent::ControlAuthority {
                turn_id: begin.turn_id.clone(),
            })
            .unwrap()
    };
    let gate = ControlAuthority::open_owned(namespace()).unwrap();
    gate.install_grant(policy.clone(), 0).unwrap();
    let allocation = DurableTeamWorkAllocation {
        receipt_id: "work-replacement".into(),
        session_id: begin.session_id.as_str().into(),
        turn_id: begin.turn_id.as_str().into(),
        required_checks: checks,
        allocation: TeamWorkGrantAllocation {
            grant: TeamWorkGrantReference {
                id: policy.id.clone(),
                revision: policy.revision,
                limits: policy.limits.clone(),
                expires_at_ms: policy.expires_at_ms,
            },
            consumed_before: GrantUsage::default(),
            settlement: None,
        },
    };
    gate.apply_team_work_allocation(&allocation).unwrap();
    gate.authorize_team_work_conditions(
        &allocation,
        &canonical.snapshot(&begin.turn_id).unwrap(),
        &content,
        &repository,
        "owned-check",
    )
    .unwrap();
    let retained = content
        .retain_activation_evidence(ActivationEvidenceContent::Grant {
            policy: policy.clone(),
        })
        .unwrap();
    let grant = GrantSnapshotRef {
        grant_id: GrantId::new(&policy.id).unwrap(),
        revision: 1,
        evidence: retained.reference().clone(),
    };
    let snapshot = canonical.snapshot(&begin.turn_id).unwrap();
    let mut graph = snapshot.contract().graph().unwrap().clone();
    let previous_graph = graph.snapshot_id.clone();
    graph.revision += 1;
    graph.snapshot_id = GraphSnapshotId::new("replacement-graph").unwrap();
    let replacement = TurnNodeId::new("replacement").unwrap();
    graph.nodes[0].node_id = replacement.clone();
    graph.nodes[0].slot_id = SessionTeamSlotId::new("replacement-slot").unwrap();
    graph.nodes[0].conversation_id = NodeConversationId::new("replacement-conversation").unwrap();
    for condition in &mut graph.conditions {
        condition.nodes = vec![replacement.clone()];
    }
    canonical
        .append(TurnContractEnvelope {
            schema_version: TURN_CONTRACT_SCHEMA_VERSION,
            command_id: CommandId::new("replace-approved").unwrap(),
            expected_revision: snapshot.contract().revision(),
            session_id: begin.session_id.clone(),
            turn_id: begin.turn_id.clone(),
            event: TurnContractEvent::ReviseGraph {
                epoch_id: snapshot.contract().epochs()[0].id.clone(),
                previous_graph,
                graph,
                mutation: GraphMutation::ReplaceFuture {
                    previous: original.clone(),
                    replacement: replacement.clone(),
                    rewire_dependents: vec![],
                },
                admission_evidence: EvidenceRef::new("exact-replacement-admission").unwrap(),
            },
        })
        .unwrap();
    start.expected_revision = canonical
        .snapshot(&begin.turn_id)
        .unwrap()
        .contract()
        .revision();
    let TurnContractEvent::StartActivation { input } = &mut start.event else {
        panic!()
    };
    input.activation.node_id = replacement.clone();
    input.conversation_id = NodeConversationId::new("replacement-conversation").unwrap();
    input.repository = RepositoryInput::Recorded {
        snapshot: repository.clone(),
    };
    let activation = input.activation.clone();
    let conversation = input.conversation_id.clone();
    canonical.append(start).unwrap();
    let output = content
        .retain_output(
            &canonical.snapshot(&begin.turn_id).unwrap(),
            ActivationOutputContent {
                activation: activation.clone(),
                recorded_at_unix_ms: 1,
                text: "actual retained replacement output".into(),
                usage: ExecutionUsage::Measured {
                    usage: TokenUsageStats::default(),
                },
                kind: OutputKind::Final,
            },
        )
        .unwrap();
    append(
        &mut canonical,
        &begin.turn_id,
        TurnContractEvent::AcceptActivation {
            activation: activation.clone(),
            checkpoint: Box::new(CheckpointRef {
                checkpoint_id: CheckpointId::new("replacement-checkpoint").unwrap(),
                session_id: begin.session_id.clone(),
                conversation_id: conversation,
                source: CheckpointSource::Accepted {
                    activation: activation.clone(),
                },
            }),
            output: output.reference().clone(),
        },
    );
    let mut runs = Vec::new();
    for (index, definition) in definitions.iter().enumerate() {
        let run = ConditionRunRef {
            session_id: begin.session_id.clone(),
            turn_id: begin.turn_id.clone(),
            epoch_id: activation.execution_epoch_id.clone(),
            condition_id: ConditionId::new(crate::team_work::standing_condition_id(
                "work-replacement",
                index,
            ))
            .unwrap(),
            run_id: ConditionRunId::new(format!("check-{index}")).unwrap(),
            activations: vec![activation.clone()],
        };
        let arguments = content
            .reserve_condition_arguments(
                &canonical.snapshot(&begin.turn_id).unwrap(),
                &run,
                &repository,
            )
            .unwrap();
        assert_eq!(arguments.definition(), definition);
        append(
            &mut canonical,
            &begin.turn_id,
            TurnContractEvent::RecordConditionIntent {
                run: run.clone(),
                intent: arguments.reference().clone(),
            },
        );
        let claim = gate
            .claim_condition_run(&canonical, &content, &arguments, &grant, "owned-check", 100)
            .unwrap();
        gate.validate_condition_claim(&canonical, &claim, 101)
            .unwrap();
        let data = gate.lock().unwrap().data.clone();
        let record = &data.condition_calls[index];
        let stored = &data.grants[0];
        let mut foreign = record.clone();
        foreign.run.activations[0].node_id = TurnNodeId::new("unmapped-added-node").unwrap();
        assert!(!condition_allowed(&foreign, stored, &stored.policy));
        assert!(replaced_condition_permission(
            &canonical.snapshot(&begin.turn_id).unwrap(),
            &foreign,
            stored
        )
        .is_none());
        let result = content
            .record_condition_result(
                &arguments,
                ConditionProcessStatus::Exited { code: 0 },
                ConditionOutputCapture::new(definition.stdout_bytes)
                    .unwrap()
                    .finish(false),
                ConditionOutputCapture::new(definition.stderr_bytes)
                    .unwrap()
                    .finish(false),
                102,
            )
            .unwrap();
        gate.settle_condition_run(&claim, &result).unwrap();
        append(
            &mut canonical,
            &begin.turn_id,
            TurnContractEvent::ResolveConditionIntent {
                run_id: run.run_id.clone(),
                resolution: ConditionEffectResolution::OutcomeRecorded {
                    evidence: result.reference().clone(),
                },
            },
        );
        append(
            &mut canonical,
            &begin.turn_id,
            TurnContractEvent::RecordCondition {
                epoch_id: run.epoch_id.clone(),
                condition_id: run.condition_id.clone(),
                activations: run.activations.clone(),
                outcome: ConditionOutcome::Passed,
                evidence: result.reference().clone(),
            },
        );
        runs.push(run);
    }
    append(
        &mut canonical,
        &begin.turn_id,
        TurnContractEvent::RecordCondition {
            epoch_id: activation.execution_epoch_id.clone(),
            condition_id: readiness,
            activations: vec![activation],
            outcome: ConditionOutcome::Passed,
            evidence: EvidenceRef::new("retained-candidate-check-readiness").unwrap(),
        },
    );
    append(
        &mut canonical,
        &begin.turn_id,
        TurnContractEvent::Close {
            closure: TurnClosure::Completed,
        },
    );
    assert_eq!(
        canonical
            .snapshot(&begin.turn_id)
            .unwrap()
            .contract()
            .state(),
        Some(LogicalTurnState::Completed)
    );
    assert_eq!(gate.usage(&policy.id).unwrap().invocations, 3);
    {
        let state = gate.lock().unwrap();
        let permissions = &state.data.grants[0].standing.as_ref().unwrap().conditions;
        assert!(permissions
            .iter()
            .any(|permission| permission.nodes.as_slice() == std::slice::from_ref(&original)));
        assert!(permissions
            .iter()
            .any(|permission| permission.nodes.as_slice() == std::slice::from_ref(&replacement)));
        validate_data(&state.data).unwrap();
    }
    drop(gate);
    let gate = ControlAuthority::open_owned(
        canonical
            .component_namespace(ExecutionComponent::ControlAuthority {
                turn_id: begin.turn_id.clone(),
            })
            .unwrap(),
    )
    .unwrap();
    assert_eq!(gate.usage(&policy.id).unwrap().invocations, 3);
    for run in runs {
        assert!(gate
            .condition_call(&run.run_id)
            .unwrap()
            .unwrap()
            .result
            .is_some());
    }
}

fn append(canonical: &mut SessionExecutionStore, turn: &LogicalTurnId, event: TurnContractEvent) {
    let snapshot = canonical.snapshot(turn).unwrap();
    canonical
        .append(TurnContractEnvelope {
            schema_version: TURN_CONTRACT_SCHEMA_VERSION,
            command_id: CommandId::new(format!("event-{}", snapshot.contract().revision()))
                .unwrap(),
            expected_revision: snapshot.contract().revision(),
            session_id: snapshot.owner().session_id.clone(),
            turn_id: turn.clone(),
            event,
        })
        .unwrap();
}
