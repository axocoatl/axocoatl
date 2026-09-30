use super::*;
use axocoatl_core::TokenUsageStats;
use axocoatl_session::execution_content::{
    ActivationOutputContent, ExecutionRequestContent, ExecutionUsage, OutputKind,
};
use axocoatl_session::execution_ownership::LegacyFormatOwnership;
use axocoatl_session::execution_store::ExecutionStoreOwner;
use axocoatl_session::turn_checks::{check_definitions, CheckGroup};

/// A shared check consumes both canonical outputs. A model's assertion that a
/// peer finished cannot substitute for the peer's retained acceptance.
#[test]
fn required_checks_consume_the_complete_two_agent_accepted_frontier() {
    check_frontier(&CheckGroup::required(), false);
}

#[test]
fn required_checks_follow_canonical_replacement_without_expanding_to_added_agents() {
    check_frontier(&CheckGroup::required(), true);
}

fn check_frontier(group: &CheckGroup, dynamic: bool) {
    let fixture: serde_json::Value = serde_json::from_str(include_str!("../../axocoatl-session/tests/fixtures/turn_contract/check_only_recovery_preserves_accepted_generations.json")).unwrap();
    let mut begin: TurnContractEnvelope =
        serde_json::from_value(fixture["steps"][0]["envelope"].clone()).unwrap();
    let source_start: TurnContractEnvelope =
        serde_json::from_value(fixture["steps"][1]["envelope"].clone()).unwrap();
    let source_accept: TurnContractEnvelope =
        serde_json::from_value(fixture["steps"][2]["envelope"].clone()).unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut canonical = SessionExecutionStore::open(
        Arc::new(
            LegacyFormatOwnership::acquire(root.path())
                .unwrap()
                .upgrade()
                .unwrap(),
        ),
        ExecutionStoreOwner {
            workspace_id: "required-checks".into(),
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
        "test -f first.txt && test -f second.txt".into(),
    ]];
    let definitions = check_definitions(&checks).unwrap();
    let definition = definitions[1].clone();
    let definition_refs = definitions
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
            description: "actual owner supplied by host".into(),
            revision: None,
        })
        .unwrap()
        .reference()
        .clone();
    let TurnContractEvent::Begin { graph, .. } = &mut begin.event else {
        panic!()
    };
    let mut second = graph.nodes[0].clone();
    second.node_id = TurnNodeId::new("b").unwrap();
    second.slot_id = SessionTeamSlotId::new("slot-b").unwrap();
    second.conversation_id = NodeConversationId::new("conversation-b").unwrap();
    graph.nodes.push(second);
    let nodes = graph
        .nodes
        .iter()
        .map(|node| node.node_id.clone())
        .collect::<Vec<_>>();
    graph.conditions = definition_refs
        .into_iter()
        .enumerate()
        .map(|(index, definition)| CompletionCondition {
            condition_id: ConditionId::new(group.condition_id(index)).unwrap(),
            kind: ConditionKind::RepositoryCheck { definition },
            nodes: nodes.clone(),
        })
        .collect();
    graph.conditions.push(CompletionCondition {
        condition_id: ConditionId::new(group.ready_id()).unwrap(),
        kind: ConditionKind::Review {
            criterion: EvidenceRef::new("readiness-definition").unwrap(),
        },
        nodes,
    });
    let request = content
        .retain_request(ExecutionRequestContent {
            turn_id: begin.turn_id.clone(),
            recorded_at_unix_ms: 1,
            display_input: "Check both Agents".into(),
            effective_input: "Check both Agents".into(),
            context: vec![],
            target_definition: None,
            model: None,
        })
        .unwrap();
    canonical
        .begin_with_request(begin.clone(), &request)
        .unwrap();
    if dynamic {
        for replacement in [true, false] {
            let snapshot = canonical.snapshot(&begin.turn_id).unwrap();
            let old = snapshot.contract().graph().unwrap();
            let mut graph = old.clone();
            graph.revision += 1;
            graph.snapshot_id =
                GraphSnapshotId::new(format!("dynamic-{}", graph.revision)).unwrap();
            let name = if replacement { "replacement" } else { "added" };
            let mut node = graph.nodes[1].clone();
            node.node_id = TurnNodeId::new(name).unwrap();
            node.slot_id = SessionTeamSlotId::new(format!("slot-{name}")).unwrap();
            node.conversation_id = NodeConversationId::new(format!("conversation-{name}")).unwrap();
            let mutation = if replacement {
                let previous = graph.nodes.remove(1).node_id;
                for condition in &mut graph.conditions {
                    for selected in &mut condition.nodes {
                        if selected == &previous {
                            *selected = node.node_id.clone();
                        }
                    }
                }
                GraphMutation::ReplaceFuture {
                    previous,
                    replacement: node.node_id.clone(),
                    rewire_dependents: vec![],
                }
            } else {
                GraphMutation::Add {
                    node_id: node.node_id.clone(),
                }
            };
            graph.nodes.push(node);
            canonical
                .append(TurnContractEnvelope {
                    schema_version: TURN_CONTRACT_SCHEMA_VERSION,
                    command_id: CommandId::new(format!("graph-{name}")).unwrap(),
                    expected_revision: snapshot.contract().revision(),
                    session_id: begin.session_id.clone(),
                    turn_id: begin.turn_id.clone(),
                    event: TurnContractEvent::ReviseGraph {
                        epoch_id: snapshot.contract().epochs().last().unwrap().id.clone(),
                        previous_graph: old.snapshot_id.clone(),
                        graph,
                        mutation,
                        admission_evidence: EvidenceRef::new(format!("approved-{name}")).unwrap(),
                    },
                })
                .unwrap();
        }
    }
    let mut activations = Vec::new();
    let mut outputs = Vec::new();
    let nodes = if dynamic {
        vec!["a", "replacement", "added"]
    } else {
        vec!["a", "b"]
    };
    for (index, node) in nodes.iter().enumerate() {
        assert!(
            check_activations(
                canonical.snapshot(&begin.turn_id).unwrap().contract(),
                group,
                definitions.len()
            )
            .unwrap()
            .is_none(),
            "All required Agents, including newly added work, must accept before any check starts"
        );
        let mut start = source_start.clone();
        start.expected_revision = canonical
            .snapshot(&begin.turn_id)
            .unwrap()
            .contract()
            .revision();
        start.command_id = CommandId::new(format!("start-{node}")).unwrap();
        let TurnContractEvent::StartActivation { input } = &mut start.event else {
            panic!()
        };
        input.manifest_id = InputManifestId::new(format!("input-{node}")).unwrap();
        input.activation.node_id = TurnNodeId::new(*node).unwrap();
        input.activation.activation_id = ActivationId::new(format!("activation-{node}")).unwrap();
        input.conversation_id = NodeConversationId::new(format!("conversation-{node}")).unwrap();
        input.repository = RepositoryInput::Recorded {
            snapshot: repository.clone(),
        };
        let activation = input.activation.clone();
        let conversation = input.conversation_id.clone();
        activations.push(activation.clone());
        canonical.append(start).unwrap();
        let run = ConditionRunRef {
            session_id: begin.session_id.clone(),
            turn_id: begin.turn_id.clone(),
            epoch_id: activation.execution_epoch_id.clone(),
            condition_id: ConditionId::new(group.condition_id(1)).unwrap(),
            run_id: ConditionRunId::new("shared-check").unwrap(),
            activations: activations.clone(),
        };
        assert!(
            content
                .reserve_condition_arguments(
                    &canonical.snapshot(&begin.turn_id).unwrap(),
                    &run,
                    &repository
                )
                .is_err(),
            "A running or missing required Agent cannot be treated as an accepted check input"
        );
        let output = content
            .retain_output(
                &canonical.snapshot(&begin.turn_id).unwrap(),
                ActivationOutputContent {
                    activation: activation.clone(),
                    recorded_at_unix_ms: 2 + index as u64,
                    text: format!("Retained output from {node}"),
                    usage: ExecutionUsage::Measured {
                        usage: TokenUsageStats::default(),
                    },
                    kind: OutputKind::Final,
                },
            )
            .unwrap()
            .reference()
            .clone();
        if index < 2 {
            outputs.push(output.clone());
        }
        let mut accept = source_accept.clone();
        accept.expected_revision = canonical
            .snapshot(&begin.turn_id)
            .unwrap()
            .contract()
            .revision();
        accept.command_id = CommandId::new(format!("accept-{node}")).unwrap();
        let TurnContractEvent::AcceptActivation {
            activation: target,
            checkpoint,
            output: target_output,
        } = &mut accept.event
        else {
            panic!()
        };
        *target = activation.clone();
        checkpoint.checkpoint_id = CheckpointId::new(format!("checkpoint-{node}")).unwrap();
        checkpoint.conversation_id = conversation;
        checkpoint.source = CheckpointSource::Accepted { activation };
        *target_output = output;
        canonical.append(accept).unwrap();
    }
    let run = ConditionRunRef {
        session_id: begin.session_id.clone(),
        turn_id: begin.turn_id.clone(),
        epoch_id: activations[0].execution_epoch_id.clone(),
        condition_id: ConditionId::new(group.condition_id(1)).unwrap(),
        run_id: ConditionRunId::new("shared-check").unwrap(),
        activations: check_activations(
            canonical.snapshot(&begin.turn_id).unwrap().contract(),
            group,
            definitions.len(),
        )
        .unwrap()
        .unwrap(),
    };
    let arguments = content
        .reserve_condition_arguments(
            &canonical.snapshot(&begin.turn_id).unwrap(),
            &run,
            &repository,
        )
        .unwrap();
    assert_eq!(arguments.definition(), &definition);
    assert_eq!(arguments.inputs().len(), 2);
    assert_eq!(
        arguments
            .inputs()
            .iter()
            .map(|input| input.output.clone())
            .collect::<Vec<_>>(),
        outputs
    );
    assert_eq!(
        arguments
            .inputs()
            .iter()
            .map(|input| input.input.activation.node_id.as_str())
            .collect::<Vec<_>>(),
        if dynamic {
            vec!["a", "replacement"]
        } else {
            vec!["a", "b"]
        }
    );
}

