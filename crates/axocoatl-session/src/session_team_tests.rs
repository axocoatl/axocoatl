use super::*;
use crate::control_authority::{ExecutionProfile, GrantLimits};
use crate::execution_ownership::LegacyFormatOwnership;
use crate::turn_contract::{
    AgentDefinitionId, ExecutionEpochId, SessionId, TurnContractEnvelope, TurnContractEvent,
    TURN_CONTRACT_SCHEMA_VERSION,
};
use std::sync::Arc;

struct Fixture {
    _root: tempfile::TempDir,
    canonical: SessionExecutionStore,
    content: ExecutionContentStore,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let ownership = Arc::new(
            LegacyFormatOwnership::acquire(root.path())
                .unwrap()
                .upgrade()
                .unwrap(),
        );
        let canonical = SessionExecutionStore::open(
            ownership,
            ExecutionStoreOwner {
                workspace_id: "team-workspace".into(),
                session_id: SessionId::new("team-session").unwrap(),
            },
        )
        .unwrap();
        let content = ExecutionContentStore::open_owned(
            canonical
                .component_namespace(ExecutionComponent::ExecutionContent)
                .unwrap(),
        )
        .unwrap();
        Self {
            _root: root,
            canonical,
            content,
        }
    }
    fn store(&self) -> SessionTeamStore {
        SessionTeamStore::open_owned(
            self.canonical
                .component_namespace(ExecutionComponent::SessionTeam)
                .unwrap(),
            &self.canonical,
            &self.content,
            None,
        )
        .unwrap()
    }
    fn definition(
        &mut self,
        id: &str,
        revision: u64,
        instructions: &str,
        model: &str,
    ) -> DefinitionSnapshotRef {
        let definition_id = AgentDefinitionId::new(id).unwrap();
        let profile = ExecutionProfile {
            definition: id.into(),
            provider: "ollama".into(),
            model: model.into(),
            isolation: "in-process".into(),
            tools: vec![],
            write_scope: None,
        };
        // The storage seam consumes retained definition evidence rather than
        // inventing a second AgentConfig schema for team editing.
        let body = serde_json::from_value(serde_json::json!({"kind":"definition","definition_id":id,"revision":revision,
            "profile":profile,"configuration":serde_json::json!({"id":id,"system_prompt":instructions,"role":"Autonomous","model":model}).to_string()})).unwrap();
        let reference = self
            .content
            .retain_activation_evidence(body)
            .unwrap()
            .reference()
            .clone();
        DefinitionSnapshotRef {
            definition_id,
            snapshot: reference,
        }
    }
    fn request(&mut self, id: &str) -> SessionTeamCommit {
        let definition = self.definition("coder", 1, "Check the exact candidate", "model-a");
        let budget = self
            .content
            .retain_activation_evidence(ActivationEvidenceContent::Budget {
                limits: GrantLimits {
                    activations: 2,
                    invocations: 4,
                    tokens: 1000,
                    cost_microunits: 0,
                },
            })
            .unwrap()
            .reference()
            .clone();
        let slots = ["a", "b"]
            .into_iter()
            .map(|name| SessionTeamSlot {
                slot_id: SessionTeamSlotId::new(format!("slot-{name}")).unwrap(),
                node_id: TurnNodeId::new(format!("node-{name}")).unwrap(),
                definition: definition.clone(),
                conversation_id: NodeConversationId::new(format!("conversation-{name}")).unwrap(),
                required: true,
                budget: budget.clone(),
                grant: None,
            })
            .collect::<Vec<_>>();
        SessionTeamCommit {
            schema_version: 1,
            command_id: CommandId::new(id).unwrap(),
            expected_configuration_revision: 0,
            continuity: slots
                .iter()
                .map(|slot| SlotContinuityDecision {
                    slot_id: slot.slot_id.clone(),
                    decision: SessionTeamContinuity::Reset,
                })
                .collect(),
            graph: SessionTeamGraph {
                slots,
                dependencies: vec![DependencyEdge {
                    parent: TurnNodeId::new("node-a").unwrap(),
                    child: TurnNodeId::new("node-b").unwrap(),
                }],
                conditions: vec![],
            },
            initial_source: None,
            layout: vec![],
        }
    }
    fn begin(&mut self, graph: TurnGraphSnapshot) {
        self.canonical
            .append(TurnContractEnvelope {
                schema_version: TURN_CONTRACT_SCHEMA_VERSION,
                command_id: CommandId::new("begin-team-turn").unwrap(),
                expected_revision: 0,
                session_id: SessionId::new("team-session").unwrap(),
                turn_id: LogicalTurnId::new("team-turn").unwrap(),
                event: TurnContractEvent::Begin {
                    epoch_id: ExecutionEpochId::new("team-epoch").unwrap(),
                    graph,
                    predecessor: None,
                },
            })
            .unwrap();
    }
}
fn empty_points(graph: &SessionTeamGraph) -> Vec<(SessionTeamSlotId, ConversationSavepoint)> {
    graph
        .slots
        .iter()
        .map(|slot| (slot.slot_id.clone(), ConversationSavepoint::Empty))
        .collect()
}
fn next(request: &SessionTeamCommit, id: &str) -> SessionTeamCommit {
    let mut value = request.clone();
    value.command_id = CommandId::new(id).unwrap();
    value.expected_configuration_revision += 1;
    value.initial_source = None;
    for decision in &mut value.continuity {
        decision.decision = SessionTeamContinuity::PreserveUnchanged;
    }
    value
}

