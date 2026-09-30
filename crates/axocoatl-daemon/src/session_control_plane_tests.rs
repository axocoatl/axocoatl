use super::*;
use axocoatl_session::turn_ledger::{
    BeginSessionTurn, RecordTurnExecution, SessionTurnAgentOutput, SessionTurnExecutionEvent,
    SessionTurnLifecycle, SessionTurnStore,
};

fn turn() -> SessionTurn {
    let root = tempfile::tempdir().unwrap();
    let mut store = SessionTurnStore::open(root.path()).unwrap();
    store
        .begin(BeginSessionTurn {
            turn_id: Some("turn-a".into()),
            session_id: "session-a".into(),
            user_input: "Check the client repository".into(),
            agent_id: None,
            model: None,
            context: vec![],
            idempotency_key: None,
            metadata: Map::new(),
        })
        .unwrap()
}

fn event(kind: &str, id: &str, at: u64, metadata: Value) -> SessionTurnExecutionEvent {
    SessionTurnExecutionEvent {
        operation_id: id.into(),
        recorded_at: at,
        event: RecordTurnExecution {
            kind: kind.into(),
            execution_id: None,
            attempt_id: None,
            metadata: metadata.as_object().unwrap().clone(),
        },
    }
}

/// History that 1.1.0 development builds recorded for a coordinated
/// multi-Agent turn still renders: one node per Agent, one activation per
/// recorded generation, and the superseded output marked as such. The removed
/// coordination records no longer add states, signals or dependencies.
#[test]
fn legacy_coordinated_history_still_renders_each_agent_output() {
    let mut turn = turn();
    turn.status = SessionTurnLifecycle::Completed;
    turn.execution_events = vec![
        event(
            "coordination_planned",
            "plan",
            1,
            json!({"agents":[{"id":"coder","depends_on":[]},{"id":"reviewer","depends_on":["coder"]}]}),
        ),
        event(
            "coordination_agent_activated",
            "coder-start",
            2,
            json!({"agent_id":"coder","generation":1}),
        ),
        event(
            "coordination_signal",
            "signal",
            3,
            json!({"signal_id":"signal-a","from_agent":"reviewer","to_agent":"coder","summary":"Fix the test","applied":true}),
        ),
    ];
    for (id, agent, generation, superseded, output) in [
        ("old-output", "coder", 1, true, "First implementation"),
        ("review-output", "reviewer", 1, false, "Looks right"),
        ("new-output", "coder", 2, false, "Second implementation"),
    ] {
        turn.agent_outputs.push(SessionTurnAgentOutput {
            operation_id: Some(id.into()),
            agent_id: agent.into(),
            model: None,
            output: output.into(),
            attempt_id: None,
            activation_generation: Some(generation),
            superseded,
            recorded_at: 20,
        });
    }
    let before = serde_json::to_value(&turn).unwrap();
    let view = SessionTurnControlPlane::from_legacy(&turn);
    assert_eq!(before, serde_json::to_value(&turn).unwrap());
    assert_eq!(view.history_version, "legacy_v1");
    let nodes = view
        .nodes
        .iter()
        .map(|node| (node.node_id.as_str(), node.activations.len()))
        .collect::<Vec<_>>();
    assert_eq!(nodes, vec![("coder", 2), ("reviewer", 1)]);
    assert_eq!(view.nodes[0].activations[0].state, "superseded");
    assert_eq!(
        view.nodes[0].activations[1].output,
        EvidenceValue::available("Second implementation".to_string())
    );
    assert!(view.edges.is_empty());
    assert!(view.nodes.iter().all(|node| node.dependencies.is_empty()));
    assert!(view
        .nodes
        .iter()
        .flat_map(|node| &node.activations)
        .flat_map(|activation| &activation.evidence)
        .all(|evidence| evidence.kind == "agent_output"));
    assert!(!view.nodes[0].activations[1].capabilities.stop.enabled);
    let wire = serde_json::to_value(&view).unwrap();
    assert_eq!(
        wire["nodes"][0]["activations"][1]["reference"]["kind"],
        "legacy"
    );
}