fn capture(tree: Option<&str>, head: Option<&str>) -> ActivationRepositorySnapshot {
    let activation = ActivationRef {
        session_id: SessionId::new("session").unwrap(),
        turn_id: LogicalTurnId::new("turn").unwrap(),
        execution_epoch_id: ExecutionEpochId::new("epoch").unwrap(),
        node_id: TurnNodeId::new("node").unwrap(),
        generation: 1,
        activation_id: ActivationId::new("activation").unwrap(),
    };
    let mut capture = repository_snapshot::empty_observation(
        &activation,
        RepositorySnapshotPhase::BeforeCheck { index: 0 },
        &EvidenceRef::new("repository").unwrap(),
    );
    capture.tree_sha256 = tree.map(str::to_owned);
    capture.head = head.map(str::to_owned);
    capture
}

type Candidate = Option<(String, Option<String>)>;

/// The candidate each of two commands ran on: the group's stable capture, and
/// none when the tree moved around them.
fn candidates(
    before: &ActivationRepositorySnapshot,
    after: &ActivationRepositorySnapshot,
) -> Vec<Candidate> {
    let candidate = (before.tree_sha256 == after.tree_sha256)
        .then(|| (after.tree_sha256.clone().unwrap(), after.head.clone()));
    vec![candidate.clone(), candidate]
}