#[test]
fn future_team_apply_preserves_active_graph_and_exact_prior_revisions_after_reopen() {
    let mut fixture = Fixture::new();
    let request = fixture.request("team-first");
    let mut store = fixture.store();
    let first = store
        .commit(request.clone(), &fixture.canonical, &fixture.content, None)
        .unwrap();
    let graph = first
        .initial_graph(
            fixture.canonical.identity().unwrap().owner(),
            GraphSnapshotId::new("turn-graph-one").unwrap(),
            &empty_points(&first.graph),
        )
        .unwrap();
    fixture.begin(graph.clone());
    let before = fixture.canonical.records().unwrap().to_vec();
    let mut edited = next(&request, "team-replacement");
    edited.graph.slots[1].definition =
        fixture.definition("reviewer", 1, "Review changed instructions", "model-b");
    edited.graph.slots[1].conversation_id = NodeConversationId::new("conversation-new-b").unwrap();
    edited.continuity[1].decision = SessionTeamContinuity::Reset;
    let second = store
        .commit(edited, &fixture.canonical, &fixture.content, None)
        .unwrap();
    assert_eq!(second.configuration_revision, 2);
    assert_eq!(
        second.graph.slots[0].conversation_id,
        first.graph.slots[0].conversation_id
    );
    assert_ne!(
        second.graph.slots[1].conversation_id,
        first.graph.slots[1].conversation_id
    );
    assert_eq!(fixture.canonical.records().unwrap(), before);
    assert_eq!(
        fixture
            .canonical
            .snapshot(&LogicalTurnId::new("team-turn").unwrap())
            .unwrap()
            .contract()
            .graph(),
        Some(&graph)
    );
    assert_eq!(store.get(1).unwrap(), Some(&first));
    drop(store);
    let reopened = fixture.store();
    assert_eq!(reopened.get(1).unwrap(), Some(&first));
    assert_eq!(reopened.current().unwrap(), Some(&second));
    assert_eq!(fixture.canonical.records().unwrap(), before);
    // Revision one's original reset remains valid after an actual later Begin
    // uses those conversations; validation is anchored to its Apply prefix.
    assert_eq!(first.canonical_record_count, 0);
    assert_eq!(second.canonical_record_count, 1);
}

#[test]
fn exact_configuration_retry_precedes_stale_checks_and_conflicts_never_mutate() {
    let mut fixture = Fixture::new();
    let first_request = fixture.request("first");
    let mut store = fixture.store();
    let first = store
        .commit(
            first_request.clone(),
            &fixture.canonical,
            &fixture.content,
            None,
        )
        .unwrap();
    let mut layout = next(&first_request, "layout");
    layout.layout.push(SessionTeamPosition {
        slot_id: layout.graph.slots[0].slot_id.clone(),
        x: -50.0,
        y: 70.0,
    });
    let second = store
        .commit(layout, &fixture.canonical, &fixture.content, None)
        .unwrap();
    assert_eq!(
        first.graph, second.graph,
        "presentation positions do not change semantic topology"
    );
    let before = store
        .namespace
        .read_limited(FILE, MAX_RETAINED_CONTRACT_BYTES)
        .unwrap();
    assert_eq!(
        store
            .commit(
                first_request.clone(),
                &fixture.canonical,
                &fixture.content,
                None
            )
            .unwrap(),
        first
    );
    let mut conflict = first_request.clone();
    conflict.graph.slots[0].required = false;
    assert!(matches!(
        store.commit(conflict, &fixture.canonical, &fixture.content, None),
        Err(SessionTeamError::CommandConflict)
    ));
    let mut stale = first_request;
    stale.command_id = CommandId::new("stale").unwrap();
    assert!(matches!(
        store.commit(stale, &fixture.canonical, &fixture.content, None),
        Err(SessionTeamError::RevisionConflict {
            expected: 0,
            actual: 2
        })
    ));
    assert_eq!(
        store
            .namespace
            .read_limited(FILE, MAX_RETAINED_CONTRACT_BYTES)
            .unwrap(),
        before
    );
}