#[test]
fn direct_legacy_history_does_not_invent_execution_start_or_usage() {
    let mut turn = turn();
    turn.agent_id = Some("coder".into());
    turn.final_output = Some(String::new());
    let view = SessionTurnControlPlane::from_legacy(&turn);
    let activation = &view.nodes[0].activations[0];
    assert!(matches!(activation.started_at, EvidenceValue::NotRecorded));
    assert!(matches!(activation.usage, EvidenceValue::Unknown { .. }));
    assert_eq!(activation.output, EvidenceValue::available(String::new()));
    assert!(matches!(activation.input, EvidenceValue::NotRecorded));
}

#[test]
fn bounded_utf8_evidence_declares_truncation_instead_of_silent_loss() {
    let mut turn = turn();
    turn.user_input = "🦎".repeat(TEXT_PREVIEW_BYTES / 4 + 1);
    let view = SessionTurnControlPlane::from_legacy(&turn);
    let EvidenceValue::Truncated {
        value,
        original_byte_len,
    } = view.request
    else {
        panic!("expected a declared preview")
    };
    assert_eq!(value.len(), TEXT_PREVIEW_BYTES);
    assert_eq!(original_byte_len, turn.user_input.len() as u64);
}

#[cfg(unix)]
mod execution {
    use super::*;
    use axocoatl_session::control_authority::ExecutionProfile;
    use axocoatl_session::execution_content::{
        ActivationOutputContent, ActivationOutputLimits, ExecutionRequestContent, ExecutionUsage,
        OutputKind,
    };
    use axocoatl_session::execution_ownership::{LegacyFormatOwnership, UpgradedFormatOwnership};
    use axocoatl_session::execution_store::{ExecutionStoreOwner, SessionExecutionStore};
    use axocoatl_session::turn_contract::*;
    use std::sync::Arc;

    struct Fixture {
        _root: tempfile::TempDir,
        _guard: Arc<UpgradedFormatOwnership>,
        canonical: SessionExecutionStore,
        content: ExecutionContentStore,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let guard = Arc::new(
                LegacyFormatOwnership::acquire(root.path())
                    .unwrap()
                    .upgrade()
                    .unwrap(),
            );
            let canonical = SessionExecutionStore::open(
                guard.clone(),
                ExecutionStoreOwner {
                    workspace_id: "workspace-a".into(),
                    session_id: SessionId::new("session-a").unwrap(),
                },
            )
            .unwrap();
            let content = ExecutionContentStore::open_owned(
                canonical
                    .component_namespace(
                        axocoatl_session::execution_namespace::ExecutionComponent::ExecutionContent,
                    )
                    .unwrap(),
            )
            .unwrap();
            Self {
                _root: root,
                _guard: guard,
                canonical,
                content,
            }
        }

        fn append(&mut self, event: TurnContractEvent) {
            let turn_id = LogicalTurnId::new("turn-a").unwrap();
            let revision = self
                .canonical
                .snapshot(&turn_id)
                .unwrap()
                .contract()
                .revision();
            self.canonical
                .append(TurnContractEnvelope {
                    schema_version: TURN_CONTRACT_SCHEMA_VERSION,
                    command_id: CommandId::new(format!("command-{revision}")).unwrap(),
                    expected_revision: revision,
                    session_id: SessionId::new("session-a").unwrap(),
                    turn_id,
                    event,
                })
                .unwrap();
        }

        fn begin(&mut self) {
            self.begin_started(2);
        }

        fn begin_started(&mut self, started: usize) {
            self.begin_started_with(started, vec![]);
        }

