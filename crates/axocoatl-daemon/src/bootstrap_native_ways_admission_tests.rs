//! Retained isolated admission must never enable the cooperative runtime.
use super::*;
use crate::bootstrap::native_ways::{NativeWaysAdmission, NativeWaysCandidateAdmission};
use axocoatl_session::execution_content::{
    ExecutionModelRef, TurnAdmissionContent, TurnAdmissionNodeInput,
};

fn admit_ways(
    f: &NativeFixture,
) -> (
    crate::session_dispatch::SessionDispatchController,
    EvidenceRef,
    NativeWaysAdmission,
    TurnGraphSnapshot,
    Vec<TurnAdmissionNodeInput>,
) {
    let token = f
        .registry
        .session_team_token(f.request.session_id.as_str())
        .unwrap();
    let (admission, graph, inputs) = f
        .registry
        .with_session_team_stores(&token, |canonical, content, _| {
            let team = SessionTeamStore::open_owned(
                canonical
                    .component_namespace(ExecutionComponent::SessionTeam)
                    .unwrap(),
                canonical,
                content,
                None,
            )
            .unwrap();
            let current = team.current().unwrap().unwrap().clone();
            drop(team);
            let request = content
                .retain_request(f.request.request.clone())
                .unwrap()
                .reference()
                .clone();
            let mut nodes = vec![];
            let mut candidates = vec![];
            let mut inputs = vec![];
            for (index, slot) in current.graph.slots.iter().enumerate() {
                let ActivationEvidenceContent::Grant { policy } = &content
                    .resolve_activation_evidence(slot.grant.as_ref().unwrap())
                    .unwrap()
                else {
                    panic!("retained grant")
                };
                let grant = GrantSnapshotRef {
                    grant_id: GrantId::new(&policy.id).unwrap(),
                    revision: policy.revision,
                    evidence: slot.grant.clone().unwrap(),
                };
                let ActivationEvidenceContent::Definition { profile, .. } = &content
                    .resolve_activation_evidence(&slot.definition.snapshot)
                    .unwrap()
                else {
                    panic!("retained definition")
                };
                let activation = ActivationRef {
                    session_id: f.request.session_id.clone(),
                    turn_id: f.request.turn_id.clone(),
                    execution_epoch_id: f.request.epoch_id.clone(),
                    node_id: slot.node_id.clone(),
                    activation_id: ActivationId::new(format!("way-{index}")).unwrap(),
                    generation: 1,
                };
                candidates.push(NativeWaysCandidateAdmission {
                    index,
                    run_id: crate::attempts::run_id(f.request.session_id.as_str(), index),
                    activation,
                    definition: slot.definition.clone(),
                    model: ExecutionModelRef {
                        provider_id: profile.provider.clone(),
                        model_id: profile.model.clone(),
                        configuration_ref: slot.definition.snapshot.clone(),
                    },
                    grant: grant.clone(),
                });
                nodes.push(GraphNode {
                    node_id: slot.node_id.clone(),
                    slot_id: slot.slot_id.clone(),
                    definition: slot.definition.clone(),
                    conversation_id: slot.conversation_id.clone(),
                    starting_savepoint: ConversationSavepoint::Empty,
                    required: true,
                });
                inputs.push(TurnAdmissionNodeInput {
                    node_id: slot.node_id.clone(),
                    guidance: vec![request.clone()],
                    attachments: vec![],
                    budget: slot.budget.clone(),
                    grant,
                });
            }
            let graph = TurnGraphSnapshot {
                snapshot_id: f.request.graph_snapshot_id.clone(),
                revision: 1,
                nodes,
                dependencies: vec![],
                conditions: vec![],
            };
            let admission = NativeWaysAdmission {
                schema_version: 1,
                session_id: f.request.session_id.as_str().into(),
                set_id: "owned-fixture-set".into(),
                source_turn_id: f.request.turn_id.clone(),
                request: request.clone(),
                candidates,
            };
            content
                .retain_turn_admission(
                    canonical,
                    TurnAdmissionContent {
                        schema_version: 1,
                        command_id: f.request.command_id.clone(),
                        turn_id: f.request.turn_id.clone(),
                        epoch_id: f.request.epoch_id.clone(),
                        source: serde_json::to_string(&admission).unwrap(),
                        graph: graph.clone(),
                        request,
                        nodes: inputs.clone(),
                    },
                )
                .unwrap();
            Ok((admission, graph, inputs))
        })
        .unwrap();
    let pending = f
        .registry
        .prepare_first_turn(f.request.session_id.as_str())
        .unwrap();
    let (controller, repository) = f
        .registry
        .begin_first_turn_checked(
            &pending,
            f.repository.owner.clone(),
            SuccessorTurn {
                command_id: f.request.command_id.clone(),
                turn_id: f.request.turn_id.clone(),
                epoch_id: f.request.epoch_id.clone(),
                graph: graph.clone(),
                request: f.request.request.clone(),
            },
            |_, _, _| Ok(()),
        )
        .unwrap();
    for grant in &f.request.grants {
        controller.install_grant(grant.clone()).unwrap();
    }
    (controller, repository, admission, graph, inputs)
}