#[test]
fn whole_graph_and_evidence_validation_refuse_invalid_drafts_atomically() {
    let mut fixture = Fixture::new();
    let first = fixture.request("first");
    let mut store = fixture.store();
    let before = store
        .namespace
        .read_limited(FILE, MAX_RETAINED_CONTRACT_BYTES)
        .unwrap();
    let mut proposals = vec![];
    let mut bad = first.clone();
    bad.graph.dependencies.push(DependencyEdge {
        parent: TurnNodeId::new("node-b").unwrap(),
        child: TurnNodeId::new("node-a").unwrap(),
    });
    proposals.push(bad);
    let mut bad = first.clone();
    bad.graph.dependencies[0].parent = TurnNodeId::new("missing").unwrap();
    proposals.push(bad);
    let mut bad = first.clone();
    bad.graph.slots[1].slot_id = bad.graph.slots[0].slot_id.clone();
    proposals.push(bad);
    let mut bad = first.clone();
    bad.graph.slots[1].conversation_id = bad.graph.slots[0].conversation_id.clone();
    proposals.push(bad);
    let mut bad = first.clone();
    bad.graph.slots[0].definition.definition_id = AgentDefinitionId::new("another").unwrap();
    proposals.push(bad);
    let mut bad = first.clone();
    bad.graph.slots[0].budget = bad.graph.slots[0].definition.snapshot.clone();
    proposals.push(bad);
    let mut bad = first.clone();
    bad.continuity.pop();
    proposals.push(bad);
    let mut bad = first.clone();
    bad.layout.push(SessionTeamPosition {
        slot_id: SessionTeamSlotId::new("missing").unwrap(),
        x: 1.0,
        y: 2.0,
    });
    proposals.push(bad);
    let mut bad = first.clone();
    bad.layout.push(SessionTeamPosition {
        slot_id: bad.graph.slots[0].slot_id.clone(),
        x: f64::NAN,
        y: 2.0,
    });
    proposals.push(bad);
    for proposal in proposals {
        assert!(store
            .commit(proposal, &fixture.canonical, &fixture.content, None)
            .is_err());
        assert_eq!(store.configuration_revision().unwrap(), 0);
        assert_eq!(
            store
                .namespace
                .read_limited(FILE, MAX_RETAINED_CONTRACT_BYTES)
                .unwrap(),
            before
        );
    }
    // Two slots may reuse the exact template, but never its conversation.
    store
        .commit(first, &fixture.canonical, &fixture.content, None)
        .unwrap();
}

#[test]
fn changed_instructions_or_route_cannot_preserve_without_actual_projection_proof() {
    let mut fixture = Fixture::new();
    let first = fixture.request("first");
    let mut store = fixture.store();
    store
        .commit(first.clone(), &fixture.canonical, &fixture.content, None)
        .unwrap();
    let mut changed = next(&first, "changed");
    changed.graph.slots[0].definition =
        fixture.definition("coder", 2, "Different instructions", "different-model");
    assert!(store
        .commit(changed.clone(), &fixture.canonical, &fixture.content, None)
        .is_err());
    changed.continuity[0].decision = SessionTeamContinuity::PreserveWithProjection {
        evidence: EvidenceRef::new("caller-assertion-is-not-proof").unwrap(),
    };
    assert!(matches!(
        store.commit(changed.clone(), &fixture.canonical, &fixture.content, None),
        Err(SessionTeamError::UnverifiedPreservation(_))
    ));
    changed.continuity[0].decision = SessionTeamContinuity::Reset;
    assert!(
        store
            .commit(changed.clone(), &fixture.canonical, &fixture.content, None)
            .is_err(),
        "reset cannot reuse a prior conversation"
    );
    changed.graph.slots[0].conversation_id = NodeConversationId::new("new-conversation-a").unwrap();
    store
        .commit(changed, &fixture.canonical, &fixture.content, None)
        .unwrap();
}

