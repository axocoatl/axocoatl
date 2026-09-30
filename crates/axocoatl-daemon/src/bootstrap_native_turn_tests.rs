//! The real owned registry, Session team journal, Begin and driver handoff seam.
//! The fixture factory explicitly fails resource preparation; no model completion
//! is fabricated, and no repository command may run through the fixture owner.
use super::*;
use crate::bootstrap::native_turn::{
    finish_owned_setup, prepare_admission, verify_selected_team, NativeFirstTurnRequest,
    NativeFirstTurnStart, NativeNodeEvidence,
};
use crate::bootstrap::session_dispatch::{NativeFirstTurnExisting, SessionDispatchRegistry};
use crate::session_dispatch::{
    AutonomousActivationFactory, AutonomousActivationResources, RetainedSessionStores,
    SuccessorTurn,
};
use axocoatl_core::{AgentConfig, AgentId, SamplingConfig};
use axocoatl_memory::activation_state::ActivationStateStore;
use axocoatl_session::control_authority::{AuthorityGrant, ExecutionProfile, GrantLimits};
use axocoatl_session::execution_content::{
    ActivationEvidenceContent, ExecutionContentStore, ExecutionRequestContent,
};
use axocoatl_session::execution_namespace::ExecutionComponent;
use axocoatl_session::session_team::*;
use axocoatl_session::turn_contract::*;