fn start_way(
    f: &NativeFixture,
    controller: &crate::session_dispatch::SessionDispatchController,
    repository: &EvidenceRef,
    candidate: &NativeWaysCandidateAdmission,
    node: &GraphNode,
    input: &TurnAdmissionNodeInput,
) {
    controller
        .append_host_event(TurnContractEnvelope {
            schema_version: TURN_CONTRACT_SCHEMA_VERSION,
            command_id: CommandId::new(format!("start-owned-way-{}", candidate.index)).unwrap(),
            expected_revision: controller.snapshot().unwrap().contract().revision(),
            session_id: f.request.session_id.clone(),
            turn_id: f.request.turn_id.clone(),
            event: TurnContractEvent::StartActivation {
                input: Box::new(ActivationInputManifest {
                    manifest_id: InputManifestId::new(format!(
                        "owned-way-input-{}",
                        candidate.index
                    ))
                    .unwrap(),
                    activation: candidate.activation.clone(),
                    definition: node.definition.clone(),
                    conversation_id: node.conversation_id.clone(),
                    starting_savepoint: ConversationSavepoint::Empty,
                    parents: vec![],
                    guidance: input.guidance.clone(),
                    attachments: vec![],
                    repository: RepositoryInput::Recorded {
                        snapshot: repository.clone(),
                    },
                    budget: input.budget.clone(),
                    grant: Some(input.grant.clone()),
                    revision_context: None,
                }),
            },
        })
        .unwrap();
}

#[tokio::test]
async fn isolated_source_keeps_inspection_but_refuses_cooperative_reexecution() {
    let f = native_fixture().await;
    let (controller, repository, admission, graph, inputs) = admit_ways(&f);
    start_way(
        &f,
        &controller,
        &repository,
        &admission.candidates[0],
        &graph.nodes[0],
        &inputs[0],
    );
    let view = controller.control_plane().unwrap();
    let capabilities = &view.nodes[0].activations[0].capabilities;
    assert!(capabilities.inspect);
    assert!(!capabilities.retry.enabled && capabilities.retry.reason.contains("isolated Way"));
    assert!(!capabilities.revise.enabled && capabilities.revise.reason.contains("isolated Way"));
    assert!(!view.turn_controls.unwrap().continue_turn.enabled);
    let factory = Arc::new(RefusingFactory(Arc::new(AtomicUsize::new(0))));
    let error = controller
        .prepare_native_host_driver(
            &serde_json::to_string(&admission).unwrap(),
            repository,
            crate::stream::StreamBus::new(16),
            factory,
        )
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("isolated Ways"));
    assert_eq!(
        controller
            .snapshot()
            .unwrap()
            .contract()
            .activations()
            .len(),
        1
    );
    controller
        .request_human_turn_stop(f.request.session_id.as_str(), f.request.turn_id.as_str())
        .unwrap();
    assert!(controller.has_owned_execution().unwrap());
    assert!(
        !crate::bootstrap::session_control_execution::settle_closed_native_control(
            &f.registry,
            &crate::stream::StreamBus::new(16),
            f.request.session_id.as_str(),
            &f.request.turn_id,
        )
        .unwrap()
    );
    assert!(
        f.repository.operation.try_lock().is_err(),
        "isolated Ways retain their actual attempt cleanup owner"
    );
}

struct FiniteWayProvider;
#[async_trait::async_trait]
impl axocoatl_llm::LlmProvider for FiniteWayProvider {
    fn provider_id(&self) -> &str {
        "ollama"
    }
    fn model_id(&self) -> &str {
        "test-model"
    }
    fn capabilities(&self) -> axocoatl_llm::ProviderCapabilities {
        axocoatl_llm::ProviderCapabilities {
            streaming: true,
            ..Default::default()
        }
    }
    fn execution_bounds(
        &self,
        _: &axocoatl_llm::ChatRequest,
    ) -> Option<axocoatl_llm::ProviderExecutionBounds> {
        Some(axocoatl_llm::ProviderExecutionBounds {
            token_limit: 100,
            cost_microunits: 0,
            response_bytes: 4096,
        })
    }
    async fn chat(
        &self,
        _: axocoatl_llm::ChatRequest,
    ) -> std::result::Result<axocoatl_llm::ChatResponse, axocoatl_llm::ProviderError> {
        unreachable!()
    }
    async fn chat_stream(
        &self,
        _: axocoatl_llm::ChatRequest,
    ) -> std::result::Result<
        std::pin::Pin<
            Box<
                dyn tokio_stream::Stream<
                        Item = std::result::Result<
                            axocoatl_llm::StreamEvent,
                            axocoatl_llm::ProviderError,
                        >,
                    > + Send,
            >,
        >,
        axocoatl_llm::ProviderError,
    > {
        Ok(Box::pin(tokio_stream::iter(vec![
            Ok(axocoatl_llm::StreamEvent::TextDelta {
                delta: "Accepted fixture candidate".into(),
            }),
            Ok(axocoatl_llm::StreamEvent::Usage(
                axocoatl_core::TokenUsageStats::new(10, 4),
            )),
            Ok(axocoatl_llm::StreamEvent::Done {
                finish_reason: axocoatl_llm::FinishReason::Stop,
            }),
        ])))
    }
}
struct WayCounter;
impl axocoatl_token::TokenCounter for WayCounter {
    fn count_text(&self, text: &str) -> usize {
        text.len() / 4 + 1
    }
    fn count_messages(&self, messages: &[axocoatl_core::ChatMessage]) -> usize {
        messages.len() * 10
    }
    fn count_tool_definition(&self, value: &serde_json::Value) -> usize {
        self.count_text(&value.to_string())
    }
}