#[test]
fn initial_team_preservation_requires_exact_owned_graph_source_and_reset_respects_all_canonical_conversations(
) {
    let mut fixture = Fixture::new();
    let request = fixture.request("first");
    let graph = SessionTeamRevision::from_commit(1, 0, request.clone())
        .initial_graph(
            fixture.canonical.identity().unwrap().owner(),
            GraphSnapshotId::new("actual-configured-graph").unwrap(),
            &empty_points(&request.graph),
        )
        .unwrap();
    fixture.begin(graph.clone());
    let mut store = fixture.store();
    assert!(
        store
            .commit(request.clone(), &fixture.canonical, &fixture.content, None)
            .is_err(),
        "Reset cannot steal a conversation used before this team store existed"
    );
    let mut imported = request;
    for choice in &mut imported.continuity {
        choice.decision = SessionTeamContinuity::PreserveUnchanged;
    }
    assert!(store
        .commit(imported.clone(), &fixture.canonical, &fixture.content, None)
        .is_err());
    imported.initial_source = Some(SessionTeamSource {
        turn_id: LogicalTurnId::new("team-turn").unwrap(),
        snapshot_id: GraphSnapshotId::new("wrong-graph").unwrap(),
        graph_revision: 1,
    });
    assert!(store
        .commit(imported.clone(), &fixture.canonical, &fixture.content, None)
        .is_err());
    imported.initial_source.as_mut().unwrap().snapshot_id = graph.snapshot_id;
    let first = store
        .commit(imported, &fixture.canonical, &fixture.content, None)
        .unwrap();
    assert_eq!(first.graph.slots.len(), 2);
    assert_eq!(first.canonical_record_count, 1);
    drop(store);
    assert_eq!(fixture.store().current().unwrap(), Some(&first));
}

#[test]
fn removal_does_not_free_a_conversation_for_reuse_and_readdition_requires_fresh_identity() {
    let mut fixture = Fixture::new();
    let first = fixture.request("first");
    let mut store = fixture.store();
    store
        .commit(first.clone(), &fixture.canonical, &fixture.content, None)
        .unwrap();
    let mut removed = next(&first, "remove-b");
    removed.graph.slots.pop();
    removed.graph.dependencies.clear();
    removed.continuity.pop();
    store
        .commit(removed.clone(), &fixture.canonical, &fixture.content, None)
        .unwrap();
    let mut added = next(&removed, "readd-b");
    added.graph.slots.push(first.graph.slots[1].clone());
    added.continuity.push(SlotContinuityDecision {
        slot_id: first.graph.slots[1].slot_id.clone(),
        decision: SessionTeamContinuity::PreserveUnchanged,
    });
    assert!(store
        .commit(added.clone(), &fixture.canonical, &fixture.content, None)
        .is_err());
    added.continuity[1].decision = SessionTeamContinuity::Reset;
    assert!(store
        .commit(added.clone(), &fixture.canonical, &fixture.content, None)
        .is_err());
    added.graph.slots[1].conversation_id =
        NodeConversationId::new("readded-conversation-b").unwrap();
    store
        .commit(added, &fixture.canonical, &fixture.content, None)
        .unwrap();
}