/// Whether two commands between the captures, with these four recorded
/// outcomes, are ready.
fn two_checks(outcomes: [ConditionOutcome; 4], before: &str, after: &str) -> bool {
    let before = capture(Some(before), Some("head"));
    let after = capture(Some(after), Some("head"));
    readiness_passed(
        4,
        &outcomes,
        &candidates(&before, &after),
        2,
        &before,
        &after,
    )
}

#[test]
fn failing_check_is_not_ready() {
    use ConditionOutcome::{Failed, Passed};
    assert!(!two_checks(
        [Passed, Passed, Failed, Passed],
        "tree",
        "tree"
    ));
    // A failed capture is no more ready than a failed command.
    assert!(!two_checks(
        [Failed, Passed, Passed, Passed],
        "tree",
        "tree"
    ));
}

#[test]
fn passing_checks_on_an_unchanged_tree_are_ready() {
    use ConditionOutcome::Passed;
    assert!(two_checks([Passed; 4], "tree", "tree"));
    let outcomes = [Passed; 4];
    let before = capture(Some("tree"), Some("head"));
    let after = capture(Some("tree"), Some("head"));
    let commands = candidates(&before, &after);
    // Every command must have run on the captured candidate.
    assert!(!readiness_passed(
        4,
        &outcomes,
        &commands[..1],
        2,
        &before,
        &after
    ));
    assert!(!readiness_passed(
        4,
        &outcomes[..3],
        &commands,
        2,
        &before,
        &after
    ));
    let elsewhere = vec![commands[0].clone(), Some(("other".into(), None))];
    assert!(!readiness_passed(
        4, &outcomes, &elsewhere, 2, &before, &after
    ));
    // An unreadable capture establishes nothing.
    let unreadable = capture(None, Some("head"));
    assert!(!readiness_passed(
        4,
        &outcomes,
        &commands,
        2,
        &unreadable,
        &unreadable
    ));
}

#[test]
fn a_check_that_changes_the_tree_is_not_ready() {
    use ConditionOutcome::Passed;
    assert!(!two_checks([Passed; 4], "tree", "changed"));
    // A commit made by a check moves HEAD even when the tree matches.
    let before = capture(Some("tree"), Some("head"));
    let after = capture(Some("tree"), Some("committed"));
    let candidate = Some(("tree".to_string(), Some("committed".to_string())));
    assert!(!readiness_passed(
        4,
        &[Passed; 4],
        &[candidate.clone(), candidate],
        2,
        &before,
        &after
    ));
}