#[tokio::test]
async fn failed_way_keeps_actual_accepted_peer_available_to_existing_keep_validator() {
    // Model dispatch consumes the same explicit invocation allowance as tools.
    let f = native_fixture_with_invocations(4).await;
    let (controller, repository, admission, graph, inputs) = admit_ways(&f);
    assert!(graph.nodes.iter().all(|node| node.required));
    assert_eq!(inputs.len(), admission.candidates.len());
    for (index, input) in inputs.iter().enumerate() {
        start_way(
            &f,
            &controller,
            &repository,
            &admission.candidates[index],
            &graph.nodes[index],
            input,
        );
    }
    let resources = controller
        .with_team_stores(|_, content, _| {
            let ActivationEvidenceContent::Definition {
                profile,
                configuration,
                ..
            } = &content
                .resolve_activation_evidence(&graph.nodes[0].definition.snapshot)
                .unwrap()
            else {
                panic!("definition");
            };
            Ok(AutonomousActivationResources {
                config: serde_json::from_str(configuration).unwrap(),
                profile: profile.clone(),
                provider: Arc::new(FiniteWayProvider),
                counter: Arc::new(WayCounter),
                tools: Arc::new(axocoatl_tools::ToolExecutor::new()),
            })
        })
        .unwrap();
    let settled = controller
        .prepare_repository_activation(
            admission.candidates[0].activation.clone(),
            resources,
            controller
                .repository_activation_resource(&repository)
                .unwrap(),
        )
        .unwrap()
        .run()
        .await
        .unwrap();
    assert!(settled.accepted, "{:?}", settled.failure);
    assert!(!controller
        .settle_native_way_task(&admission.candidates[0].activation, None)
        .unwrap());
    assert!(!controller
        .settle_native_way_task(
            &admission.candidates[1].activation,
            Some("controlled preparation failure before provider dispatch")
        )
        .unwrap());
    let snapshot = controller.snapshot().unwrap();
    assert_eq!(
        snapshot.contract().state(),
        Some(LogicalTurnState::NeedsAttention)
    );
    assert_eq!(snapshot.contract().current_accepted_activations().len(), 1);
    assert_eq!(
        snapshot.contract().activations()[1].state,
        ActivationState::Failed
    );
    controller
        .with_team_stores(|_, content, memory| {
            let (output, checkpoint) = crate::bootstrap::native_ways::retained_native_candidate(
                &snapshot,
                content,
                memory,
                &admission.candidates[0],
                "Accepted fixture candidate",
            )
            .unwrap();
            assert_eq!(&output, settled.output.reference());
            assert_eq!(
                Some(&checkpoint),
                snapshot.contract().current_accepted_activations()[0]
                    .checkpoint
                    .as_ref()
            );
            assert!(crate::bootstrap::native_ways::retained_native_candidate(
                &snapshot,
                content,
                memory,
                &admission.candidates[1],
                "Accepted fixture candidate",
            )
            .is_err());
            assert!(crate::bootstrap::native_ways::retained_native_candidate(
                &snapshot,
                content,
                memory,
                &admission.candidates[0],
                "Unobserved answer",
            )
            .is_err());
            Ok(())
        })
        .unwrap();
    assert!(controller.has_owned_execution().unwrap());
    assert!(
        f.repository.operation.try_lock().is_err(),
        "Compare and Keep retain the actual isolated owner"
    );
}