#[test]
fn namespace_identity_missing_primary_and_uncertain_writes_fail_closed() {
    let mut fixture = Fixture::new();
    let request = fixture.request("first");
    let mut store = fixture.store();
    let foreign = Fixture::new();
    assert!(store
        .commit(request.clone(), &foreign.canonical, &fixture.content, None)
        .is_err());
    assert!(store
        .commit(request.clone(), &fixture.canonical, &foreign.content, None)
        .is_err());
    assert!(
        fixture
            .canonical
            .component_namespace(ExecutionComponent::SessionTeam)
            .is_err(),
        "component writer is exclusive"
    );
    let error = store.commit_with(
        request.clone(),
        &fixture.canonical,
        &fixture.content,
        None,
        |namespace, bytes| {
            namespace.atomic_write(FILE, bytes)?;
            Err(io::Error::other("lost acknowledgement after durable write"))
        },
    );
    assert!(error.is_err());
    assert!(matches!(
        store.current(),
        Err(SessionTeamError::RecoveryRequired)
    ));
    drop(store);
    let mut reopened = fixture.store();
    assert_eq!(
        reopened
            .commit(request, &fixture.canonical, &fixture.content, None)
            .unwrap()
            .configuration_revision,
        1
    );
    assert_eq!(reopened.configuration_revision().unwrap(), 1);
    let primary = reopened.namespace.secure_dir().unwrap().path().join(FILE);
    drop(reopened);
    std::fs::remove_file(primary).unwrap();
    assert!(
        SessionTeamStore::open_owned(
            fixture
                .canonical
                .component_namespace(ExecutionComponent::SessionTeam)
                .unwrap(),
            &fixture.canonical,
            &fixture.content,
            None
        )
        .is_err(),
        "initialized marker must not become a fresh empty team"
    );
}