        /// Two required nodes whose graph carries `conditions`; the first
        /// `started` of them start.
        fn begin_started_with(&mut self, started: usize, conditions: Vec<CompletionCondition>) {
            let definition = self.content.retain_activation_evidence(ActivationEvidenceContent::Definition {
                definition_id: AgentDefinitionId::new("shared-coder").unwrap(), revision:8,
                profile:ExecutionProfile { definition:"shared-coder".into(), provider:"ollama".into(), model:"local-model".into(), isolation:"podman".into(), tools:vec!["file_read".into()], write_scope: None },
                configuration:json!({"name":"Recorded coder", "role":"autonomous", "system_prompt":"Review exact inputs", "api_key":"DO-NOT-EXPOSE", "provider":{"token":"DO-NOT-EXPOSE-EITHER"}}).to_string(),
            }).unwrap();
            let nodes = ["a", "b"]
                .iter()
                .map(|id| GraphNode {
                    node_id: TurnNodeId::new(format!("node-{id}")).unwrap(),
                    slot_id: SessionTeamSlotId::new(format!("slot-{id}")).unwrap(),
                    definition: DefinitionSnapshotRef {
                        definition_id: AgentDefinitionId::new("shared-coder").unwrap(),
                        snapshot: definition.reference().clone(),
                    },
                    conversation_id: NodeConversationId::new(format!("conversation-{id}")).unwrap(),
                    starting_savepoint: ConversationSavepoint::Empty,
                    required: true,
                })
                .collect();
            let request = self
                .content
                .retain_request(ExecutionRequestContent {
                    turn_id: LogicalTurnId::new("turn-a").unwrap(),
                    recorded_at_unix_ms: 1,
                    display_input: "Visible request".into(),
                    effective_input: "Private prompt augmentation".into(),
                    context: vec![],
                    target_definition: None,
                    model: None,
                })
                .unwrap();
            self.canonical
                .begin_with_request(
                    TurnContractEnvelope {
                        schema_version: TURN_CONTRACT_SCHEMA_VERSION,
                        command_id: CommandId::new("begin").unwrap(),
                        expected_revision: 0,
                        session_id: SessionId::new("session-a").unwrap(),
                        turn_id: LogicalTurnId::new("turn-a").unwrap(),
                        event: TurnContractEvent::Begin {
                            epoch_id: ExecutionEpochId::new("epoch-a").unwrap(),
                            predecessor: None,
                            graph: TurnGraphSnapshot {
                                snapshot_id: GraphSnapshotId::new("graph-a").unwrap(),
                                revision: 1,
                                nodes,
                                dependencies: vec![],
                                conditions,
                            },
                        },
                    },
                    &request,
                )
                .unwrap();
            for node in self
                .canonical
                .snapshot(&LogicalTurnId::new("turn-a").unwrap())
                .unwrap()
                .contract()
                .graph()
                .unwrap()
                .nodes
                .clone()
                .into_iter()
                .take(started)
            {
                self.append(TurnContractEvent::StartActivation {
                    input: Box::new(ActivationInputManifest {
                        manifest_id: InputManifestId::new(format!(
                            "input-{}",
                            node.node_id.as_str()
                        ))
                        .unwrap(),
                        activation: ActivationRef {
                            session_id: SessionId::new("session-a").unwrap(),
                            turn_id: LogicalTurnId::new("turn-a").unwrap(),
                            execution_epoch_id: ExecutionEpochId::new("epoch-a").unwrap(),
                            node_id: node.node_id.clone(),
                            generation: 1,
                            activation_id: ActivationId::new(format!(
                                "activation-{}",
                                node.node_id.as_str()
                            ))
                            .unwrap(),
                        },
                        definition: node.definition,
                        conversation_id: node.conversation_id,
                        starting_savepoint: node.starting_savepoint,
                        parents: vec![],
                        guidance: vec![],
                        attachments: vec![],
                        repository: RepositoryInput::Unavailable,
                        budget: EvidenceRef::new("budget-a").unwrap(),
                        grant: None,
                        revision_context: None,
                    }),
                });
            }
        }