/// Numerical archive limits and Git identifiers below are fixture data only.
fn no_keep_record(
    admission: &NativeWaysAdmission,
) -> axocoatl_session::ways_decision::WaysDecisionRecord {
    use axocoatl_session::execution_content::ExecutionUsage;
    use axocoatl_session::ways_decision::*;
    let evidence = |value: &str| EvidenceRef::new(value).unwrap();
    let set_id = WaysSetId(evidence(&admission.set_id));
    WaysDecisionRecord {
        schema_version: WAYS_DECISION_SCHEMA_VERSION,
        retention_limits_version: WAYS_RETENTION_LIMITS_VERSION,
        decision_id: DecisionId(evidence("fixture-no-keep-decision")),
        session_id: SessionId::new(&admission.session_id).unwrap(),
        source_turn_id: admission.source_turn_id.clone(),
        set_id: set_id.clone(),
        task: ReviewText::complete("Fixture exploration"),
        starting_repository: WaysRepositoryIdentity {
            workspace_id: evidence("fixture-workspace"),
            repository_ref: evidence("fixture-repository"),
            commit_oid: "1".repeat(40),
            tree_oid: "2".repeat(40),
        },
        candidates: admission
            .candidates
            .iter()
            .map(|candidate| WaysCandidateEvidence {
                id: WaysCandidateId {
                    set_id: set_id.clone(),
                    index: candidate.index as u32,
                },
                agent: candidate.definition.definition_id.clone(),
                model: Recorded::Available {
                    value: candidate.model.clone(),
                },
                isolation: Recorded::Available {
                    value: "controlled-fixture".into(),
                },
                terminal: WaysTerminalState::Failed,
                failure_or_no_change_reason: Recorded::Available {
                    value: ReviewText::complete(
                        "Fixture preparation failed before provider dispatch",
                    ),
                },
                outcome: ReviewText::complete("No accepted output"),
                route: ReviewText::complete("No provider dispatch"),
                tools: RetainedItems {
                    items: vec![],
                    original_count: 0,
                },
                changed_paths: RetainedItems {
                    items: vec![],
                    original_count: 0,
                },
                patch: Recorded::Unavailable {
                    reason: UnavailableReason::NotProduced,
                },
                reviewable_diff: ReviewText::complete(""),
                checks: vec![],
                usage: WaysUsage {
                    measurement_id: evidence(&format!("fixture-usage-{}", candidate.index)),
                    tokens: ExecutionUsage::Measured {
                        usage: axocoatl_core::TokenUsageStats::default(),
                    },
                    cost_usd_known_subtotal: 0.0,
                    cost_complete: true,
                },
            })
            .collect(),
        judge: None,
        shared_usage: vec![],
        human_decision: WaysHumanDecision {
            decision_intent_id: evidence("fixture-explicit-no-keep"),
            decided_at_unix_ms: 1,
            choice: WaysHumanChoice::NoKeep,
        },
        application: WaysApplicationOutcome::NotStarted,
        selected_session_turn: None,
        cleanup: WaysCleanupEvidence {
            inventory_complete: true,
            targets: vec![],
            completed_at_unix_ms: None,
        },
    }
}

