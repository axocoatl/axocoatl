use super::*;
use crate::execution_content::{
    ActivationOutputContent, ConditionOutputCapture, ConditionProcessStatus, ExecutionUsage,
    OutputKind, RepositoryCheckDefinition,
};
use crate::execution_ownership::LegacyFormatOwnership;
use crate::execution_store::ExecutionStoreOwner;
use crate::turn_checks::{check_definitions, CheckGroup};
use crate::turn_contract::*;
use std::sync::Arc;

struct Fixture {
    _root: tempfile::TempDir,
    canonical: SessionExecutionStore,
    content: ExecutionContentStore,
    begin: TurnContractEnvelope,
    start: TurnContractEnvelope,
    definitions: Vec<RepositoryCheckDefinition>,
    repository: EvidenceRef,
}

/// A turn whose required nodes are `nodes` and whose graph carries one
/// required check, as admission injects it.
fn fixture(nodes: &[&str]) -> Fixture {
    fixture_with(
        nodes,
        &[vec![
            "sh".into(),
            "-c".into(),
            "test -f reviewed.txt".into(),
        ]],
    )
}

fn fixture_with(nodes: &[&str], checks: &[Vec<String>]) -> Fixture {
    fixture_with_definitions(nodes, check_definitions(checks).unwrap())
}

/// A turn whose graph carries exactly `definitions` as its required check
/// group, however they were made.
fn fixture_with_definitions(
    nodes: &[&str],
    definitions: Vec<RepositoryCheckDefinition>,
) -> Fixture {
    let source: serde_json::Value = serde_json::from_str(include_str!(
        "../tests/fixtures/turn_contract/check_only_recovery_preserves_accepted_generations.json"
    ))
    .unwrap();
    let mut begin: TurnContractEnvelope =
        serde_json::from_value(source["steps"][0]["envelope"].clone()).unwrap();
    let start: TurnContractEnvelope =
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
    let template = graph.nodes[0].clone();
    graph.nodes = nodes
        .iter()
        .map(|node| {
            let mut copy = template.clone();
            copy.node_id = TurnNodeId::new(*node).unwrap();
            copy.slot_id = SessionTeamSlotId::new(format!("slot-{node}")).unwrap();
            copy.conversation_id = NodeConversationId::new(format!("conversation-{node}")).unwrap();
            copy
        })
        .collect();
    let required: Vec<_> = graph
        .nodes
        .iter()
        .map(|node| node.node_id.clone())
        .collect();
    let group = CheckGroup::required();
    graph.conditions = definitions
        .iter()
        .enumerate()
        .map(|(index, definition)| CompletionCondition {
            condition_id: ConditionId::new(group.condition_id(index)).unwrap(),
            kind: ConditionKind::RepositoryCheck {
                definition: content
                    .retain_repository_check_definition(definition.clone())
                    .unwrap()
                    .reference()
                    .clone(),
            },
            nodes: required.clone(),
        })
        .collect();
    if !definitions.is_empty() {
        graph.conditions.push(CompletionCondition {
            condition_id: ConditionId::new(group.ready_id()).unwrap(),
            kind: ConditionKind::Review {
                criterion: EvidenceRef::new("required-check-readiness").unwrap(),
            },
            nodes: required,
        });
    }
    canonical.append(begin.clone()).unwrap();
    Fixture {
        _root: root,
        canonical,
        content,
        begin,
        start,
        definitions,
        repository,
    }
}