        fn snapshot(&self) -> DurableTurnSnapshot {
            self.canonical
                .snapshot(&LogicalTurnId::new("turn-a").unwrap())
                .unwrap()
        }
    }

    /// A required check whose record cannot be read is shown as unavailable;
    /// the other checks, their readiness and the rest of the turn still show.
    #[test]
    fn an_unreadable_required_check_does_not_hide_the_turn() {
        use axocoatl_session::turn_checks::{check_definitions, CheckGroup};
        let mut fixture = Fixture::new();
        let checks = vec![
            vec!["cargo".to_string(), "test".to_string()],
            vec!["npm".to_string(), "test".to_string()],
        ];
        let group = CheckGroup::required();
        let nodes = vec![
            TurnNodeId::new("node-a").unwrap(),
            TurnNodeId::new("node-b").unwrap(),
        ];
        let mut conditions: Vec<_> = check_definitions(&checks)
            .unwrap()
            .into_iter()
            .enumerate()
            .map(|(index, definition)| CompletionCondition {
                condition_id: ConditionId::new(group.condition_id(index)).unwrap(),
                kind: ConditionKind::RepositoryCheck {
                    definition: if index == 1 {
                        EvidenceRef::new("unreadable-check-definition").unwrap()
                    } else {
                        fixture
                            .content
                            .retain_repository_check_definition(definition)
                            .unwrap()
                            .reference()
                            .clone()
                    },
                },
                nodes: nodes.clone(),
            })
            .collect();
        conditions.push(CompletionCondition {
            condition_id: ConditionId::new(group.ready_id()).unwrap(),
            kind: ConditionKind::Review {
                criterion: EvidenceRef::new("readiness-criterion").unwrap(),
            },
            nodes,
        });
        fixture.begin_started_with(2, conditions);
        let view =
            SessionTurnControlPlane::from_execution(&fixture.snapshot(), &fixture.content).unwrap();
        assert_eq!(view.nodes.len(), 2);
        assert_eq!(view.required_checks.len(), 2);
        let unreadable = &view.required_checks[0];
        assert_eq!(unreadable.state, "unavailable");
        assert!(unreadable
            .reason
            .as_deref()
            .is_some_and(|reason| reason.starts_with("This check's command cannot be read")));
        assert_eq!(view.required_checks[1].argv, checks[1]);
        assert_eq!(view.required_checks[1].state, "pending");
        let readiness = view.required_check_readiness.unwrap();
        assert_eq!(readiness.state, "not_run");
        assert!(readiness
            .reason
            .contains("have not run on the current result"));
    }

    #[test]
    fn recovered_requests_keep_authority_disabled_and_preserve_exact_canonical_identity() {
        let mut fixture = Fixture::new();
        fixture.begin();
        let accepted = fixture.snapshot().contract().activations()[0]
            .activation
            .clone();
        fixture.append(TurnContractEvent::AcceptActivation {
            activation: accepted.clone(),
            output: EvidenceRef::new("accepted-output").unwrap(),
            checkpoint: Box::new(CheckpointRef {
                checkpoint_id: CheckpointId::new("accepted-checkpoint").unwrap(),
                session_id: accepted.session_id.clone(),
                conversation_id: NodeConversationId::new("conversation-a").unwrap(),
                source: CheckpointSource::Accepted {
                    activation: accepted.clone(),
                },
            }),
        });
        fixture.append(TurnContractEvent::InterruptEpoch {
            epoch_id: ExecutionEpochId::new("epoch-a").unwrap(),
        });
        let snapshot = fixture.snapshot();
        let before = fixture.canonical.records().unwrap();
        let mut view =
            SessionTurnControlPlane::from_execution(&snapshot, &fixture.content).unwrap();
        assert!(view.turn_controls.is_none());
        view.expose_recovery_requests(&snapshot).unwrap();
        let controls = view.turn_controls.as_ref().unwrap();
        assert_eq!(controls.execution_epoch_id.as_str(), "epoch-a");
        assert!(!controls.continue_turn.enabled);
        assert!(controls.continue_turn.requires_revalidation);
        assert!(!controls.finish.enabled);
        assert!(controls.finish.requires_revalidation);
        assert_eq!(controls.continuation_choices.len(), 1);
        assert_eq!(
            controls.continuation_choices[0].activation,
            snapshot.contract().activations()[1].activation
        );
        assert!(
            controls.continuation_choices[0]
                .capability
                .requires_revalidation
        );
        let caps = &view.nodes[0].activations[0].capabilities;
        assert!(caps.revise.requires_revalidation);
        assert!(
            !caps.revise.enabled
                && !caps.retry.enabled
                && !caps.guide.enabled
                && !caps.stop.enabled
        );
        assert!(!caps.retry.requires_revalidation);
        assert_eq!(
            view.turn_revision,
            EvidenceValue::available(snapshot.contract().revision())
        );
        assert_eq!(
            before,
            fixture.canonical.records().unwrap(),
            "read projection must not change the journal"
        );
        let mut rewound =
            SessionTurnControlPlane::from_execution(&snapshot, &fixture.content).unwrap();
        rewound.mark_conversation_superseded(true);
        rewound.expose_recovery_requests(&snapshot).unwrap();
        assert!(rewound.turn_controls.is_none());
        assert!(
            !rewound.nodes[0].activations[0]
                .capabilities
                .revise
                .requires_revalidation
        );
        fixture.append(TurnContractEvent::Close {
            closure: TurnClosure::Finished,
        });
        let closed = fixture.snapshot();
        let mut closed_view =
            SessionTurnControlPlane::from_execution(&closed, &fixture.content).unwrap();
        closed_view.expose_recovery_requests(&closed).unwrap();
        assert!(closed_view.turn_controls.is_none());
    }

    #[test]
    fn stopped_unrun_node_projection_retains_exact_request_without_fabricating_activation() {
        let mut fixture = Fixture::new();
        fixture.begin_started(1);
        let original = fixture.snapshot();
        assert!(
            serde_json::to_value(fixture.content.project(&original).unwrap())
                .unwrap()
                .get("stop_requested")
                .is_none()
        );
        fixture.append(TurnContractEvent::RequestTurnStop {
            evidence: original.request_ref().unwrap().clone(),
        });
        let snapshot = fixture.snapshot();
        let before = fixture.canonical.records().unwrap().to_vec();
        let view = SessionTurnControlPlane::from_execution(&snapshot, &fixture.content).unwrap();
        let intent = view.stop_requested.as_ref().unwrap();
        assert_eq!(intent, snapshot.contract().stop_requested().unwrap());
        assert_eq!(intent.unrun_nodes, vec![TurnNodeId::new("node-b").unwrap()]);
        assert!(view
            .nodes
            .iter()
            .find(|node| node.node_id == "node-b")
            .unwrap()
            .activations
            .is_empty());
        assert_eq!(
            view.nodes
                .iter()
                .find(|node| node.node_id == "node-a")
                .unwrap()
                .activations[0]
                .state,
            "running"
        );
        assert_eq!(view.nodes.len(), 2);
        assert_eq!(before, fixture.canonical.records().unwrap());
        let history = fixture.content.project(&snapshot).unwrap();
        assert_eq!(history.stop_requested.as_ref(), Some(intent));
        let mut markdown = String::new();
        axocoatl_session::session_history::append_turn_stop_markdown(&mut markdown, &history);
        assert!(markdown.contains("`node-b` · Stopped before starting"));
        assert!(!markdown.contains("`node-a` · Stopped before starting"));
    }

    #[test]
    fn exact_projection_keeps_same_template_nodes_distinct_and_excludes_private_configuration() {
        let mut fixture = Fixture::new();
        fixture.begin();
        let snapshot = fixture.snapshot();
        let records = fixture.canonical.records().unwrap().to_vec();
        let view = SessionTurnControlPlane::from_execution(&snapshot, &fixture.content).unwrap();
        assert_eq!(records, fixture.canonical.records().unwrap());
        assert_eq!(view.nodes.len(), 2);
        assert_eq!(view.nodes[0].definition_id, view.nodes[1].definition_id);
        assert_ne!(view.nodes[0].node_id, view.nodes[1].node_id);
        assert_ne!(
            view.nodes[0].activations[0].reference,
            view.nodes[1].activations[0].reference
        );
        assert_eq!(view.nodes[0].label, "Recorded coder");
        assert!(matches!(
            view.nodes[0].activations[0].reference,
            ControlPlaneActivationRef::Exact { .. }
        ));
        let wire = serde_json::to_string(&view).unwrap();
        assert!(!wire.contains("DO-NOT-EXPOSE"));
        assert!(!wire.contains("Private prompt augmentation"));
        assert!(wire.contains("Review exact inputs"));
        assert!(wire.contains("conversation-a"));
        assert!(wire.contains("conversation-b"));
        assert!(!view.nodes[0].activations[0].capabilities.retry.enabled);
    }

    #[test]
    fn exact_projection_retains_unknown_effects_and_declares_partial_truncation() {
        let mut fixture = Fixture::new();
        fixture.begin();
        let activation = fixture.snapshot().contract().activations()[0]
            .activation
            .clone();
        fixture.append(TurnContractEvent::RecordIntent {
            invocation_id: InvocationId::new("tool-a").unwrap(),
            activation: activation.clone(),
        });
        let snapshot = fixture.snapshot();
        let reservation = fixture
            .content
            .reserve_activation_output(
                &snapshot,
                &activation,
                ActivationOutputLimits {
                    partial_records: 1,
                    partial_bytes: 5,
                    settlement_bytes: 5,
                },
            )
            .unwrap();
        fixture
            .content
            .record_activation_partial(
                &reservation,
                0,
                ActivationOutputContent {
                    activation,
                    recorded_at_unix_ms: 2,
                    text: "é🦎 observed output".into(),
                    usage: ExecutionUsage::Unknown {
                        known_subtotal: Default::default(),
                    },
                    kind: OutputKind::Partial,
                },
            )
            .unwrap();
        let view = SessionTurnControlPlane::from_execution(&snapshot, &fixture.content).unwrap();
        let wire = serde_json::to_value(&view).unwrap();
        assert_eq!(
            wire["invocations"]["value"][0]["disposition"],
            "outcome_unknown"
        );
        let partial = &view.nodes[0].activations[0].partial_outputs[0];
        assert!(partial.truncated);
        assert_eq!(partial.text, "é");
        assert!(matches!(
            view.nodes[0].activations[0].output,
            EvidenceValue::NotRecorded
        ));
        assert!(matches!(
            view.nodes[0].activations[0].usage,
            EvidenceValue::Unknown { .. }
        ));
    }

    #[test]
    fn failed_activation_reason_resolves_only_its_exact_retained_output() {
        for (capacity, foreign) in [(128, false), (8, false), (128, true)] {
            let mut fixture = Fixture::new();
            fixture.begin();
            let snapshot = fixture.snapshot();
            let failed = snapshot.contract().activations()[0].activation.clone();
            let source = snapshot.contract().activations()[usize::from(foreign)]
                .activation
                .clone();
            let reservation = fixture
                .content
                .reserve_activation_output(
                    &snapshot,
                    &source,
                    ActivationOutputLimits {
                        partial_records: 1,
                        partial_bytes: 128,
                        settlement_bytes: capacity,
                    },
                )
                .unwrap();
            let message = "Activation failed: request exceeds the configured context limit";
            let output = fixture
                .content
                .settle_activation_output(
                    &reservation,
                    ActivationOutputContent {
                        activation: source,
                        recorded_at_unix_ms: 2,
                        text: message.into(),
                        usage: ExecutionUsage::Unknown {
                            known_subtotal: Default::default(),
                        },
                        kind: OutputKind::Partial,
                    },
                )
                .unwrap();
            let reference = output.reference().clone();
            fixture.append(TurnContractEvent::FailActivation {
                activation: failed,
                evidence: reference.clone(),
            });
            let before = fixture.canonical.records().unwrap().to_vec();
            let view =
                SessionTurnControlPlane::from_execution(&fixture.snapshot(), &fixture.content)
                    .unwrap();
            let expected = if foreign {
                EvidenceValue::Missing {
                    reference: reference.as_str().into(),
                }
            } else if capacity < message.len() {
                EvidenceValue::Truncated {
                    value: message[..capacity].into(),
                    original_byte_len: message.len() as u64,
                }
            } else {
                EvidenceValue::available(message.into())
            };
            assert_eq!(view.nodes[0].activations[0].reason, expected);
            assert!(matches!(
                view.nodes[0].activations[0].output,
                EvidenceValue::NotRecorded
            ));
            assert_eq!(before, fixture.canonical.records().unwrap());
        }
    }

    #[test]
    fn pre_actor_failure_resolves_exact_retained_guidance_without_claiming_output() {
        let mut fixture = Fixture::new();
        fixture.begin();
        let activation = fixture.snapshot().contract().activations()[0]
            .activation
            .clone();
        let message =
            "Activation resource preparation failed: unsupported repository tool list_files";
        let reference = fixture
            .content
            .retain_activation_evidence(ActivationEvidenceContent::Guidance {
                text: message.into(),
            })
            .unwrap()
            .reference()
            .clone();
        fixture.append(TurnContractEvent::FailActivation {
            activation,
            evidence: reference,
        });
        let before = fixture.canonical.records().unwrap().to_vec();
        let view =
            SessionTurnControlPlane::from_execution(&fixture.snapshot(), &fixture.content).unwrap();
        let activation = &view.nodes[0].activations[0];
        assert_eq!(activation.reason, EvidenceValue::available(message.into()));
        assert!(matches!(activation.output, EvidenceValue::NotRecorded));
        assert!(activation.partial_outputs.is_empty());
        assert_eq!(before, fixture.canonical.records().unwrap());
    }

    #[test]
    fn accepted_reserved_final_output_is_not_duplicated_as_partial_evidence() {
        let mut fixture = Fixture::new();
        fixture.begin();
        let snapshot = fixture.snapshot();
        let activation = snapshot.contract().activations()[0].activation.clone();
        let reservation = fixture
            .content
            .reserve_activation_output(
                &snapshot,
                &activation,
                ActivationOutputLimits {
                    partial_records: 1,
                    partial_bytes: 128,
                    settlement_bytes: 128,
                },
            )
            .unwrap();
        let output = fixture
            .content
            .settle_activation_output(
                &reservation,
                ActivationOutputContent {
                    activation: activation.clone(),
                    recorded_at_unix_ms: 2,
                    text: "Accepted final output".into(),
                    usage: ExecutionUsage::Measured {
                        usage: Default::default(),
                    },
                    kind: OutputKind::Final,
                },
            )
            .unwrap()
            .complete_output()
            .unwrap();
        fixture.append(TurnContractEvent::AcceptActivation {
            activation: activation.clone(),
            output: output.reference().clone(),
            checkpoint: Box::new(CheckpointRef {
                checkpoint_id: CheckpointId::new("accepted-checkpoint").unwrap(),
                session_id: activation.session_id.clone(),
                conversation_id: NodeConversationId::new("conversation-a").unwrap(),
                source: CheckpointSource::Accepted { activation },
            }),
        });
        let view =
            SessionTurnControlPlane::from_execution(&fixture.snapshot(), &fixture.content).unwrap();
        assert_eq!(
            view.nodes[0].activations[0].output,
            EvidenceValue::available("Accepted final output".into())
        );
        assert!(view.nodes[0].activations[0].partial_outputs.is_empty());
        let exact_accepted = view.nodes[0].activations[0].clone();
        let mut rewound = view.clone();
        rewound.mark_conversation_superseded(true);
        assert!(rewound.superseded_conversation);
        assert_eq!(
            rewound.nodes[0].activations[0], exact_accepted,
            "Rewind must not reinterpret historical acceptance or output"
        );
        assert!(rewound
            .warnings
            .iter()
            .any(|warning| warning.contains("conversation rewind")));
    }

    impl Fixture {
        /// Admission evidence shaped like a native child grant: the grant is
        /// held by `holder` and issued under a proposal naming `node`.
        fn child_admission(
            &mut self,
            lead: &ActivationRef,
            kind: &str,
            helper: &str,
            node: &str,
            holder: &str,
        ) -> EvidenceRef {
            use axocoatl_session::control_authority::{AuthorityGrant, GrantLimits};
            let proposal = self
                .content
                .retain_activation_evidence(ActivationEvidenceContent::Guidance {
                    text: json!({
                        "kind": kind,
                        "parent": lead,
                        "request": {"logical_worker_id": helper, "task_input": "Survey"},
                        "node_id": node,
                    })
                    .to_string(),
                })
                .unwrap()
                .reference()
                .clone();
            self.content
                .retain_activation_evidence(ActivationEvidenceContent::Grant {
                    policy: AuthorityGrant {
                        id: format!("child-grant-{node}"),
                        revision: 1,
                        issuer_evidence: proposal,
                        holder: TurnNodeId::new(holder).unwrap(),
                        descendants: vec![],
                        allow_stop_descendants: false,
                        delegation: None,
                        profiles: vec![],
                        conditions: vec![],
                        limits: GrantLimits {
                            activations: 1,
                            invocations: 4,
                            tokens: 1000,
                            cost_microunits: 0,
                        },
                        expires_at_ms: u64::MAX,
                    },
                })
                .unwrap()
                .reference()
                .clone()
        }

        fn add_node(&mut self, node: &str, admission: EvidenceRef) {
            let old = self.snapshot().contract().graph().unwrap().clone();
            let mut graph = old.clone();
            graph.revision += 1;
            graph.snapshot_id = GraphSnapshotId::new(format!("graph-{node}")).unwrap();
            graph.nodes.push(GraphNode {
                node_id: TurnNodeId::new(node).unwrap(),
                slot_id: SessionTeamSlotId::new(format!("dynamic-slot-{node}")).unwrap(),
                definition: old.nodes[0].definition.clone(),
                conversation_id: NodeConversationId::new(format!("conversation-{node}")).unwrap(),
                starting_savepoint: ConversationSavepoint::Empty,
                required: false,
            });
            self.append(TurnContractEvent::ReviseGraph {
                epoch_id: ExecutionEpochId::new("epoch-a").unwrap(),
                previous_graph: old.snapshot_id,
                graph,
                mutation: GraphMutation::Add {
                    node_id: TurnNodeId::new(node).unwrap(),
                },
                admission_evidence: admission,
            });
        }
    }

    #[test]
    fn agent_admitted_nodes_name_their_lead_without_becoming_dependencies() {
        let mut fixture = Fixture::new();
        fixture.begin();
        let lead = fixture.snapshot().contract().activations()[0]
            .activation
            .clone();
        let helper = fixture.child_admission(
            &lead,
            "native_delegate_child_v1",
            "scout",
            "node-helper",
            "node-helper",
        );
        fixture.add_node("node-helper", helper.clone());
        // Children a native Coordinator admitted before it ran as a lead.
        let worker = fixture.child_admission(
            &lead,
            "native_coordinator_child_v1",
            "worker",
            "node-worker",
            "node-worker",
        );
        fixture.add_node("node-worker", worker);
        // A human graph edit is admitted by its request evidence, not a grant.
        let request = fixture
            .content
            .retain_activation_evidence(ActivationEvidenceContent::Guidance {
                text: "Add a reviewer".into(),
            })
            .unwrap()
            .reference()
            .clone();
        fixture.add_node("node-human", request);
        // A grant whose proposal names another node proves nothing about this one.
        let forged = fixture.child_admission(
            &lead,
            "native_delegate_child_v1",
            "scout",
            "node-helper",
            "node-forged",
        );
        fixture.add_node("node-forged", forged);

        let view =
            SessionTurnControlPlane::from_execution(&fixture.snapshot(), &fixture.content).unwrap();
        let delegated = view
            .edges
            .iter()
            .filter(|edge| edge.kind == "delegated_by")
            .collect::<Vec<_>>();
        assert_eq!(delegated.len(), 2, "{delegated:?}");
        assert_eq!(delegated[0].source, lead.node_id.as_str());
        assert_eq!(delegated[0].target, "node-helper");
        assert_eq!(
            delegated[0].summary,
            EvidenceValue::available("scout".into())
        );
        assert_eq!(
            delegated[0].evidence,
            EvidenceValue::available(helper.as_str().into())
        );
        assert_eq!(delegated[0].generation, EvidenceValue::available(1));
        assert_eq!(delegated[1].source, lead.node_id.as_str());
        assert_eq!(delegated[1].target, "node-worker");
        assert_eq!(
            delegated[1].summary,
            EvidenceValue::available("worker".into())
        );
        for node in &view.nodes {
            assert!(node.dependencies.is_empty(), "{}", node.node_id);
        }
        assert!(view.edges.iter().all(|edge| edge.kind != "dependency"));
    }
}

#[test]
fn legacy_tools_keep_exact_event_identity_without_inventing_a_generation() {
    let mut source = turn();
    source.execution_events = vec![
        event(
            "tool_started",
            "tool-intent",
            2,
            json!({"agent_id":"coder","name":"read_file","arguments":{"path":"src/lib.rs"}}),
        ),
        event(
            "tool_result",
            "tool-outcome",
            3,
            json!({"agent_id":"coder","name":"read_file","result":"retained source","is_error":false}),
        ),
    ];
    let view = SessionTurnControlPlane::from_legacy(&source);
    let node = &view.nodes[0];
    let historical = node
        .activations
        .iter()
        .find(|activation| matches!(activation.generation, EvidenceValue::NotRecorded))
        .unwrap();
    assert_eq!(historical.evidence.len(), 2);
    assert!(
        matches!(&historical.evidence[0].reference, EvidenceValue::Available {value} if value == "tool-intent")
    );
    assert!(serde_json::to_string(&historical.evidence[1])
        .unwrap()
        .contains("retained source"));
    assert_eq!(node.activations.len(), 1);
}