#[tokio::test]
async fn cleaned_incomplete_ways_require_exact_decision_and_preserve_explicit_stop() {
    use axocoatl_session::ways_decision::*;
    use axocoatl_session::ways_decision_store::WaysDecisionStore;
    for explicit_stop in [false, true] {
        let mut f = native_fixture().await;
        f.request.turn_id = LogicalTurnId::new(format!(
            "ways-{}",
            crate::attempts::set_key("owned-fixture-set")
        ))
        .unwrap();
        f.request.request.turn_id = f.request.turn_id.clone();
        let (controller, repository, admission, graph, inputs) = admit_ways(&f);
        assert_eq!(inputs.len(), admission.candidates.len());
        for (index, input) in inputs.iter().enumerate() {
            start_way(
                &f,
                &controller,
                &repository,
                &admission.candidates[index],
                &graph.nodes[index],
                input,
            );
        }
        for candidate in &admission.candidates {
            controller
                .settle_native_way_task(
                    &candidate.activation,
                    Some("Fixture preparation failed before provider dispatch"),
                )
                .unwrap();
        }
        assert_eq!(
            controller.snapshot().unwrap().contract().state(),
            Some(LogicalTurnState::NeedsAttention)
        );
        if explicit_stop {
            controller
                .request_human_turn_stop(f.request.session_id.as_str(), f.request.turn_id.as_str())
                .unwrap();
        }
        let mut record = no_keep_record(&admission);
        controller
            .with_team_stores(|canonical, _, _| {
                let mut archive = WaysDecisionStore::open_owned(
                    canonical
                        .component_namespace(ExecutionComponent::WaysDecisions)
                        .unwrap(),
                    canonical,
                    WaysRetentionLimits {
                        version: 1,
                        field_bytes: 16 * 1024,
                        record_bytes: 128 * 1024,
                        aggregate_bytes: 1024 * 1024,
                        records: 4,
                        candidates: 4,
                        items_per_field: 16,
                    },
                )
                .unwrap();
                archive.freeze(record.clone(), vec![]).unwrap();
                Ok(())
            })
            .unwrap();
        let cleanup = f
            .registry
            .prepare_session_cleanup(f.request.session_id.as_str(), Duration::from_secs(2))
            .await
            .unwrap();
        f.registry.complete_session_cleanup(&cleanup).unwrap();
        drop(cleanup);
        drop(controller);
        let ownership = Arc::new(
            axocoatl_session::execution_ownership::UpgradedFormatOwnership::open(
                f.repository._data.path(),
            )
            .unwrap(),
        );
        let canonical = SessionExecutionStore::open_existing(
            ownership,
            f.repository.owner.identity().owner().clone(),
        )
        .unwrap();
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
        let mut stores = Some(RetainedSessionStores {
            canonical,
            content,
            memory,
        });
        f.registry
            .retain_existing_lifecycle_session(&mut stores, false)
            .unwrap();
        let token = f
            .registry
            .session_team_token(f.request.session_id.as_str())
            .unwrap();
        let before = f
            .registry
            .with_session_team_stores(&token, |canonical, _, _| {
                Ok(canonical
                    .snapshot(&f.request.turn_id)
                    .unwrap()
                    .contract()
                    .revision())
            })
            .unwrap();
        if explicit_stop {
            f.registry
                .close_cleaned_native_ways(f.request.session_id.as_str(), &admission.set_id, None)
                .unwrap();
        } else {
            assert!(f
                .registry
                .close_cleaned_native_ways(f.request.session_id.as_str(), &admission.set_id, None)
                .is_err());
            assert!(f
                .registry
                .close_cleaned_native_ways(
                    f.request.session_id.as_str(),
                    &admission.set_id,
                    Some(&record)
                )
                .is_err());
            f.registry
                .with_session_team_stores(&token, |canonical, _, _| {
                    assert_eq!(
                        canonical
                            .snapshot(&f.request.turn_id)
                            .unwrap()
                            .contract()
                            .revision(),
                        before
                    );
                    let mut archive = WaysDecisionStore::open_configured(
                        canonical
                            .component_namespace(ExecutionComponent::WaysDecisions)
                            .unwrap(),
                        canonical,
                    )
                    .unwrap()
                    .unwrap();
                    record.application = WaysApplicationOutcome::NoKeepRecorded {
                        receipt_ref: EvidenceRef::new("fixture-no-keep-applied").unwrap(),
                        recorded_at_unix_ms: 2,
                    };
                    archive.record_progress(record.clone()).unwrap();
                    record = archive.get(&record.decision_id).unwrap().unwrap();
                    Ok(())
                })
                .unwrap();
            let mut foreign = record.clone();
            foreign.source_turn_id = LogicalTurnId::new("another-source").unwrap();
            assert!(f
                .registry
                .close_cleaned_native_ways(
                    f.request.session_id.as_str(),
                    &admission.set_id,
                    Some(&foreign)
                )
                .is_err());
            f.registry
                .close_cleaned_native_ways(
                    f.request.session_id.as_str(),
                    &admission.set_id,
                    Some(&record),
                )
                .unwrap();
        }
        let expected = if explicit_stop {
            LogicalTurnState::Cancelled
        } else {
            LogicalTurnState::Finished
        };
        let revision = f
            .registry
            .with_session_team_stores(&token, |canonical, _, memory| {
                let snapshot = canonical.snapshot(&f.request.turn_id).unwrap();
                assert_eq!(snapshot.contract().state(), Some(expected));
                assert_eq!(snapshot.contract().activations().len(), 2);
                assert!(snapshot
                    .contract()
                    .current_accepted_activations()
                    .is_empty());
                assert!(memory.promotion(&snapshot).unwrap().is_some());
                Ok(snapshot.contract().revision())
            })
            .unwrap();
        f.registry
            .close_cleaned_native_ways(
                f.request.session_id.as_str(),
                &admission.set_id,
                Some(&record),
            )
            .unwrap();
        f.registry
            .with_session_team_stores(&token, |canonical, _, _| {
                assert_eq!(
                    canonical
                        .snapshot(&f.request.turn_id)
                        .unwrap()
                        .contract()
                        .revision(),
                    revision
                );
                Ok(())
            })
            .unwrap();
    }
}