impl Fixture {
    fn gate(&self) -> ControlAuthority {
        ControlAuthority::open_owned(
            self.canonical
                .component_namespace(ExecutionComponent::ControlAuthority {
                    turn_id: self.begin.turn_id.clone(),
                })
                .unwrap(),
        )
        .unwrap()
    }
    fn snapshot(&self) -> DurableTurnSnapshot {
        self.canonical.snapshot(&self.begin.turn_id).unwrap()
    }
    fn authorize(&self, gate: &ControlAuthority) -> Result<(), AuthorityError> {
        gate.authorize_required_checks(
            &self.snapshot(),
            &self.content,
            &self.repository,
            "owned-check",
        )
    }
    fn definition_ref(&self, index: usize) -> EvidenceRef {
        self.content
            .retained_check_definition(&self.definitions[index])
            .unwrap()
            .unwrap()
    }
    /// Start and accept `node`, whose conversation is `conversation`.
    fn accept(&mut self, node: &TurnNodeId, conversation: &str) -> ActivationRef {
        let mut start = self.start.clone();
        start.command_id = CommandId::new(format!("start-{}", node.as_str())).unwrap();
        start.expected_revision = self.snapshot().contract().revision();
        let TurnContractEvent::StartActivation { input } = &mut start.event else {
            panic!()
        };
        input.manifest_id = InputManifestId::new(format!("input-{}", node.as_str())).unwrap();
        input.activation.node_id = node.clone();
        input.activation.activation_id =
            ActivationId::new(format!("activation-{}", node.as_str())).unwrap();
        input.conversation_id = NodeConversationId::new(conversation).unwrap();
        input.repository = RepositoryInput::Recorded {
            snapshot: self.repository.clone(),
        };
        let activation = input.activation.clone();
        let conversation = input.conversation_id.clone();
        self.canonical.append(start).unwrap();
        let output = self
            .content
            .retain_output(
                &self.snapshot(),
                ActivationOutputContent {
                    activation: activation.clone(),
                    recorded_at_unix_ms: 1,
                    text: format!("actual retained output of {}", node.as_str()),
                    usage: ExecutionUsage::Measured {
                        usage: TokenUsageStats::default(),
                    },
                    kind: OutputKind::Final,
                },
            )
            .unwrap();
        append(
            &mut self.canonical,
            &self.begin.turn_id,
            TurnContractEvent::AcceptActivation {
                activation: activation.clone(),
                checkpoint: Box::new(CheckpointRef {
                    checkpoint_id: CheckpointId::new(format!("checkpoint-{}", node.as_str()))
                        .unwrap(),
                    session_id: self.begin.session_id.clone(),
                    conversation_id: conversation,
                    source: CheckpointSource::Accepted {
                        activation: activation.clone(),
                    },
                }),
                output: output.reference().clone(),
            },
        );
        activation
    }
}

fn policy(id: &str, holder: &TurnNodeId, tools: &[&str]) -> AuthorityGrant {
    AuthorityGrant {
        id: id.into(),
        revision: 1,
        issuer_evidence: EvidenceRef::new("applied-team").unwrap(),
        holder: holder.clone(),
        descendants: vec![],
        allow_stop_descendants: false,
        delegation: None,
        profiles: vec![ExecutionProfile {
            definition: "shared-coder".into(),
            provider: "local".into(),
            model: "finite".into(),
            isolation: "owned-check".into(),
            tools: tools.iter().map(|tool| (*tool).into()).collect(),
            write_scope: None,
        }],
        conditions: vec![],
        limits: GrantLimits {
            activations: 4,
            invocations: 10,
            tokens: 1000,
            cost_microunits: 0,
        },
        expires_at_ms: 1000,
    }
}