#[test]
fn oversized_drafts_and_unsupported_or_foreign_journals_never_publish_a_revision() {
    let mut fixture = Fixture::new();
    let request = fixture.request("first");
    let mut store = fixture.store();
    let mut oversized = request.clone();
    oversized.graph.slots = vec![request.graph.slots[0].clone(); 10_000];
    assert!(matches!(
        store.commit(oversized, &fixture.canonical, &fixture.content, None),
        Err(SessionTeamError::Capacity)
    ));
    assert_eq!(store.configuration_revision().unwrap(), 0);
    let primary = store.namespace.secure_dir().unwrap().path().join(FILE);
    let original = store
        .namespace
        .read_limited(FILE, MAX_RETAINED_CONTRACT_BYTES)
        .unwrap();
    drop(store);
    for mutation in ["schema", "owner"] {
        let mut value: serde_json::Value = serde_json::from_slice(&original).unwrap();
        if mutation == "schema" {
            value["schema_version"] = 99.into();
        } else {
            value["canonical_journal_id"] = "another-incarnation".into();
        }
        std::fs::write(&primary, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(SessionTeamStore::open_owned(
            fixture
                .canonical
                .component_namespace(ExecutionComponent::SessionTeam)
                .unwrap(),
            &fixture.canonical,
            &fixture.content,
            None
        )
        .is_err());
    }
}

#[test]
fn initial_graph_requires_explicit_savepoint_for_every_exact_slot() {
    let mut fixture = Fixture::new();
    let request = fixture.request("first");
    let mut store = fixture.store();
    let record = store
        .commit(request, &fixture.canonical, &fixture.content, None)
        .unwrap();
    let owner = fixture.canonical.identity().unwrap().owner().clone();
    let id = GraphSnapshotId::new("selected-config-for-next-turn").unwrap();
    assert!(record.initial_graph(&owner, id.clone(), &[]).is_err());
    let mut points = empty_points(&record.graph);
    points[1].0 = points[0].0.clone();
    assert!(record.initial_graph(&owner, id.clone(), &points).is_err());
    let graph = record
        .initial_graph(&owner, id, &empty_points(&record.graph))
        .unwrap();
    assert_eq!(graph.revision, 1);
    assert_eq!(graph.nodes[0].definition, record.graph.slots[0].definition);
    assert_eq!(graph.nodes[0].slot_id, record.graph.slots[0].slot_id);
    assert!(graph
        .nodes
        .iter()
        .all(|node| node.starting_savepoint == ConversationSavepoint::Empty));
}

#[test]
fn copied_identity_on_standalone_content_cannot_authorize_session_configuration() {
    let mut fixture = Fixture::new();
    let request = fixture.request("first");
    let external = tempfile::tempdir().unwrap();
    let mut copied =
        ExecutionContentStore::open(external.path(), fixture.canonical.identity().unwrap())
            .unwrap();
    for reference in [
        &request.graph.slots[0].definition.snapshot,
        &request.graph.slots[0].budget,
    ] {
        let actual = fixture
            .content
            .resolve_activation_evidence(reference)
            .unwrap()
            .clone();
        let retained = copied.retain_activation_evidence(actual.clone()).unwrap();
        assert_eq!(retained.reference(), reference);
        assert_eq!(
            copied.resolve_activation_evidence(reference).unwrap(),
            &actual
        );
    }
    let mut store = fixture.store();
    let original = store
        .namespace
        .read_limited(FILE, MAX_RETAINED_CONTRACT_BYTES)
        .unwrap();
    assert!(store
        .commit(request.clone(), &fixture.canonical, &copied, None)
        .is_err());
    assert_eq!(store.configuration_revision().unwrap(), 0);
    assert_eq!(
        store
            .namespace
            .read_limited(FILE, MAX_RETAINED_CONTRACT_BYTES)
            .unwrap(),
        original
    );
    drop(store);
    assert!(SessionTeamStore::open_owned(
        fixture
            .canonical
            .component_namespace(ExecutionComponent::SessionTeam)
            .unwrap(),
        &fixture.canonical,
        &copied,
        None
    )
    .is_err());
    // Actual retained content still opens and applies the same exact request.
    fixture
        .store()
        .commit(request, &fixture.canonical, &fixture.content, None)
        .unwrap();
}

#[test]
fn preview_validates_whole_candidate_without_acknowledging_or_publishing_configuration() {
    let mut fixture = Fixture::new();
    let request = fixture.request("preview");
    let mut store = fixture.store();
    let before = store
        .namespace
        .read_limited(FILE, MAX_RETAINED_CONTRACT_BYTES)
        .unwrap();
    let preview = store
        .preview(request.clone(), &fixture.canonical, &fixture.content, None)
        .unwrap();
    assert_eq!(preview.configuration_revision, 1);
    assert_eq!(store.current().unwrap(), None);
    assert_eq!(
        store
            .namespace
            .read_limited(FILE, MAX_RETAINED_CONTRACT_BYTES)
            .unwrap(),
        before
    );
    assert_eq!(
        store
            .commit(request, &fixture.canonical, &fixture.content, None)
            .unwrap(),
        preview
    );
}

#[test]
fn retained_execution_policy_requires_exact_slot_profile_budget_holder_and_issuer() {
    use crate::control_authority::AuthorityGrant;
    let mut fixture = Fixture::new();
    let mut request = fixture.request("approve");
    let issuer = fixture
        .content
        .retain_activation_evidence(ActivationEvidenceContent::Guidance {
            text: "actual authenticated Apply fixture".into(),
        })
        .unwrap()
        .reference()
        .clone();
    let slot = &request.graph.slots[0];
    let ActivationEvidenceContent::Definition { profile, .. } = fixture
        .content
        .resolve_activation_evidence(&slot.definition.snapshot)
        .unwrap()
    else {
        panic!()
    };
    let ActivationEvidenceContent::Budget { limits } = fixture
        .content
        .resolve_activation_evidence(&slot.budget)
        .unwrap()
    else {
        panic!()
    };
    let policy = AuthorityGrant {
        id: "approved-team-slot-a".into(),
        revision: 1,
        issuer_evidence: issuer,
        holder: slot.node_id.clone(),
        descendants: vec![],
        allow_stop_descendants: false,
        delegation: None,
        profiles: vec![profile.clone()],
        conditions: vec![],
        limits: limits.clone(),
        expires_at_ms: u64::MAX,
    };
    let mut store = fixture.store();
    for mutation in ["holder", "profile", "budget", "issuer"] {
        let mut invalid = policy.clone();
        match mutation {
            "holder" => invalid.holder = TurnNodeId::new("another-node").unwrap(),
            "profile" => invalid.profiles[0].model = "another-model".into(),
            "budget" => invalid.limits.tokens += 1,
            _ => invalid.issuer_evidence = EvidenceRef::new("missing-issuer").unwrap(),
        }
        request.graph.slots[0].grant = Some(
            fixture
                .content
                .retain_activation_evidence(ActivationEvidenceContent::Grant { policy: invalid })
                .unwrap()
                .reference()
                .clone(),
        );
        assert!(store
            .preview(request.clone(), &fixture.canonical, &fixture.content, None)
            .is_err());
        assert_eq!(store.current().unwrap(), None);
    }
    request.graph.slots[0].grant = Some(
        fixture
            .content
            .retain_activation_evidence(ActivationEvidenceContent::Grant {
                policy: policy.clone(),
            })
            .unwrap()
            .reference()
            .clone(),
    );
    let record = store
        .commit(request, &fixture.canonical, &fixture.content, None)
        .unwrap();
    drop(store);
    assert_eq!(fixture.store().current().unwrap(), Some(&record));
    assert!(
        record.graph.slots[1].grant.is_none(),
        "compatibility absence supplies no policy"
    );
}