#[tokio::test]
async fn isolated_way_tool_observations_keep_matching_legacy_occurrences_after_durable_recording() {
    use crate::stream::{StreamBus, StreamFrame};
    use axocoatl_actor::AgentStreamChunk;
    let f = native_fixture_with_invocations(4).await;
    let (controller, repository, admission, graph, inputs) = admit_ways(&f);
    let candidate = &admission.candidates[0];
    start_way(
        &f,
        &controller,
        &repository,
        candidate,
        &graph.nodes[0],
        &inputs[0],
    );
    let bus = StreamBus::new(32);
    let mut subscription = bus.subscribe();
    controller.attach_stream_bus(bus).unwrap();
    let resources = controller
        .with_team_stores(|_, content, _| {
            let ActivationEvidenceContent::Definition {
                profile,
                configuration,
                ..
            } = &content
                .resolve_activation_evidence(&graph.nodes[0].definition.snapshot)
                .unwrap()
            else {
                panic!("definition");
            };
            Ok(AutonomousActivationResources {
                config: serde_json::from_str(configuration).unwrap(),
                profile: profile.clone(),
                provider: Arc::new(FiniteWayProvider),
                counter: Arc::new(WayCounter),
                tools: Arc::new(axocoatl_tools::ToolExecutor::new()),
            })
        })
        .unwrap();
    // Bind the real native activation. This is an observation-adapter test:
    // no provider or tool is executed, and no output is accepted.
    let prepared = controller
        .prepare_repository_activation(
            candidate.activation.clone(),
            resources,
            controller
                .repository_activation_resource(&repository)
                .unwrap(),
        )
        .unwrap();
    let observer = controller.stream_observer_for_test(candidate.activation.clone());
    for group in 1..=2 {
        observer
            .observe(&AgentStreamChunk::ToolCallStarted {
                source_agent: None,
                id: "reused-provider-call".into(),
                name: "read_file".into(),
                arguments: serde_json::json!({"path":"fixture.txt"}),
                provider_arguments: serde_json::json!({"path":"fixture.txt"}),
                provider_metadata: Default::default(),
                assistant_content: None,
                provider_response_group: group,
                provider_call_index: 0,
                provider_call_count: 1,
            })
            .unwrap();
        observer
            .observe(&AgentStreamChunk::ToolCallResult {
                source_agent: None,
                id: "reused-provider-call".into(),
                name: "read_file".into(),
                result: serde_json::json!({"error":"observation fixture; no dispatch"}),
                is_error: true,
            })
            .unwrap();
    }
    let mut legacy = vec![];
    let mut native = vec![];
    while let Ok(frame) = subscription.try_recv() {
        match frame {
            StreamFrame::ToolCall {
                workflow,
                turn_id,
                call_id,
                occurrence,
                phase,
                coordination_generation,
                ..
            } => {
                assert_eq!(workflow, candidate.run_id);
                assert_eq!(
                    turn_id.as_deref(),
                    Some(candidate.activation.turn_id.as_str())
                );
                assert_eq!(call_id, "reused-provider-call");
                assert_eq!(coordination_generation, Some(1));
                legacy.push((phase, occurrence));
            }
            StreamFrame::ActivationStream { event } => native.push(event),
            _ => {}
        }
    }
    assert_eq!(
        legacy,
        vec![
            ("start".to_owned(), 0u64),
            ("result".to_owned(), 0u64),
            ("start".to_owned(), 1u64),
            ("result".to_owned(), 1u64)
        ]
    );
    assert_eq!(native.len(), 4);
    controller
        .with_team_stores(|canonical, content, _| {
            let snapshot = canonical.snapshot(&candidate.activation.turn_id).unwrap();
            assert_eq!(
                content
                    .activation_stream(&snapshot, &candidate.activation)
                    .unwrap(),
                native
            );
            assert_eq!(snapshot.contract().state(), Some(LogicalTurnState::Running));
            assert!(snapshot
                .contract()
                .current_accepted_activations()
                .is_empty());
            Ok(())
        })
        .unwrap();
    assert!(
        controller.control_plane().is_ok(),
        "tool publication must not poison the canonical controller mutex"
    );
    drop(prepared);
}