struct NativeFixture {
    repository: Fixture,
    registry: SessionDispatchRegistry,
    request: NativeFirstTurnRequest,
}
async fn native_fixture() -> NativeFixture {
    native_fixture_with_invocations(0).await
}
async fn native_fixture_with_invocations(invocations: u32) -> NativeFixture {
    native_fixture_with(invocations, "Exact test host approval", [&[], &[]]).await
}
/// The team's grants carry the Apply that approved these required checks;
/// node-0 has no bash and node-1 has.
async fn native_fixture_with_checks(checks: &[Vec<String>]) -> NativeFixture {
    let approval = serde_json::json!({
        "kind": "authenticated_session_team_apply",
        "edit": {
            "command_id": "apply-initial-team",
            "expected_configuration_revision": 0,
            "slots": [],
            "dependencies": [],
            "layout": [],
            "required_checks": checks,
        },
        "templates": [],
    });
    native_fixture_with(0, &approval.to_string(), [&["read_file"], &["bash"]]).await
}
/// Two slots whose grants `issuer` approved; slot `n` has `tools[n]`.
async fn native_fixture_with(invocations: u32, issuer: &str, tools: [&[&str]; 2]) -> NativeFixture {
    let mut repository = fixture_with_legacy_turn(Some("legacy-before-native")).await;
    let canonical = repository._canonical.take().unwrap();
    let content = ExecutionContentStore::open_owned(
        canonical
            .component_namespace(ExecutionComponent::ExecutionContent)
            .unwrap(),
    )
    .unwrap();
    let memory = ActivationStateStore::open_owned(
        canonical
            .component_namespace(ExecutionComponent::ActivationState)
            .unwrap(),
    )
    .unwrap();
    let mut held = Some(RetainedSessionStores {
        canonical,
        content,
        memory,
    });
    let registry = SessionDispatchRegistry::default();
    registry.retain_existing_session(&mut held).unwrap();
    let session_id = repository.owner.identity().owner().session_id.clone();
    let token = registry.session_team_token(session_id.as_str()).unwrap();
    let (grants, node_evidence) = registry
        .with_session_team_stores(&token, |canonical, content, _| {
            let issuer = content
                .retain_activation_evidence(ActivationEvidenceContent::Guidance {
                    text: issuer.into(),
                })
                .unwrap()
                .reference()
                .clone();
            let mut slots = Vec::new();
            let mut continuity = Vec::new();
            let mut grants = Vec::new();
            let mut inputs = Vec::new();
            for index in 0..2 {
                let node_id = TurnNodeId::new(format!("node-{index}")).unwrap();
                let slot_id = SessionTeamSlotId::new(format!("slot-{index}")).unwrap();
                let conversation_id =
                    NodeConversationId::new(format!("conversation-{index}")).unwrap();
                let definition_id = AgentDefinitionId::new(format!("definition-{index}")).unwrap();
                let tools: Vec<String> = tools[index].iter().map(|tool| (*tool).into()).collect();
                let config = AgentConfig {
                    id: AgentId::new(conversation_id.as_str()),
                    provider: "ollama".into(),
                    model: "test-model".into(),
                    tools: tools.clone(),
                    sampling: SamplingConfig {
                        max_tokens: Some(128),
                        ..Default::default()
                    },
                    ..Default::default()
                };
                let profile = ExecutionProfile {
                    definition: definition_id.as_str().into(),
                    provider: config.provider.clone(),
                    model: config.model.clone(),
                    isolation: "in-process".into(),
                    tools,
                    write_scope: None,
                };
                let definition = content
                    .retain_activation_evidence(ActivationEvidenceContent::Definition {
                        definition_id: definition_id.clone(),
                        revision: 1,
                        profile: profile.clone(),
                        configuration: serde_json::to_string(&config).unwrap(),
                    })
                    .unwrap()
                    .reference()
                    .clone();
                let limits = GrantLimits {
                    activations: 2,
                    invocations,
                    tokens: 8192,
                    cost_microunits: 0,
                };
                let policy = AuthorityGrant {
                    id: format!("grant-{index}"),
                    revision: 1,
                    issuer_evidence: issuer.clone(),
                    holder: node_id.clone(),
                    descendants: vec![],
                    allow_stop_descendants: false,
                    delegation: None,
                    profiles: vec![profile],
                    conditions: vec![],
                    limits: limits.clone(),
                    expires_at_ms: u64::MAX,
                };
                let grant = content
                    .retain_activation_evidence(ActivationEvidenceContent::Grant {
                        policy: policy.clone(),
                    })
                    .unwrap()
                    .reference()
                    .clone();
                let budget = content
                    .retain_activation_evidence(ActivationEvidenceContent::Budget { limits })
                    .unwrap()
                    .reference()
                    .clone();
                slots.push(SessionTeamSlot {
                    slot_id: slot_id.clone(),
                    node_id: node_id.clone(),
                    definition: DefinitionSnapshotRef {
                        definition_id,
                        snapshot: definition,
                    },
                    conversation_id,
                    required: true,
                    budget,
                    grant: Some(grant),
                });
                continuity.push(SlotContinuityDecision {
                    slot_id,
                    decision: SessionTeamContinuity::Reset,
                });
                inputs.push(NativeNodeEvidence {
                    node_id,
                    guidance: vec![],
                    attachments: vec![],
                });
                grants.push(policy);
            }
            let graph = SessionTeamGraph {
                slots,
                dependencies: vec![DependencyEdge {
                    parent: TurnNodeId::new("node-0").unwrap(),
                    child: TurnNodeId::new("node-1").unwrap(),
                }],
                conditions: vec![],
            };
            let mut team = SessionTeamStore::open_owned(
                canonical
                    .component_namespace(ExecutionComponent::SessionTeam)
                    .unwrap(),
                canonical,
                content,
                None,
            )
            .unwrap();
            team.commit(
                SessionTeamCommit {
                    schema_version: 1,
                    command_id: CommandId::new("apply-initial-team").unwrap(),
                    expected_configuration_revision: 0,
                    graph,
                    initial_source: None,
                    continuity,
                    layout: vec![],
                },
                canonical,
                content,
                None,
            )
            .unwrap();
            Ok((grants, inputs))
        })
        .unwrap();
    let turn_id = LogicalTurnId::new("native-first").unwrap();
    let request = NativeFirstTurnRequest {
        model_selections: vec![],
        standing_work: None,
        schema_version: 1,
        ingress: Some(serde_json::json!({"input":"Do the work","context":[]})),
        session_id,
        command_id: CommandId::new("native-first-begin").unwrap(),
        turn_id: turn_id.clone(),
        epoch_id: ExecutionEpochId::new("native-first-epoch").unwrap(),
        graph_snapshot_id: GraphSnapshotId::new("native-first-graph").unwrap(),
        expected_team_revision: 1,
        target_definition: None,
        request: ExecutionRequestContent {
            turn_id,
            recorded_at_unix_ms: 1,
            display_input: "Do the work".into(),
            effective_input: "Do the work".into(),
            context: vec![],
            target_definition: None,
            model: None,
        },
        grants,
        node_evidence,
    };
    NativeFixture {
        repository,
        registry,
        request,
    }
}
fn begin(
    f: &NativeFixture,
    request: &NativeFirstTurnRequest,
) -> (
    crate::session_dispatch::SessionDispatchController,
    EvidenceRef,
) {
    let team_token = f
        .registry
        .session_team_token(request.session_id.as_str())
        .unwrap();
    let setup = prepare_admission(
        &f.registry,
        &team_token,
        &SecureDir::open(f.repository._data.path()).unwrap(),
        request,
        &request.source().unwrap(),
    )
    .unwrap();
    let token = f
        .registry
        .prepare_first_turn(request.session_id.as_str())
        .unwrap();
    f.registry
        .begin_first_turn_checked(
            &token,
            f.repository.owner.clone(),
            SuccessorTurn {
                command_id: request.command_id.clone(),
                turn_id: request.turn_id.clone(),
                epoch_id: request.epoch_id.clone(),
                graph: setup.content.graph.clone(),
                request: request.request.clone(),
            },
            |canonical, content, memory| {
                verify_selected_team(
                    canonical,
                    content,
                    memory,
                    request.expected_team_revision,
                    &request.turn_id,
                    &setup.content.graph,
                    request.target_definition.as_ref(),
                )
            },
        )
        .unwrap()
}
struct RefusingFactory(Arc<AtomicUsize>);
#[async_trait::async_trait]
impl AutonomousActivationFactory for RefusingFactory {
    async fn resources(
        &self,
        _: &ActivationInputManifest,
    ) -> std::result::Result<AutonomousActivationResources, String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err("Fixture records unavailable configured model; no inference occurred".into())
    }
}
#[tokio::test]
async fn native_admission_retains_complete_ingress_and_only_exact_approved_grants() {
    let f = native_fixture().await;
    let token = f
        .registry
        .session_team_token(f.request.session_id.as_str())
        .unwrap();
    let data = SecureDir::open(f.repository._data.path()).unwrap();
    let source = f.request.source().unwrap();
    let first = prepare_admission(&f.registry, &token, &data, &f.request, &source).unwrap();
    let repeat = prepare_admission(&f.registry, &token, &data, &f.request, &source).unwrap();
    assert_eq!(first.content, repeat.content);
    assert_eq!(first.content.nodes.len(), 2);
    for node in &first.content.nodes {
        assert_eq!(node.guidance, vec![first.content.request.clone()]);
    }
    let mut changed = f.request.clone();
    changed.ingress = Some(serde_json::json!({"input":"Do another thing"}));
    assert!(prepare_admission(
        &f.registry,
        &token,
        &data,
        &changed,
        &changed.source().unwrap()
    )
    .is_err());
    let mut unapproved = f.request.clone();
    unapproved.turn_id = LogicalTurnId::new("unapproved").unwrap();
    unapproved.request.turn_id = unapproved.turn_id.clone();
    unapproved.command_id = CommandId::new("unapproved-begin").unwrap();
    unapproved.grants[0].limits.tokens += 1;
    assert!(prepare_admission(
        &f.registry,
        &token,
        &data,
        &unapproved,
        &unapproved.source().unwrap()
    )
    .is_err());
    f.registry
        .with_session_team_stores(&token, |canonical, _, _| {
            assert!(canonical.records().unwrap().is_empty());
            Ok(())
        })
        .unwrap();
}
#[tokio::test]
async fn targeted_native_send_uses_only_selected_slot_and_its_own_conversation() {
    let f = native_fixture().await;
    let mut request = f.request.clone();
    request.target_definition = Some(AgentDefinitionId::new("definition-1").unwrap());
    request.request.target_definition = request.target_definition.clone();
    request.grants.remove(0);
    request.node_evidence.remove(0);
    let token = f
        .registry
        .session_team_token(request.session_id.as_str())
        .unwrap();
    let setup = prepare_admission(
        &f.registry,
        &token,
        &SecureDir::open(f.repository._data.path()).unwrap(),
        &request,
        &request.source().unwrap(),
    )
    .unwrap();
    assert_eq!(setup.content.graph.nodes.len(), 1);
    assert!(setup.content.graph.dependencies.is_empty());
    assert!(setup.content.graph.conditions.is_empty());
    let node = &setup.content.graph.nodes[0];
    assert_eq!(node.node_id.as_str(), "node-1");
    assert_eq!(node.conversation_id.as_str(), "conversation-1");
    assert!(node.required);
    let (controller, _) = begin(&f, &request);
    assert_eq!(
        controller.snapshot().unwrap().contract().graph().unwrap(),
        &setup.content.graph
    );
}
#[tokio::test]
async fn approved_required_checks_become_turn_conditions_and_survive_exact_retry() {
    let checks = vec![vec!["sh".into(), "-c".into(), "test -f done.txt".into()]];
    let f = native_fixture_with_checks(&checks).await;
    let token = f
        .registry
        .session_team_token(f.request.session_id.as_str())
        .unwrap();
    let data = SecureDir::open(f.repository._data.path()).unwrap();
    let source = f.request.source().unwrap();
    let first = prepare_admission(&f.registry, &token, &data, &f.request, &source).unwrap();
    let repeat = prepare_admission(&f.registry, &token, &data, &f.request, &source).unwrap();
    assert_eq!(first.content, repeat.content);
    let graph = &first.content.graph;
    assert_eq!(
        graph
            .conditions
            .iter()
            .map(|condition| condition.condition_id.as_str())
            .collect::<Vec<_>>(),
        [
            "required-check:0",
            "required-check:1",
            "required-check:2",
            "required-check:ready"
        ]
    );
    let nodes = vec![
        TurnNodeId::new("node-0").unwrap(),
        TurnNodeId::new("node-1").unwrap(),
    ];
    assert!(graph
        .conditions
        .iter()
        .all(|condition| condition.nodes == nodes));
    assert_eq!(
        axocoatl_session::turn_checks::group_of(graph),
        Some((axocoatl_session::turn_checks::CheckGroup::required(), 1))
    );
    f.registry
        .with_session_team_stores(&token, |_, content, _| {
            let ConditionKind::RepositoryCheck { definition } = &graph.conditions[1].kind else {
                panic!("the command is a repository check")
            };
            assert_eq!(
                content
                    .resolve_repository_check_definition(definition)
                    .unwrap()
                    .argv,
                checks[0]
            );
            let ConditionKind::Review { criterion } = &graph.conditions[3].kind else {
                panic!("readiness is a review")
            };
            assert!(matches!(
                content.resolve_activation_evidence(criterion).unwrap(),
                ActivationEvidenceContent::Guidance { text }
                    if *text == axocoatl_session::turn_checks::readiness_text(&checks)
            ));
            Ok(())
        })
        .unwrap();
    let (controller, repository) = begin(&f, &f.request);
    assert_eq!(
        controller.snapshot().unwrap().contract().graph(),
        Some(graph)
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let factory = Arc::new(RefusingFactory(calls.clone()));
    let bus = crate::stream::StreamBus::new(64);
    let NativeFirstTurnStart::Prepared(prepared) = finish_owned_setup(
        &f.registry,
        controller.clone(),
        repository.clone(),
        &source,
        bus.clone(),
        factory.clone(),
    )
    .unwrap() else {
        panic!("first owned handoff must prepare")
    };
    // The first required Agent that may use bash pays: node-1, not node-0.
    let pays = |grant: &'static str| {
        controller
            .with_team_work_authority(|_, _, authority| {
                Ok(authority.grant_pays_required_checks(grant).unwrap())
            })
            .unwrap()
    };
    assert!(!pays("grant-0"));
    assert!(pays("grant-1"));
    // An exact retry reattaches without another driver or authorization.
    assert!(matches!(
        finish_owned_setup(
            &f.registry,
            controller.clone(),
            repository,
            &source,
            bus,
            factory
        )
        .unwrap(),
        NativeFirstTurnStart::Reattached(_)
    ));
    let token = f
        .registry
        .session_team_token(f.request.session_id.as_str())
        .unwrap();
    let retried = prepare_admission(&f.registry, &token, &data, &f.request, &source).unwrap();
    assert_eq!(retried.content, first.content);
    // No Agent was accepted, so no check ran; the turn needs attention.
    let outcome = prepared.run().await.unwrap();
    assert_eq!(
        outcome.snapshot.contract().state(),
        Some(LogicalTurnState::NeedsAttention)
    );
    assert!(outcome.snapshot.contract().condition_runs().is_empty());
    let view = controller.control_plane().unwrap();
    assert_eq!(view.required_checks.len(), 1);
    assert_eq!(view.required_checks[0].argv, checks[0]);
    assert_eq!(view.required_checks[0].state, "pending");
    let choices = view.turn_controls.unwrap().check_choices;
    let choice = choices
        .iter()
        .find(|choice| choice.condition_id.as_str() == "required-check:1")
        .unwrap();
    assert_eq!(
        choice
            .required_conditions
            .iter()
            .map(ConditionId::as_str)
            .collect::<Vec<_>>(),
        [
            "required-check:0",
            "required-check:2",
            "required-check:ready"
        ]
    );
}
#[tokio::test]
async fn targeted_send_keeps_required_checks() {
    let checks = vec![vec!["sh".into(), "-c".into(), "test -f done.txt".into()]];
    let f = native_fixture_with_checks(&checks).await;
    let token = f
        .registry
        .session_team_token(f.request.session_id.as_str())
        .unwrap();
    let data = SecureDir::open(f.repository._data.path()).unwrap();
    let target = |index: usize| {
        let mut request = f.request.clone();
        request.target_definition =
            Some(AgentDefinitionId::new(format!("definition-{index}")).unwrap());
        request.request.target_definition = request.target_definition.clone();
        request.grants.remove(1 - index);
        request.node_evidence.remove(1 - index);
        request
    };
    // node-0 cannot use bash, so no Agent of this turn could pay for them.
    let shell_less = target(0);
    let refused = prepare_admission(
        &f.registry,
        &token,
        &data,
        &shell_less,
        &shell_less.source().unwrap(),
    )
    .err()
    .unwrap()
    .to_string();
    assert!(refused.contains("required checks"), "{refused}");
    let request = target(1);
    let setup = prepare_admission(
        &f.registry,
        &token,
        &data,
        &request,
        &request.source().unwrap(),
    )
    .unwrap();
    let graph = &setup.content.graph;
    assert_eq!(graph.nodes.len(), 1);
    assert_eq!(graph.nodes[0].node_id.as_str(), "node-1");
    assert_eq!(graph.conditions.len(), 4);
    assert!(graph
        .conditions
        .iter()
        .all(|condition| condition.nodes == vec![graph.nodes[0].node_id.clone()]));
    assert_eq!(
        axocoatl_session::turn_checks::group_of(graph),
        Some((axocoatl_session::turn_checks::CheckGroup::required(), 1))
    );
    let (controller, _) = begin(&f, &request);
    assert_eq!(
        controller.snapshot().unwrap().contract().graph(),
        Some(graph)
    );
}
#[tokio::test]
async fn exact_native_retry_cannot_create_a_second_driver_or_replay_after_stop_and_successor() {
    let f = native_fixture().await;
    let source = f.request.source().unwrap();
    let (controller, repository) = begin(&f, &f.request);
    let calls = Arc::new(AtomicUsize::new(0));
    let factory = Arc::new(RefusingFactory(calls.clone()));
    let bus = crate::stream::StreamBus::new(64);
    let first = finish_owned_setup(
        &f.registry,
        controller.clone(),
        repository.clone(),
        &source,
        bus.clone(),
        factory.clone(),
    )
    .unwrap();
    let NativeFirstTurnStart::Prepared(prepared) = first else {
        panic!("first owned handoff must prepare")
    };
    assert!(matches!(
        finish_owned_setup(
            &f.registry,
            controller.clone(),
            repository,
            &source,
            bus,
            factory
        )
        .unwrap(),
        NativeFirstTurnStart::Reattached(_)
    ));
    let outcome = prepared.run().await.unwrap();
    assert_eq!(
        outcome.snapshot.contract().state(),
        Some(LogicalTurnState::NeedsAttention)
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    f.registry
        .request_human_turn_stop(f.request.session_id.as_str(), f.request.turn_id.as_str())
        .unwrap();
    assert_eq!(
        controller.snapshot().unwrap().contract().state(),
        Some(LogicalTurnState::Cancelled)
    );
    let mut successor = f.request.clone();
    successor.turn_id = LogicalTurnId::new("native-second").unwrap();
    successor.request.turn_id = successor.turn_id.clone();
    successor.command_id = CommandId::new("native-second-begin").unwrap();
    successor.epoch_id = ExecutionEpochId::new("native-second-epoch").unwrap();
    successor.graph_snapshot_id = GraphSnapshotId::new("native-second-graph").unwrap();
    let token = f
        .registry
        .session_team_token(f.request.session_id.as_str())
        .unwrap();
    let next = prepare_admission(
        &f.registry,
        &token,
        &SecureDir::open(f.repository._data.path()).unwrap(),
        &successor,
        &successor.source().unwrap(),
    )
    .unwrap();
    drop(controller);
    let (current, _) = f
        .registry
        .begin_native_successor_checked(
            f.request.session_id.as_str(),
            SuccessorTurn {
                command_id: successor.command_id.clone(),
                turn_id: successor.turn_id.clone(),
                epoch_id: successor.epoch_id.clone(),
                graph: next.content.graph.clone(),
                request: successor.request.clone(),
            },
            |canonical, content, memory| {
                verify_selected_team(
                    canonical,
                    content,
                    memory,
                    1,
                    &successor.turn_id,
                    &next.content.graph,
                    None,
                )
            },
        )
        .unwrap();
    assert_eq!(current.snapshot().unwrap().turn_id(), &successor.turn_id);
    assert!(matches!(
        f.registry
            .native_first_turn_existing(f.request.session_id.as_str(), &f.request.turn_id, &source)
            .unwrap(),
        NativeFirstTurnExisting::Retained(_)
    ));
    assert!(f
        .registry
        .native_first_turn_existing(
            f.request.session_id.as_str(),
            &f.request.turn_id,
            "changed body"
        )
        .is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

fn continue_request(
    controller: &crate::session_dispatch::SessionDispatchController,
    id: &str,
) -> crate::session_dispatch::HumanControlActionRequest {
    let snapshot = controller.snapshot().unwrap();
    let contract = snapshot.contract();
    let activation = contract
        .activations()
        .iter()
        .rev()
        .find(|item| item.activation.node_id.as_str() == "node-0")
        .unwrap()
        .activation
        .clone();
    crate::session_dispatch::HumanControlActionRequest {
        schema_version: 1,
        command_id: CommandId::new(id).unwrap(),
        session_id: snapshot.owner().session_id.clone(),
        turn_id: snapshot.turn_id().clone(),
        execution_epoch_id: contract.epochs().last().unwrap().id.clone(),
        expected_turn_revision: contract.revision(),
        expected_graph_revision: contract.graph().unwrap().revision,
        activation: None,
        action: crate::session_dispatch::HumanControlAction::Continue,
        instruction: None,
        include_previous_output: false,
        context: None,
        continuation: Some(crate::session_dispatch::HumanContinuationSelection {
            restart: vec![activation],
            checks: vec![],
        }),
        blocker_id: None,
        human_response: None,
        partial_finish: None,
    }
}
#[tokio::test]
async fn exact_applied_continue_runs_existing_driver_once_and_preserves_original_grant() {
    let f = native_fixture().await;
    let (controller, repository) = begin(&f, &f.request);
    let calls = Arc::new(AtomicUsize::new(0));
    let factory = Arc::new(RefusingFactory(calls.clone()));
    let bus = crate::stream::StreamBus::new(64);
    let NativeFirstTurnStart::Prepared(first) = finish_owned_setup(
        &f.registry,
        controller.clone(),
        repository.clone(),
        &f.request.source().unwrap(),
        bus.clone(),
        factory.clone(),
    )
    .unwrap() else {
        panic!("expected first owned driver")
    };
    first.run().await.unwrap();
    let request = continue_request(&controller, "continue-after-resource-failure");
    let receipt = f
        .registry
        .submit_human_action(
            f.request.session_id.as_str(),
            f.request.turn_id.as_str(),
            request.clone(),
            2,
        )
        .unwrap();
    assert_eq!(
        receipt.state,
        axocoatl_session::control_command::ControlCommandState::Settled,
        "{receipt:?}"
    );
    let driver = controller
        .prepare_native_control_driver(
            &request.command_id,
            repository.clone(),
            bus.clone(),
            factory.clone(),
        )
        .unwrap()
        .unwrap();
    assert!(controller
        .prepare_native_control_driver(
            &request.command_id,
            repository.clone(),
            bus.clone(),
            factory.clone()
        )
        .unwrap()
        .is_none());
    let outcome = driver.run().await.unwrap();
    assert_eq!(
        outcome.snapshot.contract().state(),
        Some(LogicalTurnState::NeedsAttention)
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let replay = f
        .registry
        .submit_human_action(
            f.request.session_id.as_str(),
            f.request.turn_id.as_str(),
            request.clone(),
            3,
        )
        .unwrap();
    assert_eq!(replay, receipt);
    assert!(controller
        .prepare_native_control_driver(&request.command_id, repository, bus, factory)
        .unwrap()
        .is_none());
    let snapshot = controller.snapshot().unwrap();
    let latest = snapshot
        .contract()
        .activations()
        .iter()
        .rev()
        .find(|item| item.activation.node_id.as_str() == "node-0")
        .unwrap();
    assert_eq!(latest.activation.generation, 2);
    assert_eq!(latest.input.grant.as_ref().unwrap().revision, 1);
}
#[tokio::test]
async fn dropped_control_handoff_interrupts_epoch_and_receipt_retry_never_replays_it() {
    let f = native_fixture().await;
    let (controller, repository) = begin(&f, &f.request);
    let calls = Arc::new(AtomicUsize::new(0));
    let factory = Arc::new(RefusingFactory(calls.clone()));
    let bus = crate::stream::StreamBus::new(64);
    let NativeFirstTurnStart::Prepared(first) = finish_owned_setup(
        &f.registry,
        controller.clone(),
        repository.clone(),
        &f.request.source().unwrap(),
        bus.clone(),
        factory.clone(),
    )
    .unwrap() else {
        panic!("expected first owned driver")
    };
    first.run().await.unwrap();
    let request = continue_request(&controller, "continue-before-lost-host");
    let receipt = f
        .registry
        .submit_human_action(
            f.request.session_id.as_str(),
            f.request.turn_id.as_str(),
            request.clone(),
            2,
        )
        .unwrap();
    assert_eq!(
        receipt.state,
        axocoatl_session::control_command::ControlCommandState::Settled,
        "{receipt:?}"
    );
    let driver = controller
        .prepare_native_control_driver(
            &request.command_id,
            repository.clone(),
            bus.clone(),
            factory.clone(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        controller.live_owned_turn().unwrap(),
        Some(f.request.turn_id.clone())
    );
    assert_eq!(
        f.registry.live_native_turns().unwrap(),
        vec![(
            f.request.session_id.as_str().to_owned(),
            f.request.turn_id.as_str().to_owned()
        )]
    );
    drop(driver);
    assert_eq!(controller.live_owned_turn().unwrap(), None);
    assert!(f.registry.live_native_turns().unwrap().is_empty());
    let snapshot = controller.snapshot().unwrap();
    assert_eq!(
        snapshot.contract().state(),
        Some(LogicalTurnState::NeedsAttention)
    );
    assert_eq!(
        snapshot.contract().epochs().last().unwrap().state,
        EpochState::Interrupted
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the lost handoff did not reach its factory"
    );
    f.registry
        .submit_human_action(
            f.request.session_id.as_str(),
            f.request.turn_id.as_str(),
            request.clone(),
            3,
        )
        .unwrap();
    assert!(controller
        .prepare_native_control_driver(&request.command_id, repository, bus, factory)
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn environment_change_requires_no_unfinished_native_turn_and_keeps_history() {
    let f = native_fixture().await;
    f.registry
        .require_native_environment_change_ready(f.request.session_id.as_str())
        .unwrap();
    let (controller, repository) = begin(&f, &f.request);
    assert!(f
        .registry
        .require_native_environment_change_ready(f.request.session_id.as_str())
        .is_err());
    // A live, registered turn never admits the same-generation repair.
    assert!(f
        .registry
        .require_native_environment_retry_ready(f.request.session_id.as_str())
        .is_err());
    let calls = Arc::new(AtomicUsize::new(0));
    let factory = Arc::new(RefusingFactory(calls));
    let bus = crate::stream::StreamBus::new(64);
    let NativeFirstTurnStart::Prepared(first) = finish_owned_setup(
        &f.registry,
        controller.clone(),
        repository,
        &f.request.source().unwrap(),
        bus,
        factory,
    )
    .unwrap() else {
        panic!("expected native driver")
    };
    first.run().await.unwrap();
    assert!(f
        .registry
        .require_native_environment_change_ready(f.request.session_id.as_str())
        .is_err());
    // Still registered with its repository owner: repair stays refused.
    assert!(f
        .registry
        .require_native_environment_retry_ready(f.request.session_id.as_str())
        .is_err());
    f.registry
        .request_human_turn_stop(f.request.session_id.as_str(), f.request.turn_id.as_str())
        .unwrap();
    f.registry
        .require_native_environment_change_ready(f.request.session_id.as_str())
        .unwrap();
    assert!(f
        .registry
        .history_snapshot(f.request.session_id.as_str())
        .unwrap()
        .unwrap()
        .get(f.request.turn_id.as_str())
        .is_some());
}

#[path = "bootstrap_native_coordinator_tests.rs"]
mod coordinator_tests;

#[path = "bootstrap_native_ways_admission_tests.rs"]
mod ways_admission_tests;

#[path = "bootstrap_native_model_tests.rs"]
mod model_tests;

#[tokio::test]
async fn late_stop_after_attention_releases_exact_owner_for_rewind_and_reacquisition() {
    use crate::bootstrap::session_control_execution::settle_closed_native_control;
    use axocoatl_memory::legacy_conversation::ToolReplayPolicy;
    let f = native_fixture().await;
    let (controller, repository) = begin(&f, &f.request);
    let bus = crate::stream::StreamBus::new(64);
    let mut events = bus.subscribe();
    let factory = Arc::new(RefusingFactory(Arc::new(AtomicUsize::new(0))));
    let NativeFirstTurnStart::Prepared(prepared) = finish_owned_setup(
        &f.registry,
        controller.clone(),
        repository,
        &f.request.source().unwrap(),
        bus.clone(),
        factory,
    )
    .unwrap() else {
        panic!("owned driver")
    };
    assert!(controller.has_owned_execution().unwrap());
    assert!(!settle_closed_native_control(
        &f.registry,
        &bus,
        f.request.session_id.as_str(),
        &f.request.turn_id
    )
    .unwrap());
    assert!(f.repository.operation.try_lock().is_err());
    let outcome = prepared.run().await.unwrap();
    assert_eq!(
        outcome.snapshot.contract().state(),
        Some(LogicalTurnState::NeedsAttention)
    );
    assert!(!controller.has_owned_execution().unwrap());
    assert!(!settle_closed_native_control(
        &f.registry,
        &bus,
        f.request.session_id.as_str(),
        &f.request.turn_id
    )
    .unwrap());
    assert!(f.repository.operation.try_lock().is_err());
    f.registry
        .request_human_turn_stop(f.request.session_id.as_str(), f.request.turn_id.as_str())
        .unwrap();
    assert_eq!(
        controller.snapshot().unwrap().contract().state(),
        Some(LogicalTurnState::Cancelled)
    );
    assert!(settle_closed_native_control(
        &f.registry,
        &bus,
        f.request.session_id.as_str(),
        &f.request.turn_id
    )
    .unwrap());
    assert!(
        f.repository.owner.execution_lease().await.is_err(),
        "old physical capability remains retired"
    );
    assert_eq!(f.repository.sandbox.stop_calls.load(Ordering::SeqCst), 0);
    let operation = f
        .repository
        .operation
        .try_lock()
        .expect("Rewind can acquire the actual Workspace operation gate after late Stop");
    let token = f
        .registry
        .session_team_token(f.request.session_id.as_str())
        .unwrap();
    let rewind = f
        .registry
        .with_session_team_stores(&token, |canonical, content, memory| {
            memory
                .rewind_session(
                    canonical,
                    content,
                    None,
                    &[
                        NodeConversationId::new("conversation-0").unwrap(),
                        NodeConversationId::new("conversation-1").unwrap(),
                    ],
                    ToolReplayPolicy::CompleteNativeGroups,
                )
                .map_err(|error| crate::DaemonError::SessionConflict(error.to_string()))
        })
        .unwrap();
    assert!(rewind
        .superseded_turn_ids
        .contains(&f.request.turn_id.as_str().to_owned()));
    drop(operation);
    let revision = controller.snapshot().unwrap().contract().revision();
    assert!(settle_closed_native_control(
        &f.registry,
        &bus,
        f.request.session_id.as_str(),
        &f.request.turn_id
    )
    .unwrap());
    assert_eq!(
        controller.snapshot().unwrap().contract().revision(),
        revision
    );
    let mut cancelled = false;
    while let Ok(frame) = events.try_recv() {
        if matches!(frame, crate::stream::StreamFrame::SessionCancelled { ref session, ref turn_id, .. } if session == f.request.session_id.as_str() && turn_id == f.request.turn_id.as_str())
        {
            cancelled = true;
        }
    }
    assert!(
        cancelled,
        "late Stop publishes its actual canonical disposition"
    );
    let reacquisition = f
        .registry
        .prepare_reacquisition(f.request.session_id.as_str())
        .unwrap();
    let owner = f.repository.owner.reacquire_between_turns().await.unwrap();
    f.registry
        .complete_reacquisition(reacquisition, owner.clone())
        .unwrap();
    assert!(f.repository.operation.try_lock().is_err());
    assert!(owner.execution_is_idle().unwrap());
}

#[tokio::test]
async fn standing_work_settles_closed_retained_controller_before_reserving_next_event() {
    use axocoatl_session::team_work::*;
    let mut f = native_fixture_with_invocations(4).await;
    let directory = tempfile::tempdir().unwrap();
    let mut inbox = TeamWorkInbox::open(directory.path()).unwrap();
    let binding = TeamWorkBinding {
        binding_id: "retained-owner".into(),
        binding_revision: 1,
        workspace_id: f.repository.owner.identity().owner().workspace_id.clone(),
        session_id: f.request.session_id.as_str().into(),
        team_revision: 1,
        grant_id: f.request.grants[0].id.clone(),
        grant_revision: f.request.grants[0].revision,
        source_id: "manual".into(),
        event_kind: "candidate_ready".into(),
    };
    inbox
        .configure_binding(
            0,
            ArmedTeamWorkBinding {
                binding: binding.clone(),
                source: TeamWorkSource::Manual,
                armed: true,
                instruction: "Inspect this exact candidate".into(),
                required_checks: vec![],
                grants: f
                    .request
                    .grants
                    .iter()
                    .map(|grant| TeamWorkGrantReference {
                        id: grant.id.clone(),
                        revision: grant.revision,
                        limits: grant.limits.clone(),
                        expires_at_ms: grant.expires_at_ms,
                    })
                    .collect(),
                authorized_at_ms: 1,
                source_after_turn: None,
            },
        )
        .unwrap();
    let event = TeamWorkEvent {
        source_id: "manual".into(),
        event_id: "first".into(),
        event_kind: "candidate_ready".into(),
        content_sha256: "a".repeat(64),
        correlation_id: "retained-owner".into(),
        caused_by_turn_id: None,
        subject: TeamWorkSubject {
            kind: "tree".into(),
            reference_id: "fixture".into(),
            version: "a".repeat(64),
        },
        evidence_refs: vec![],
    };
    let first = inbox
        .admit_bound(
            TeamWorkRequest {
                binding: binding.clone(),
                event: event.clone(),
            },
            2,
        )
        .unwrap();
    f.request.turn_id = LogicalTurnId::new(&first.turn_id).unwrap();
    f.request.request.turn_id = f.request.turn_id.clone();
    let source = f.request.source().unwrap();
    inbox
        .reserve_native_turn(&first.receipt_id, source)
        .unwrap();
    let allocations = inbox.native_allocations(&first.receipt_id).unwrap();
    let (controller, repository) = begin(&f, &f.request);
    controller
        .install_team_work_allocations(&allocations, &repository)
        .unwrap();
    f.registry
        .request_human_turn_stop(f.request.session_id.as_str(), f.request.turn_id.as_str())
        .unwrap();
    assert_eq!(
        controller.snapshot().unwrap().contract().state(),
        Some(LogicalTurnState::Cancelled)
    );
    let token = f
        .registry
        .session_team_token(f.request.session_id.as_str())
        .unwrap();
    let settled = f
        .registry
        .with_session_team_settlement_stores(&token, |canonical, held| {
            assert!(
                held.is_some(),
                "the actual closed controller is still retained"
            );
            assert!(
                canonical
                    .existing_component_namespace(
                        ExecutionComponent::ControlAuthority {
                            turn_id: f.request.turn_id.clone()
                        },
                        std::path::Path::new("control-authority.v1.json"),
                    )
                    .is_err(),
                "a second writer lease must remain forbidden"
            );
            crate::bootstrap::session_team_work::settle_work_allocations(
                canonical,
                held,
                &f.request.turn_id,
                &allocations,
                false,
            )
        })
        .unwrap()
        .expect("settlement uses the already-held authority");
    inbox
        .settle_native_budget(&first.receipt_id, &settled)
        .unwrap();
    let mut second_event = event;
    second_event.event_id = "second".into();
    let second = inbox
        .admit_bound(
            TeamWorkRequest {
                binding,
                event: second_event,
            },
            3,
        )
        .unwrap();
    let reserved = inbox
        .reserve_native_turn(
            &second.receipt_id,
            serde_json::json!({"turn_id":second.turn_id,"source":"second reviewed event"})
                .to_string(),
        )
        .unwrap();
    assert!(reserved
        .allocations
        .iter()
        .all(|allocation| allocation.consumed_before == Default::default()));
    assert_eq!(controller.snapshot().unwrap().turn_id(), &f.request.turn_id);
}

#[tokio::test]
async fn live_host_grant_review_uses_existing_authority_lease_before_activation() {
    let fixture = native_fixture().await;
    let (controller, repository) = begin(&fixture, &fixture.request);
    let prepared = finish_owned_setup(
        &fixture.registry,
        controller,
        repository,
        &fixture.request.source().unwrap(),
        crate::stream::StreamBus::new(64),
        Arc::new(RefusingFactory(Arc::new(AtomicUsize::new(0)))),
    )
    .unwrap();
    let token = fixture
        .registry
        .session_team_token(fixture.request.session_id.as_str())
        .unwrap();
    fixture
        .registry
        .with_session_team_grant_stores(&token, |canonical, content, held| {
            assert!(
                held.is_some(),
                "the prepared native controller owns authority"
            );
            assert!(
                canonical
                    .existing_component_namespace(
                        ExecutionComponent::ControlAuthority {
                            turn_id: fixture.request.turn_id.clone()
                        },
                        Path::new("control-authority.v1.json"),
                    )
                    .is_err(),
                "the exclusive writer is not relaxed for read access"
            );
            let before = canonical
                .snapshot(&fixture.request.turn_id)
                .unwrap()
                .contract()
                .revision();
            let review = crate::session_dispatch::retained_grant_view(
                canonical,
                content,
                &fixture.request.turn_id,
                held,
            )
            .unwrap();
            assert_eq!(
                review.grants.len(),
                2,
                "approved grants are visible before activation admission"
            );
            assert!(review.proposals.is_empty());
            assert_eq!(
                canonical
                    .snapshot(&fixture.request.turn_id)
                    .unwrap()
                    .contract()
                    .revision(),
                before,
                "review does not create work"
            );
            Ok(())
        })
        .unwrap();
    drop(prepared);
}