fn grant_ref(content: &mut ExecutionContentStore, policy: &AuthorityGrant) -> GrantSnapshotRef {
    let retained = content
        .retain_activation_evidence(ActivationEvidenceContent::Grant {
            policy: policy.clone(),
        })
        .unwrap();
    GrantSnapshotRef {
        grant_id: GrantId::new(&policy.id).unwrap(),
        revision: policy.revision,
        evidence: retained.reference().clone(),
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

fn host_checks(gate: &ControlAuthority, grant: &str) -> Vec<ConditionPermission> {
    let state = gate.lock().unwrap();
    state.data.grants[grant_index(&state.data, grant).unwrap()]
        .host_checks
        .clone()
}

#[test]
fn required_check_authority_follows_only_canonical_replacement_and_survives_reopen() {
    let mut f = fixture(&["a"]);
    let original = TurnNodeId::new("a").unwrap();
    let policy = policy("paying-grant", &original, &["bash"]);
    let gate = f.gate();
    gate.install_grant(policy.clone(), 0).unwrap();
    f.authorize(&gate).unwrap();
    let authorized = host_checks(&gate, &policy.id);
    // The two captures share one definition, so one permission covers both.
    assert_eq!(authorized.len(), 2);
    let grant = grant_ref(&mut f.content, &policy);
    let snapshot = f.snapshot();
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
    f.canonical
        .append(TurnContractEnvelope {
            schema_version: TURN_CONTRACT_SCHEMA_VERSION,
            command_id: CommandId::new("replace-approved").unwrap(),
            expected_revision: snapshot.contract().revision(),
            session_id: f.begin.session_id.clone(),
            turn_id: f.begin.turn_id.clone(),
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
    // Authorizing again reads the graph admitted at Begin: nothing changes.
    f.authorize(&gate).unwrap();
    assert_eq!(host_checks(&gate, &policy.id), authorized);
    let activation = f.accept(&replacement, "replacement-conversation");
    let group = CheckGroup::required();
    let mut runs = Vec::new();
    for (index, definition) in f.definitions.clone().iter().enumerate() {
        let run = ConditionRunRef {
            session_id: f.begin.session_id.clone(),
            turn_id: f.begin.turn_id.clone(),
            epoch_id: activation.execution_epoch_id.clone(),
            condition_id: ConditionId::new(group.condition_id(index)).unwrap(),
            run_id: ConditionRunId::new(format!("check-{index}")).unwrap(),
            activations: vec![activation.clone()],
        };
        let arguments = f
            .content
            .reserve_condition_arguments(&f.snapshot(), &run, &f.repository)
            .unwrap();
        assert_eq!(arguments.definition(), definition);
        append(
            &mut f.canonical,
            &f.begin.turn_id,
            TurnContractEvent::RecordConditionIntent {
                run: run.clone(),
                intent: arguments.reference().clone(),
            },
        );
        let claim = gate
            .claim_condition_run(
                &f.canonical,
                &f.content,
                &arguments,
                &grant,
                "owned-check",
                100,
            )
            .unwrap();
        gate.validate_condition_claim(&f.canonical, &claim, 101)
            .unwrap();
        let data = gate.lock().unwrap().data.clone();
        let record = &data.condition_calls[index];
        let stored = &data.grants[0];
        let mut foreign = record.clone();
        foreign.run.activations[0].node_id = TurnNodeId::new("unmapped-added-node").unwrap();
        assert!(!condition_allowed(&foreign, stored, &stored.policy));
        assert!(replaced_condition_permission(&f.snapshot(), &foreign, stored).is_none());
        let result = f
            .content
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
            &mut f.canonical,
            &f.begin.turn_id,
            TurnContractEvent::ResolveConditionIntent {
                run_id: run.run_id.clone(),
                resolution: ConditionEffectResolution::OutcomeRecorded {
                    evidence: result.reference().clone(),
                },
            },
        );
        append(
            &mut f.canonical,
            &f.begin.turn_id,
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
        &mut f.canonical,
        &f.begin.turn_id,
        TurnContractEvent::RecordCondition {
            epoch_id: activation.execution_epoch_id.clone(),
            condition_id: ConditionId::new(group.ready_id()).unwrap(),
            activations: vec![activation],
            outcome: ConditionOutcome::Passed,
            evidence: EvidenceRef::new("retained-candidate-check-readiness").unwrap(),
        },
    );
    append(
        &mut f.canonical,
        &f.begin.turn_id,
        TurnContractEvent::Close {
            closure: TurnClosure::Completed,
        },
    );
    assert_eq!(
        f.snapshot().contract().state(),
        Some(LogicalTurnState::Completed)
    );
    assert_eq!(gate.usage(&policy.id).unwrap().invocations, 3);
    {
        let permissions = host_checks(&gate, &policy.id);
        assert!(permissions.starts_with(&authorized));
        assert!(permissions
            .iter()
            .any(|permission| permission.nodes.as_slice() == std::slice::from_ref(&original)));
        assert!(permissions
            .iter()
            .any(|permission| permission.nodes.as_slice() == std::slice::from_ref(&replacement)));
        validate_data(&gate.lock().unwrap().data).unwrap();
    }
    // The derived permissions extend the Begin set; authorizing is still a
    // no-op once the turn has closed.
    f.authorize(&gate).unwrap();
    drop(gate);
    let gate = f.gate();
    assert_eq!(gate.usage(&policy.id).unwrap().invocations, 3);
    assert!(gate.grant_pays_required_checks(&policy.id).unwrap());
    for run in runs {
        assert!(gate
            .condition_call(&run.run_id)
            .unwrap()
            .unwrap()
            .result
            .is_some());
    }
}

#[test]
fn required_check_authority_is_idempotent_and_bound_to_the_turn_repository() {
    let mut f = fixture(&["a"]);
    let node = TurnNodeId::new("a").unwrap();
    let policy = policy("paying-grant", &node, &["bash", "read_file"]);
    let gate = f.gate();
    gate.install_grant(policy.clone(), 0).unwrap();
    assert!(!gate.grant_pays_required_checks(&policy.id).unwrap());
    f.authorize(&gate).unwrap();
    let revision = gate.revision().unwrap();
    f.authorize(&gate).unwrap();
    assert_eq!(
        gate.revision().unwrap(),
        revision,
        "a repeat writes nothing"
    );
    let stored = host_checks(&gate, &policy.id);
    assert_eq!(stored.len(), 2);
    assert!(stored
        .iter()
        .all(|permission| permission.repository == f.repository
            && permission.isolation == "owned-check"
            && permission.nodes == vec![node.clone()]
            && permission.max_timeout_ms == 180_000));
    // Another repository or isolation is a different set: refused.
    let other = f
        .content
        .retain_activation_evidence(ActivationEvidenceContent::Repository {
            description: "another checkout".into(),
            revision: None,
        })
        .unwrap()
        .reference()
        .clone();
    assert!(matches!(
        gate.authorize_required_checks(&f.snapshot(), &f.content, &other, "owned-check"),
        Err(AuthorityError::Denied)
    ));
    assert!(matches!(
        gate.authorize_required_checks(&f.snapshot(), &f.content, &f.repository, "other"),
        Err(AuthorityError::Denied)
    ));
    assert_eq!(host_checks(&gate, &policy.id), stored);
    // Another turn's journal cannot authorize this one.
    let foreign = fixture(&["a"]);
    assert!(matches!(
        gate.authorize_required_checks(
            &foreign.snapshot(),
            &f.content,
            &f.repository,
            "owned-check"
        ),
        Err(AuthorityError::Denied)
    ));
    let check = f.definition_ref(1);
    assert_eq!(
        gate.required_check_grant(&check, &f.repository, 100)
            .unwrap(),
        Some(policy.id.clone())
    );
    assert_eq!(
        gate.required_check_grant(&check, &other, 100).unwrap(),
        None
    );
    assert_eq!(
        gate.required_check_grant(&check, &f.repository, policy.expires_at_ms)
            .unwrap(),
        None
    );
    // A claim against another repository is outside the permission.
    let activation = f.accept(&node, "conversation-a");
    let run = ConditionRunRef {
        session_id: f.begin.session_id.clone(),
        turn_id: f.begin.turn_id.clone(),
        epoch_id: activation.execution_epoch_id.clone(),
        condition_id: ConditionId::new(CheckGroup::required().condition_id(1)).unwrap(),
        run_id: ConditionRunId::new("foreign-repository").unwrap(),
        activations: vec![activation],
    };
    let arguments = f
        .content
        .reserve_condition_arguments(&f.snapshot(), &run, &other)
        .unwrap();
    append(
        &mut f.canonical,
        &f.begin.turn_id,
        TurnContractEvent::RecordConditionIntent {
            run,
            intent: arguments.reference().clone(),
        },
    );
    let grant = grant_ref(&mut f.content, &policy);
    assert!(matches!(
        gate.claim_condition_run(
            &f.canonical,
            &f.content,
            &arguments,
            &grant,
            "owned-check",
            100
        ),
        Err(AuthorityError::Denied)
    ));
    // A turn without required checks authorizes nothing.
    let plain = fixture_with(&["a"], &[]);
    let gate = plain.gate();
    gate.install_grant(policy.clone(), 0).unwrap();
    let revision = gate.revision().unwrap();
    plain.authorize(&gate).unwrap();
    assert_eq!(gate.revision().unwrap(), revision);
    assert!(!gate.grant_pays_required_checks(&policy.id).unwrap());
}

#[test]
fn required_checks_are_never_paid_by_delegated_or_shell_less_grants() {
    let f = fixture(&["a", "b"]);
    let (a, b) = (TurnNodeId::new("a").unwrap(), TurnNodeId::new("b").unwrap());
    // Only a shell-less grant: nothing may pay, so nothing is authorized.
    let gate = f.gate();
    gate.install_grant(policy("grant-a", &a, &["read_file"]), 0)
        .unwrap();
    assert!(matches!(f.authorize(&gate), Err(AuthorityError::Denied)));
    assert!(host_checks(&gate, "grant-a").is_empty());
    // The first required node that may use bash pays, in graph order.
    gate.install_grant(policy("grant-b", &b, &["bash"]), gate.revision().unwrap())
        .unwrap();
    f.authorize(&gate).unwrap();
    assert!(!gate.grant_pays_required_checks("grant-a").unwrap());
    assert!(gate.grant_pays_required_checks("grant-b").unwrap());
    let check = f.definition_ref(1);
    assert_eq!(
        gate.required_check_grant(&check, &f.repository, 100)
            .unwrap()
            .as_deref(),
        Some("grant-b")
    );
    // A delegated grant never pays and its stored permission is refused.
    let mut state = gate.lock().unwrap();
    let index = grant_index(&state.data, "grant-b").unwrap();
    let mut data = state.data.clone();
    data.grants[index].delegated_from = Some(DelegatedGrantReservation {
        parent_grant_id: "grant-a".into(),
        parent_grant_revision: 1,
        parent_activation: ActivationRef {
            session_id: f.begin.session_id.clone(),
            turn_id: f.begin.turn_id.clone(),
            execution_epoch_id: ExecutionEpochId::new("epoch-1").unwrap(),
            node_id: a.clone(),
            generation: 1,
            activation_id: ActivationId::new("activation-a").unwrap(),
        },
        command_id: CommandId::new("add-child").unwrap(),
        template: DefinitionSnapshotRef {
            definition_id: AgentDefinitionId::new("shared-coder").unwrap(),
            snapshot: EvidenceRef::new("definition-v1").unwrap(),
        },
        admission_evidence: EvidenceRef::new("child-admission").unwrap(),
        limits: data.grants[index].policy.limits.clone(),
    });
    let graph = f.snapshot().contract().graph().unwrap().clone();
    assert_eq!(paying_grant(&data, &graph), None);
    assert!(validate_host_checks(&data, &data.grants[index]).is_err());
    let delegated = data.grants[index].clone();
    state.data = data;
    drop(state);
    assert_eq!(
        gate.required_check_grant(&check, &f.repository, 100)
            .unwrap(),
        None
    );
    assert!(
        replaced_condition_permission(&f.snapshot(), &placeholder_call(&f), &delegated).is_none()
    );
    assert!(!condition_allowed(
        &placeholder_call(&f),
        &delegated,
        &delegated.policy
    ));
}

/// A claim record for the group's command, as the host would make it.
fn placeholder_call(f: &Fixture) -> ConditionCallRecord {
    let definition = &f.definitions[1];
    let activation = ActivationRef {
        session_id: f.begin.session_id.clone(),
        turn_id: f.begin.turn_id.clone(),
        execution_epoch_id: ExecutionEpochId::new("epoch-1").unwrap(),
        node_id: TurnNodeId::new("b").unwrap(),
        generation: 1,
        activation_id: ActivationId::new("activation-b").unwrap(),
    };
    let protected = crate::invocation_audit::ProtectedArguments {
        evidence_ref: EvidenceRef::new("intent").unwrap(),
        sha256: "0".repeat(64),
        byte_len: 1,
    };
    ConditionCallRecord {
        run: ConditionRunRef {
            session_id: f.begin.session_id.clone(),
            turn_id: f.begin.turn_id.clone(),
            epoch_id: activation.execution_epoch_id.clone(),
            condition_id: ConditionId::new(CheckGroup::required().condition_id(1)).unwrap(),
            run_id: ConditionRunId::new("run").unwrap(),
            activations: vec![activation],
        },
        intent: EvidenceRef::new("intent").unwrap(),
        definition: f.definition_ref(1),
        repository: f.repository.clone(),
        isolation: "owned-check".into(),
        timeout_ms: definition.timeout_ms,
        stdout_bytes: definition.stdout_bytes,
        stderr_bytes: definition.stderr_bytes,
        arguments: protected,
        grant: GrantSnapshotRef {
            grant_id: GrantId::new("grant-b").unwrap(),
            revision: 1,
            evidence: EvidenceRef::new("grant").unwrap(),
        },
        claimed_at_ms: 100,
        dispatch_scope: "scope".into(),
        result: None,
    }
}

#[test]
fn authority_without_host_checks_keeps_its_exact_bytes() {
    let f = fixture(&["a"]);
    let node = TurnNodeId::new("a").unwrap();
    let gate = f.gate();
    gate.install_grant(policy("plain", &node, &["bash"]), 0)
        .unwrap();
    // What commit writes: the whole authority data.
    let bytes = || serde_json::to_vec(&gate.lock().unwrap().data).unwrap();
    let stored = bytes();
    assert!(!String::from_utf8_lossy(&stored).contains("host_checks"));
    let reloaded: AuthorityData = serde_json::from_slice(&stored).unwrap();
    validate_data(&reloaded).unwrap();
    assert_eq!(serde_json::to_vec(&reloaded).unwrap(), stored);
    // A grant that pays for required checks records them and round-trips.
    f.authorize(&gate).unwrap();
    let paying = bytes();
    assert!(String::from_utf8_lossy(&paying).contains("\"host_checks\":["));
    let reloaded: AuthorityData = serde_json::from_slice(&paying).unwrap();
    validate_data(&reloaded).unwrap();
    assert_eq!(serde_json::to_vec(&reloaded).unwrap(), paying);
    drop(gate);
    let reopened = f.gate();
    assert!(reopened.grant_pays_required_checks("plain").unwrap());
}

/// A lead's grant carries its helpers' profiles too. A helper that may use
/// bash does not make a lead without bash pay: the host holds the check
/// allowance back only from an activation that runs commands itself, so the
/// next required Agent whose own profile has bash pays.
#[test]
fn a_lead_pays_for_checks_only_when_its_own_profile_has_bash() {
    let f = fixture(&["a", "b"]);
    let (a, b) = (TurnNodeId::new("a").unwrap(), TurnNodeId::new("b").unwrap());
    let mut lead = policy("grant-a", &a, &["read_file"]);
    let mut helper = lead.profiles[0].clone();
    helper.definition = "builder-helper".into();
    helper.tools = vec!["bash".into()];
    lead.profiles.push(helper);
    let gate = f.gate();
    gate.install_grant(lead.clone(), 0).unwrap();
    // Only the lead: its helper's bash cannot pay.
    assert!(matches!(f.authorize(&gate), Err(AuthorityError::Denied)));
    assert_eq!(gate.required_check_payer().unwrap(), None);
    gate.install_grant(policy("grant-b", &b, &["bash"]), gate.revision().unwrap())
        .unwrap();
    f.authorize(&gate).unwrap();
    assert!(!gate.grant_pays_required_checks("grant-a").unwrap());
    assert!(gate.grant_pays_required_checks("grant-b").unwrap());
    let payer = gate.required_check_payer().unwrap().unwrap();
    assert_eq!(payer.grant_id, "grant-b");
    assert_eq!(payer.holder, b);
    assert_eq!(payer.invocations_left, 10);
    assert!(!payer.revoked && !payer.delegating && !payer.closed);
    // The same lead whose own profile has bash pays itself.
    let g = fixture(&["a", "b"]);
    let gate = g.gate();
    let mut own = lead.clone();
    own.profiles[0].tools = vec!["bash".into()];
    gate.install_grant(own, 0).unwrap();
    gate.install_grant(policy("grant-b", &b, &["bash"]), gate.revision().unwrap())
        .unwrap();
    g.authorize(&gate).unwrap();
    assert!(gate.grant_pays_required_checks("grant-a").unwrap());
    assert!(!gate.grant_pays_required_checks("grant-b").unwrap());
}

/// What Team Apply admits with `check_options`: a check with a ten-minute
/// timeout gets a definition with that timeout and a permission whose bound
/// covers it, and a claim of that check is allowed under it. A definition
/// over thirty minutes is never authorized, and a graph admitted by 1.2 (all
/// checks three minutes) still is.
#[test]
fn check_timeouts_reach_the_permission_up_to_thirty_minutes() {
    use crate::check_options::{RequiredCheckOptions, MAX_CHECK_TIMEOUT_MS};
    use crate::turn_checks::check_definitions_with_options;
    let checks = vec![vec!["npx".to_string(), "e2e".to_string()]];
    let long = RequiredCheckOptions {
        timeout_ms: Some(600_000),
        ..RequiredCheckOptions::default()
    };
    let definitions = check_definitions_with_options(&checks, &[long]).unwrap();
    assert_eq!(definitions[1].timeout_ms, 600_000);
    // The captures around it keep three minutes.
    assert_eq!(definitions[0].timeout_ms, 180_000);
    assert_eq!(definitions[2].timeout_ms, 180_000);
    let mut f = fixture_with_definitions(&["a"], definitions.clone());
    let node = TurnNodeId::new("a").unwrap();
    let policy = policy("paying-grant", &node, &["bash"]);
    let gate = f.gate();
    gate.install_grant(policy.clone(), 0).unwrap();
    f.authorize(&gate).unwrap();
    let permissions = host_checks(&gate, &policy.id);
    assert!(permissions
        .iter()
        .any(|permission| permission.max_timeout_ms == 600_000));
    validate_data(&gate.lock().unwrap().data).unwrap();
    // The check itself is claimed under that permission.
    let activation = f.accept(&node, "conversation-a");
    let run = ConditionRunRef {
        session_id: f.begin.session_id.clone(),
        turn_id: f.begin.turn_id.clone(),
        epoch_id: activation.execution_epoch_id.clone(),
        condition_id: ConditionId::new(CheckGroup::required().condition_id(1)).unwrap(),
        run_id: ConditionRunId::new("long-check").unwrap(),
        activations: vec![activation],
    };
    let arguments = f
        .content
        .reserve_condition_arguments(&f.snapshot(), &run, &f.repository)
        .unwrap();
    assert_eq!(arguments.definition().timeout_ms, 600_000);
    append(
        &mut f.canonical,
        &f.begin.turn_id,
        TurnContractEvent::RecordConditionIntent {
            run: run.clone(),
            intent: arguments.reference().clone(),
        },
    );
    let grant = grant_ref(&mut f.content, &policy);
    let claim = gate
        .claim_condition_run(
            &f.canonical,
            &f.content,
            &arguments,
            &grant,
            "owned-check",
            100,
        )
        .unwrap();
    gate.validate_condition_claim(&f.canonical, &claim, 101)
        .unwrap();
    assert_eq!(
        gate.condition_call(&run.run_id)
            .unwrap()
            .unwrap()
            .timeout_ms,
        600_000
    );
    // The bound is thirty minutes: a longer definition is never authorized.
    let mut over = definitions.clone();
    over[1].timeout_ms = MAX_CHECK_TIMEOUT_MS + 1;
    let f = fixture_with_definitions(&["a"], over);
    let gate = f.gate();
    gate.install_grant(policy.clone(), 0).unwrap();
    assert!(f.authorize(&gate).is_err());
    assert!(!gate.grant_pays_required_checks(&policy.id).unwrap());
    // Exactly thirty minutes is.
    let mut most = definitions;
    most[1].timeout_ms = MAX_CHECK_TIMEOUT_MS;
    let f = fixture_with_definitions(&["a"], most);
    let gate = f.gate();
    gate.install_grant(policy.clone(), 0).unwrap();
    f.authorize(&gate).unwrap();
    // A 1.2 graph: every check three minutes.
    let f = fixture(&["a"]);
    let gate = f.gate();
    gate.install_grant(policy.clone(), 0).unwrap();
    f.authorize(&gate).unwrap();
    assert!(host_checks(&gate, &policy.id)
        .iter()
        .all(|permission| permission.max_timeout_ms == 180_000));
}