#[tokio::test]
async fn recovered_running_ways_freeze_interrupted_under_cleanup_lease_and_close_no_keep() {
    use crate::bootstrap::{ActiveAttemptRun, AxocoatlDaemon};
    use crate::git::{AttemptLaneState, AttemptLaneStatus, AttemptSet, AttemptSetState, Variant};
    use axocoatl_session::ways_decision::*;
    use axocoatl_session::ways_decision_store::WaysDecisionStore;
    let mut f = native_fixture().await;
    f.request.turn_id = LogicalTurnId::new(format!(
        "ways-{}",
        crate::attempts::set_key("owned-fixture-set")
    ))
    .unwrap();
    f.request.request.turn_id = f.request.turn_id.clone();
    let (controller, repository, admission, graph, inputs) = admit_ways(&f);
    assert_eq!(inputs.len(), admission.candidates.len());
    for (index, input) in inputs.iter().enumerate() {
        start_way(
            &f,
            &controller,
            &repository,
            &admission.candidates[index],
            &graph.nodes[index],
            input,
        );
    }
    // Native admission was durable, but no actor/provider/tool was launched.
    // Retire that process owner and reopen only the retained journals.
    let cleanup = f
        .registry
        .prepare_session_cleanup(f.request.session_id.as_str(), Duration::from_secs(2))
        .await
        .unwrap();
    f.registry.complete_session_cleanup(&cleanup).unwrap();
    drop(cleanup);
    drop(controller);
    let ownership = Arc::new(
        axocoatl_session::execution_ownership::UpgradedFormatOwnership::open(
            f.repository._data.path(),
        )
        .unwrap(),
    );
    let canonical = SessionExecutionStore::open_existing(
        ownership,
        f.repository.owner.identity().owner().clone(),
    )
    .unwrap();
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
    f.registry
        .retain_existing_lifecycle_session(
            &mut Some(RetainedSessionStores {
                canonical,
                content,
                memory,
            }),
            false,
        )
        .unwrap();

    // The physical operation lease is held by cleanup, while these persisted
    // Running records have no process-local attempt owner after recovery.
    let _cleanup_operation =
        tokio::time::timeout(Duration::from_secs(2), f.repository.operation.lock())
            .await
            .unwrap();
    assert!(f.repository.operation.try_lock().is_err());
    let directory = tempfile::tempdir().unwrap();
    let root = axocoatl_core::SecureDir::open_existing_all(directory.path()).unwrap();
    let set = AttemptSet {
        id: admission.set_id.clone(),
        session_id: admission.session_id.clone(),
        task: "Recovered exploration".into(),
        instruction: "Recovered exploration".into(),
        base_sha: "1".repeat(40),
        base_tree: "2".repeat(40),
        state: AttemptSetState::Running,
        kept_index: None,
        created_at: 1,
        lanes: admission
            .candidates
            .iter()
            .map(|candidate| Variant {
                index: candidate.index,
                branch: format!("fixture-{}", candidate.index),
                worktree: directory
                    .path()
                    .join(format!("way-{}", candidate.index))
                    .to_string_lossy()
                    .into_owned(),
                model: None,
                agent: None,
                provider: None,
            })
            .collect(),
    };
    for lane in &set.lanes {
        AxocoatlDaemon::write_host_json_file(
            &root,
            std::path::Path::new(&format!("state-{}.json", lane.index)),
            &AttemptLaneStatus {
                index: lane.index,
                state: AttemptLaneState::Running,
                error: None,
                started_at: Some(1),
                finished_at: None,
            },
        )
        .unwrap();
    }
    let release = Arc::new(tokio::sync::Notify::new());
    let task_release = release.clone();
    let mut live = ActiveAttemptRun::new(&set.id);
    live.tasks.push(tokio::spawn(async move {
        task_release.notified().await;
    }));
    let mut active = std::collections::HashMap::from([(set.session_id.clone(), live)]);
    assert!(
        AxocoatlDaemon::frozen_ways_lane_states_host(&root, &set, &active).is_err(),
        "a real registered task must be joined before decision evidence can freeze"
    );
    let live = active.remove(&set.session_id).unwrap();
    release.notify_one();
    for task in live.tasks {
        task.await.unwrap();
    }
    let states = AxocoatlDaemon::frozen_ways_lane_states_host(&root, &set, &active).unwrap();
    assert!(states
        .iter()
        .all(|state| state.state == AttemptLaneState::Interrupted
            && state.finished_at.is_some()
            && state.error.is_some()));
    // Projection does not rewrite raw historical Running evidence.
    let raw: AttemptLaneStatus =
        AxocoatlDaemon::read_host_json_file(&root, std::path::Path::new("state-0.json"))
            .unwrap()
            .unwrap();
    assert_eq!(raw.state, AttemptLaneState::Running);

    let token = f
        .registry
        .session_team_token(&admission.session_id)
        .unwrap();
    let mut record = no_keep_record(&admission);
    for candidate in &mut record.candidates {
        candidate.terminal = WaysTerminalState::Interrupted;
        candidate.failure_or_no_change_reason = Recorded::Available {
            value: ReviewText::complete(
                states
                    .iter()
                    .find(|state| state.index == candidate.id.index as usize)
                    .unwrap()
                    .error
                    .as_ref()
                    .unwrap(),
            ),
        };
    }
    let record = f
        .registry
        .with_session_team_stores(&token, |canonical, _, _| {
            let mut archive = WaysDecisionStore::open_owned(
                canonical
                    .component_namespace(ExecutionComponent::WaysDecisions)
                    .unwrap(),
                canonical,
                WaysRetentionLimits {
                    version: 1,
                    field_bytes: 16 * 1024,
                    record_bytes: 128 * 1024,
                    aggregate_bytes: 1024 * 1024,
                    records: 4,
                    candidates: 4,
                    items_per_field: 16,
                },
            )
            .unwrap();
            archive.freeze(record.clone(), vec![]).unwrap();
            record.application = WaysApplicationOutcome::NoKeepRecorded {
                receipt_ref: EvidenceRef::new("recovered-no-keep-receipt").unwrap(),
                recorded_at_unix_ms: 2,
            };
            archive.record_progress(record.clone()).unwrap();
            Ok(archive.get(&record.decision_id).unwrap().unwrap())
        })
        .unwrap();
    f.registry
        .close_cleaned_native_ways(&admission.session_id, &admission.set_id, Some(&record))
        .unwrap();
    f.registry
        .with_session_team_stores(&token, |canonical, _, memory| {
            let snapshot = canonical.snapshot(&admission.source_turn_id).unwrap();
            assert_eq!(
                snapshot.contract().state(),
                Some(LogicalTurnState::Finished)
            );
            assert!(snapshot
                .contract()
                .activations()
                .iter()
                .all(|activation| activation.state == ActivationState::Interrupted));
            assert!(snapshot
                .contract()
                .current_accepted_activations()
                .is_empty());
            assert!(memory.promotion(&snapshot).unwrap().is_some());
            Ok(())
        })
        .unwrap();
}

#[tokio::test]
async fn completed_ways_keep_knowledge_private_until_exact_candidate_is_selected() {
    use axocoatl_memory::knowledge::{
        KnowledgeDraft, KnowledgeKind, KnowledgeProvenance, KnowledgeStore, ProposalStatus,
    };
    let f = native_fixture_with_invocations(4).await;
    let (controller, repository, admission, graph, inputs) = admit_ways(&f);
    assert_eq!(admission.candidates.len(), 2);
    let root = tempfile::tempdir().unwrap();
    let mut knowledge = KnowledgeStore::open(SecureDir::open(root.path()).unwrap()).unwrap();
    let (workspace, journal) = controller
        .with_team_stores(|canonical, _, _| {
            Ok((
                canonical.owner().workspace_id.clone(),
                canonical.identity().unwrap().journal_id().to_string(),
            ))
        })
        .unwrap();
    knowledge.bind_workspace(&workspace).unwrap();
    let knowledge = Arc::new(std::sync::Mutex::new(knowledge));
    controller
        .attach_workspace_knowledge(knowledge.clone())
        .unwrap();
    // Actual Ways preparation starts every isolated candidate before any settles.
    for (index, candidate) in admission.candidates.iter().enumerate() {
        start_way(
            &f,
            &controller,
            &repository,
            candidate,
            &graph.nodes[index],
            &inputs[index],
        );
    }
    let mut proposals = Vec::new();
    for (index, candidate) in admission.candidates.iter().enumerate() {
        let note = KnowledgeDraft {
            id: format!("way-finding-{index}"),
            title: format!("Candidate {index} finding"),
            body: "Specific to this isolated candidate".into(),
            kind: KnowledgeKind::Finding,
            links: vec![],
            sources: vec![],
            provenance: KnowledgeProvenance::Model {
                journal_id: journal.clone(),
                activation: candidate.activation.clone(),
            },
        };
        proposals.push(
            knowledge
                .lock()
                .unwrap()
                .propose(note, 0, &candidate.activation, &journal)
                .unwrap(),
        );
        let resources = controller
            .with_team_stores(|_, content, _| {
                let ActivationEvidenceContent::Definition {
                    profile,
                    configuration,
                    ..
                } = &content
                    .resolve_activation_evidence(&graph.nodes[index].definition.snapshot)
                    .unwrap()
                else {
                    panic!("definition")
                };
                Ok(AutonomousActivationResources {
                    config: serde_json::from_str(configuration).unwrap(),
                    profile: profile.clone(),
                    provider: Arc::new(FiniteWayProvider),
                    counter: Arc::new(WayCounter),
                    tools: Arc::new(axocoatl_tools::ToolExecutor::new()),
                })
            })
            .unwrap();
        let settled = controller
            .prepare_repository_activation(
                candidate.activation.clone(),
                resources,
                controller
                    .repository_activation_resource(&repository)
                    .unwrap(),
            )
            .unwrap()
            .run()
            .await
            .unwrap();
        assert!(settled.accepted, "{:?}", settled.failure);
        assert_eq!(
            controller
                .settle_native_way_task(&candidate.activation, None)
                .unwrap(),
            index == 1
        );
    }
    assert_eq!(
        controller.snapshot().unwrap().contract().state(),
        Some(LogicalTurnState::Completed)
    );
    assert!(
        knowledge.lock().unwrap().list().unwrap().is_empty(),
        "successful unkept candidates must never enter accepted workspace memory"
    );
    controller
        .attach_workspace_knowledge(knowledge.clone())
        .unwrap();
    assert!(
        knowledge.lock().unwrap().list().unwrap().is_empty(),
        "recovery must preserve the same Keep requirement"
    );
    controller
        .with_team_stores(|canonical, content, _| {
            for proposal in &proposals {
                assert!(
                    crate::bootstrap::session_knowledge::knowledge_publication_snapshot(
                        canonical, content, proposal
                    )
                    .unwrap()
                    .is_none()
                );
            }
            let snapshot = canonical.snapshot(&admission.source_turn_id).unwrap();
            let receipt = content
                .retain_activation_evidence(ActivationEvidenceContent::Guidance {
                    text: "Exact host-retained Keep continuation".into(),
                })
                .unwrap();
            content
                .retain_ways_selection(
                    &snapshot,
                    axocoatl_session::ways_decision::WaysSelectedSessionTurn {
                        session_id: f.request.session_id.clone(),
                        turn_id: admission.source_turn_id.clone(),
                        transcript_receipt_ref: receipt.reference().clone(),
                    },
                    admission.candidates[1].activation.clone(),
                )
                .unwrap();
            assert!(
                crate::bootstrap::session_knowledge::knowledge_publication_snapshot(
                    canonical,
                    content,
                    &proposals[0]
                )
                .unwrap()
                .is_none()
            );
            assert!(
                crate::bootstrap::session_knowledge::knowledge_publication_snapshot(
                    canonical,
                    content,
                    &proposals[1]
                )
                .unwrap()
                .is_some()
            );
            Ok(())
        })
        .unwrap();
    controller
        .attach_workspace_knowledge(knowledge.clone())
        .unwrap();
    controller
        .attach_workspace_knowledge(knowledge.clone())
        .unwrap();
    let knowledge = knowledge.lock().unwrap();
    assert_eq!(knowledge.list().unwrap().len(), 1);
    assert_eq!(knowledge.list().unwrap()[0].id, "way-finding-1");
    assert_eq!(
        knowledge.proposal(&proposals[0].id).unwrap().status,
        ProposalStatus::Pending
    );
    assert_eq!(
        knowledge.proposal(&proposals[1].id).unwrap().status,
        ProposalStatus::Published
    );
}
