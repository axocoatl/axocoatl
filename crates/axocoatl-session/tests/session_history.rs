#![cfg(unix)]

use std::sync::Arc;

use axocoatl_core::TokenUsageStats;
use axocoatl_session::execution_content::*;
use axocoatl_session::execution_namespace::ExecutionComponent;
use axocoatl_session::execution_ownership::{LegacyFormatOwnership, UpgradedFormatOwnership};
use axocoatl_session::execution_store::*;
use axocoatl_session::session_history::*;
use axocoatl_session::turn_contract::*;
use axocoatl_session::{
    BeginSessionTurn, SessionTurnLifecycle, SessionTurnStore, TransitionSessionTurn,
};

fn legacy_turn(legacy: &mut SessionTurnStore, id: &str) {
    legacy_turn_in_session(legacy, "session-a", id);
}

fn legacy_turn_in_session(legacy: &mut SessionTurnStore, session_id: &str, id: &str) {
    legacy
        .begin(BeginSessionTurn {
            turn_id: Some(id.into()),
            session_id: session_id.into(),
            user_input: format!("Request {id} 🦎"),
            agent_id: Some("coder".into()),
            model: None,
            context: vec![],
            idempotency_key: None,
            metadata: Default::default(),
        })
        .unwrap();
    legacy
        .transition(
            id,
            format!("close-{id}"),
            TransitionSessionTurn {
                status: SessionTurnLifecycle::Interrupted,
                final_output: None,
                error: Some(format!("Error {id}")),
                metadata: Default::default(),
            },
        )
        .unwrap();
}

fn owner() -> ExecutionStoreOwner {
    ExecutionStoreOwner {
        workspace_id: "workspace-a".into(),
        session_id: SessionId::new("session-a").unwrap(),
    }
}

struct Fixture {
    _root: tempfile::TempDir,
    guard: Arc<UpgradedFormatOwnership>,
    canonical: SessionExecutionStore,
    content: ExecutionContentStore,
    legacy: SessionTurnStore,
}

impl Fixture {
    fn new(ids: &[&str], rewind: Option<&str>) -> Self {
        let root = tempfile::tempdir().unwrap();
        let mut legacy = SessionTurnStore::open(root.path().join("session-history")).unwrap();
        for id in ids {
            legacy_turn(&mut legacy, id);
        }
        if let Some(boundary) = rewind {
            legacy
                .rewind("session-a", Some(boundary), "rewind")
                .unwrap();
        }
        let guard = Arc::new(
            LegacyFormatOwnership::acquire(root.path())
                .unwrap()
                .upgrade()
                .unwrap(),
        );
        let canonical = SessionExecutionStore::open(guard.clone(), owner()).unwrap();
        let content = ExecutionContentStore::open_owned(
            canonical
                .component_namespace(ExecutionComponent::ExecutionContent)
                .unwrap(),
        )
        .unwrap();
        Self {
            _root: root,
            guard,
            canonical,
            content,
            legacy,
        }
    }

    fn seal(&mut self) {
        let snapshot = self.canonical.legacy_history_snapshot().unwrap();
        let retained = self.content.retain_legacy_history(&snapshot).unwrap();
        self.canonical.seal_legacy_history(&retained).unwrap();
    }

    fn begin(&mut self, id: &str, timestamp: u64) {
        let mut event = initial()[0].clone();
        event.turn_id = LogicalTurnId::new(id).unwrap();
        event.command_id = CommandId::new(format!("begin-{id}")).unwrap();
        if let TurnContractEvent::Begin { predecessor, .. } = &mut event.event {
            *predecessor = self.canonical.records().unwrap().last().map(|record| {
                self.canonical
                    .turn(&record.turn_id)
                    .unwrap()
                    .unwrap()
                    .closed_reference()
                    .unwrap()
            });
        }
        let request = self
            .content
            .retain_request(ExecutionRequestContent {
                turn_id: event.turn_id.clone(),
                recorded_at_unix_ms: timestamp,
                display_input: format!("Visible request {id}"),
                effective_input: "private-augmentation-only".into(),
                context: vec![],
                target_definition: None,
                model: None,
            })
            .unwrap();
        self.canonical.begin_with_request(event, &request).unwrap();
    }
}

fn initial() -> Vec<TurnContractEnvelope> {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/turn_contract/partial_finish_is_not_success.json"
    ))
    .unwrap();
    fixture["steps"].as_array().unwrap()[..2]
        .iter()
        .map(|step| serde_json::from_value(step["envelope"].clone()).unwrap())
        .collect()
}

fn append(canonical: &mut SessionExecutionStore, id: &str, name: &str, event: TurnContractEvent) {
    let turn_id = LogicalTurnId::new(id).unwrap();
    let revision = canonical.snapshot(&turn_id).unwrap().contract().revision();
    canonical
        .append(TurnContractEnvelope {
            schema_version: TURN_CONTRACT_SCHEMA_VERSION,
            command_id: CommandId::new(name).unwrap(),
            expected_revision: revision,
            session_id: owner().session_id,
            turn_id,
            event,
        })
        .unwrap();
}

#[test]
fn legacy_compatibility_preserves_exact_search_transcript_and_hidden_rows() {
    let root = tempfile::tempdir().unwrap();
    let mut legacy = SessionTurnStore::open(root.path()).unwrap();
    legacy_turn(&mut legacy, "first");
    legacy_turn(&mut legacy, "hidden");
    legacy.rewind("session-a", Some("first"), "rewind").unwrap();
    let history = SessionHistory::from_legacy(&legacy, "session-a").unwrap();
    assert_eq!(history.session_id(), "session-a");
    assert_eq!(
        history.legacy_rows(HistoryVisibility::Visible).unwrap(),
        legacy.list("session-a")
    );
    assert_eq!(
        history
            .legacy_rows(HistoryVisibility::IncludingSuperseded)
            .unwrap(),
        legacy.list_including_superseded("session-a")
    );
    for query in ["  ReQuEsT ", "Error", "🦎", "hidden", "[", ""] {
        assert_eq!(
            history.legacy_search(query).unwrap(),
            legacy.search(Some("session-a"), query)
        );
    }
    assert_eq!(
        history.legacy_transcript().unwrap(),
        legacy.transcript("session-a")
    );
    assert!(!history.get("hidden").unwrap().is_visible());
    assert!(history.get("absent").is_none());
    assert_eq!(history.legacy_get("hidden").unwrap(), legacy.get("hidden"));
    assert_eq!(history.legacy_get("absent").unwrap(), None);
}

#[test]
fn legacy_catalog_preserves_interleaved_global_order_and_search_exactly() {
    let root = tempfile::tempdir().unwrap();
    let mut legacy = SessionTurnStore::open(root.path()).unwrap();
    for (session, id) in [
        ("session-b", "b-first"),
        ("session-a", "a-first"),
        ("session-b", "b-hidden"),
        ("session-c", "c-first"),
        ("session-a", "a-second"),
    ] {
        legacy_turn_in_session(&mut legacy, session, id);
    }
    legacy
        .rewind("session-b", Some("b-first"), "rewind-b")
        .unwrap();
    let catalog = SessionHistoryCatalog::from_legacy(&legacy).unwrap();
    assert_eq!(
        catalog
            .entries(HistoryVisibility::IncludingSuperseded)
            .iter()
            .map(|entry| entry.turn_id())
            .collect::<Vec<_>>(),
        vec!["b-first", "a-first", "b-hidden", "c-first", "a-second"]
    );
    assert_eq!(
        catalog
            .entries(HistoryVisibility::Visible)
            .iter()
            .map(|entry| entry.turn_id())
            .collect::<Vec<_>>(),
        vec!["b-first", "a-first", "c-first", "a-second"]
    );
    for query in ["request", "  ErRoR ", "first", "hidden", "🦎", ""] {
        assert_eq!(
            catalog.legacy_search(query).unwrap(),
            legacy.search(None, query)
        );
    }
    assert_eq!(
        catalog
            .search("request")
            .iter()
            .map(|hit| hit.entry.turn_id())
            .collect::<Vec<_>>(),
        vec!["b-first", "a-first", "c-first", "a-second"]
    );
    assert_eq!(
        catalog.legacy_get("b-hidden").unwrap(),
        legacy.get("b-hidden")
    );
    assert_eq!(catalog.legacy_get("absent").unwrap(), None);
}

#[test]
fn upgraded_reads_require_a_seal_and_reject_foreign_content() {
    let mut fixture = Fixture::new(&[], None);
    assert!(matches!(
        SessionHistory::from_upgraded(&fixture.canonical, &fixture.content),
        Err(SessionHistoryError::MissingLegacySeal)
    ));
    fixture.seal();
    let other = Fixture::new(&[], None);
    assert!(SessionHistory::from_upgraded(&fixture.canonical, &other.content).is_err());
    assert!(
        SessionHistory::from_upgraded(&fixture.canonical, &fixture.content)
            .unwrap()
            .entries(HistoryVisibility::Visible)
            .is_empty()
    );
}

#[test]
fn live_output_search_crosses_chunk_boundaries_without_accepting_or_searching_reasoning() {
    let mut fixture = Fixture::new(&["legacy"], None);
    fixture.seal();
    fixture.begin("turn-a", 42);
    fixture.canonical.append(initial()[1].clone()).unwrap();
    let snapshot = fixture
        .canonical
        .snapshot(&LogicalTurnId::new("turn-a").unwrap())
        .unwrap();
    let activation = snapshot.contract().activations()[0].activation.clone();
    for (sequence, payload) in [
        ActivationStreamPayload::Text {
            delta: "Observed live ".into(),
        },
        ActivationStreamPayload::ReasoningSummary {
            delta: "reasoning-only-marker".into(),
        },
        ActivationStreamPayload::Text {
            delta: "result 🦎".into(),
        },
    ]
    .into_iter()
    .enumerate()
    {
        fixture
            .content
            .record_activation_stream(
                &snapshot,
                ActivationStreamContent {
                    schema_version: 1,
                    activation: activation.clone(),
                    sequence: sequence as u64,
                    recorded_at_unix_ms: 43 + sequence as u64,
                    payload,
                },
            )
            .unwrap();
    }
    let canonical_bytes = std::fs::read(fixture.canonical.path()).unwrap();
    let history = SessionHistory::from_upgraded(&fixture.canonical, &fixture.content).unwrap();
    let hits = history.search("LIVE RESULT 🦎");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].entry.turn_id(), "turn-a");
    assert_eq!(
        hits[0].matched_fields,
        [axocoatl_session::turn_ledger::TurnSearchField::Output]
    );
    assert!(history.search("reasoning-only-marker").is_empty());
    assert!(history.search("private-augmentation-only").is_empty());
    let SessionHistoryEntry::ExecutionV2(view) = &hits[0].entry else {
        panic!("native history union");
    };
    assert!(!view.activations[0].currently_accepted);
    assert_eq!(
        view.activations[0].activation.state,
        ActivationState::Running
    );
    assert_eq!(
        std::fs::read(fixture.canonical.path()).unwrap(),
        canonical_bytes
    );
    drop(fixture.content);
    let content = ExecutionContentStore::open_owned(
        fixture
            .canonical
            .component_namespace(ExecutionComponent::ExecutionContent)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        SessionHistory::from_upgraded(&fixture.canonical, &content)
            .unwrap()
            .search("live result 🦎"),
        hits
    );
}

#[test]
fn sealed_frontier_remains_exact_when_mutable_legacy_source_later_changes() {
    let mut fixture = Fixture::new(&["first", "hidden"], Some("first"));
    fixture.seal();
    let expected = fixture.legacy.list_including_superseded("session-a");
    // Deliberately exercise an old writer outside the host ownership protocol:
    // an upgraded read must not consult this mutable source after the seal.
    legacy_turn(&mut fixture.legacy, "later-old-writer");
    let history = SessionHistory::from_upgraded(&fixture.canonical, &fixture.content).unwrap();
    assert_eq!(
        history
            .legacy_rows(HistoryVisibility::IncludingSuperseded)
            .unwrap(),
        expected
    );
    assert!(history.get("later-old-writer").is_none());
    assert_eq!(history.entries(HistoryVisibility::Visible).len(), 1);
    assert_eq!(
        history
            .entries(HistoryVisibility::IncludingSuperseded)
            .len(),
        2
    );
}

#[test]
fn hidden_legacy_identity_cannot_collide_with_a_v2_turn() {
    let mut fixture = Fixture::new(&["old", "turn-a"], Some("old"));
    fixture.seal();
    fixture.begin("turn-a", 100);
    assert!(matches!(
        SessionHistory::from_upgraded(&fixture.canonical, &fixture.content),
        Err(SessionHistoryError::TurnIdentityCollision(id)) if id == "turn-a"
    ));
}

#[test]
fn canonical_begin_order_wins_over_clock_order_and_legacy_consumers_refuse_v2() {
    let mut fixture = Fixture::new(&["old"], None);
    fixture.seal();
    fixture.begin("turn-a", 200);
    append(
        &mut fixture.canonical,
        "turn-a",
        "interrupt-a",
        TurnContractEvent::InterruptEpoch {
            epoch_id: ExecutionEpochId::new("epoch-1").unwrap(),
        },
    );
    append(
        &mut fixture.canonical,
        "turn-a",
        "cancel-a",
        TurnContractEvent::Close {
            closure: TurnClosure::Cancelled,
        },
    );
    fixture.begin("turn-b", 100);
    let history = SessionHistory::from_upgraded(&fixture.canonical, &fixture.content).unwrap();
    assert_eq!(
        history
            .entries(HistoryVisibility::Visible)
            .iter()
            .map(|entry| entry.turn_id())
            .collect::<Vec<_>>(),
        vec!["old", "turn-a", "turn-b"]
    );
    assert!(matches!(
        history.legacy_transcript(),
        Err(SessionHistoryError::RequiresVersionedConsumer)
    ));
    assert!(matches!(
        history.legacy_search(""),
        Err(SessionHistoryError::RequiresVersionedConsumer)
    ));
    assert!(matches!(
        history.legacy_rows(HistoryVisibility::Visible),
        Err(SessionHistoryError::RequiresVersionedConsumer)
    ));
    for id in ["old", "turn-a", "absent"] {
        assert!(matches!(
            history.legacy_get(id),
            Err(SessionHistoryError::RequiresVersionedConsumer)
        ));
    }
    assert_eq!(history.search("Visible request").len(), 2);
    assert!(history.search("private-augmentation-only").is_empty());
    let exported: serde_json::Value =
        serde_json::from_str(&history.export_json(HistoryVisibility::Visible).unwrap()).unwrap();
    assert_eq!(exported[0]["history_version"], "legacy_v1");
    assert_eq!(exported[1]["history_version"], "execution_v2");
    assert!(exported[1]["turn"].get("completed_at").is_none());
}

#[test]
fn restart_preserves_needs_attention_partial_evidence_and_unknown_usage() {
    let mut fixture = Fixture::new(&[], None);
    fixture.seal();
    fixture.begin("turn-a", 100);
    fixture.canonical.append(initial()[1].clone()).unwrap();
    let snapshot = fixture
        .canonical
        .snapshot(&LogicalTurnId::new("turn-a").unwrap())
        .unwrap();
    let partial = ActivationOutputContent {
        activation: snapshot.contract().activations()[0].activation.clone(),
        recorded_at_unix_ms: 101,
        text: "observed partial 🦎".into(),
        usage: ExecutionUsage::Unknown {
            known_subtotal: TokenUsageStats::default(),
        },
        kind: OutputKind::Partial,
    };
    fixture
        .content
        .retain_output(&snapshot, partial.clone())
        .unwrap();
    let Fixture {
        _root,
        guard,
        canonical,
        content,
        legacy,
    } = fixture;
    drop(content);
    drop(canonical);
    let canonical = SessionExecutionStore::open(guard, owner()).unwrap();
    let content = ExecutionContentStore::open_owned(
        canonical
            .component_namespace(ExecutionComponent::ExecutionContent)
            .unwrap(),
    )
    .unwrap();
    let history = SessionHistory::from_upgraded(&canonical, &content).unwrap();
    let SessionHistoryEntry::ExecutionV2(turn) = history.get("turn-a").unwrap() else {
        panic!("expected v2")
    };
    assert_eq!(turn.state, LogicalTurnState::NeedsAttention);
    assert_eq!(turn.activations[0].partial_outputs, vec![partial]);
    assert!(!turn.activations[0].currently_accepted);
    assert_eq!(history.search("partial 🦎").len(), 1);
    assert!(
        matches!(&history.transcript()[0], SessionHistoryTranscriptEntry::ExecutionV2(view) if view.state == LogicalTurnState::NeedsAttention)
    );
    let markdown = history.export_markdown();
    assert!(markdown.contains("NeedsAttention"));
    assert!(markdown.contains("not acceptance"));
    drop(legacy);
}

#[test]
fn missing_final_body_and_unrecorded_request_are_preserved_without_invention() {
    let mut fixture = Fixture::new(&[], None);
    fixture.seal();
    for event in initial() {
        fixture.canonical.append(event).unwrap();
    }
    let snapshot = fixture
        .canonical
        .snapshot(&LogicalTurnId::new("turn-a").unwrap())
        .unwrap();
    let activation = &snapshot.contract().activations()[0];
    append(
        &mut fixture.canonical,
        "turn-a",
        "accept-missing",
        TurnContractEvent::AcceptActivation {
            activation: activation.activation.clone(),
            checkpoint: Box::new(CheckpointRef {
                checkpoint_id: CheckpointId::new("accepted-checkpoint").unwrap(),
                session_id: owner().session_id,
                conversation_id: activation.conversation_id.clone(),
                source: CheckpointSource::Accepted {
                    activation: activation.activation.clone(),
                },
            }),
            output: EvidenceRef::new("missing-final-body").unwrap(),
        },
    );
    let history = SessionHistory::from_upgraded(&fixture.canonical, &fixture.content).unwrap();
    let SessionHistoryEntry::ExecutionV2(turn) = history.get("turn-a").unwrap() else {
        panic!("expected v2")
    };
    assert!(matches!(turn.request, ContentResolution::NotRecorded));
    assert!(matches!(
        turn.activations[0].output,
        ContentResolution::Missing { .. }
    ));
    assert!(turn.activations[0].currently_accepted);
    assert!(history.search("missing-final-body").is_empty());
    let markdown = history.export_markdown();
    assert!(markdown.contains("Request: not recorded"));
    assert!(markdown.contains("Final output: unavailable"));
}

#[test]
fn reserved_terminal_prefix_survives_restart_search_transcript_and_export_without_acceptance() {
    let mut fixture = Fixture::new(&[], None);
    fixture.seal();
    fixture.begin("turn-a", 100);
    fixture.canonical.append(initial()[1].clone()).unwrap();
    let snapshot = fixture
        .canonical
        .snapshot(&LogicalTurnId::new("turn-a").unwrap())
        .unwrap();
    let exact = snapshot.contract().activations()[0].activation.clone();
    let reservation = fixture
        .content
        .reserve_activation_output(
            &snapshot,
            &exact,
            ActivationOutputLimits {
                partial_records: 1,
                partial_bytes: 8,
                settlement_bytes: 15,
            },
        )
        .unwrap();
    let output = |text: &str| ActivationOutputContent {
        activation: exact.clone(),
        recorded_at_unix_ms: 101,
        text: text.into(),
        usage: ExecutionUsage::Unknown {
            known_subtotal: TokenUsageStats::default(),
        },
        kind: OutputKind::Partial,
    };
    fixture
        .content
        .record_activation_partial(&reservation, 0, output(""))
        .unwrap();
    let full = "reserved-marker followed by more observed output 🦎";
    fixture
        .content
        .settle_activation_output(&reservation, output(full))
        .unwrap();
    let Fixture {
        _root,
        guard,
        canonical,
        content,
        legacy,
    } = fixture;
    drop(content);
    drop(canonical);
    let canonical = SessionExecutionStore::open(guard, owner()).unwrap();
    let content = ExecutionContentStore::open_owned(
        canonical
            .component_namespace(ExecutionComponent::ExecutionContent)
            .unwrap(),
    )
    .unwrap();
    let journal_before = std::fs::read(canonical.path()).unwrap();
    let history = SessionHistory::from_upgraded(&canonical, &content).unwrap();
    let SessionHistoryEntry::ExecutionV2(turn) = history.get("turn-a").unwrap() else {
        panic!("v2 history required")
    };
    assert_eq!(turn.state, LogicalTurnState::NeedsAttention);
    assert!(matches!(
        turn.activations[0].output,
        ContentResolution::NotRecorded
    ));
    assert!(turn.activations[0].partial_outputs.is_empty());
    assert_eq!(turn.activations[0].reserved_outputs.len(), 2);
    assert!(!turn.activations[0].currently_accepted);
    assert_eq!(history.search("reserved-marker").len(), 1);
    assert!(
        matches!(&history.transcript()[0], SessionHistoryTranscriptEntry::ExecutionV2(view)
        if view.activations[0].reserved_outputs.len() == 2)
    );
    let markdown = history.export_markdown();
    assert_eq!(markdown.matches("reserved-marker").count(), 1);
    assert!(markdown.contains("Recorded empty partial output."));
    assert!(markdown.contains(&format!("retained 15 of {} bytes", full.len())));
    assert!(markdown.contains("not acceptance"));
    assert!(!markdown.contains("currently accepted: true"));
    assert_eq!(std::fs::read(canonical.path()).unwrap(), journal_before);
    drop(legacy);
}

#[test]
fn available_reserved_final_is_exported_once_without_duplicate_unaccepted_label() {
    let mut fixture = Fixture::new(&[], None);
    fixture.seal();
    fixture.begin("turn-a", 100);
    fixture.canonical.append(initial()[1].clone()).unwrap();
    let snapshot = fixture
        .canonical
        .snapshot(&LogicalTurnId::new("turn-a").unwrap())
        .unwrap();
    let activation = snapshot.contract().activations()[0].clone();
    let reservation = fixture
        .content
        .reserve_activation_output(
            &snapshot,
            &activation.activation,
            ActivationOutputLimits {
                partial_records: 0,
                partial_bytes: 0,
                settlement_bytes: 64,
            },
        )
        .unwrap();
    let retained = fixture
        .content
        .settle_activation_output(
            &reservation,
            ActivationOutputContent {
                activation: activation.activation.clone(),
                recorded_at_unix_ms: 101,
                text: "accepted-reserved-final".into(),
                usage: ExecutionUsage::Unknown {
                    known_subtotal: TokenUsageStats::default(),
                },
                kind: OutputKind::Final,
            },
        )
        .unwrap();
    append(
        &mut fixture.canonical,
        "turn-a",
        "accept-reserved",
        TurnContractEvent::AcceptActivation {
            activation: activation.activation.clone(),
            checkpoint: Box::new(CheckpointRef {
                checkpoint_id: CheckpointId::new("accepted-reserved-checkpoint").unwrap(),
                session_id: owner().session_id,
                conversation_id: activation.conversation_id.clone(),
                source: CheckpointSource::Accepted {
                    activation: activation.activation,
                },
            }),
            output: retained.reference().clone(),
        },
    );
    let history = SessionHistory::from_upgraded(&fixture.canonical, &fixture.content).unwrap();
    let markdown = history.export_markdown();
    assert_eq!(markdown.matches("accepted-reserved-final").count(), 1);
    assert!(!markdown.contains("Output evidence (not acceptance)"));
    assert_eq!(history.search("accepted-reserved-final").len(), 1);
}

#[test]
fn stopped_before_start_history_survives_restart_search_and_exports_without_activation() {
    let mut fixture = Fixture::new(&["legacy"], None);
    fixture.seal();
    fixture.begin("never-started", 42);
    let turn_id = LogicalTurnId::new("never-started").unwrap();
    let original = fixture.canonical.snapshot(&turn_id).unwrap();
    let expected_nodes = original
        .contract()
        .graph()
        .unwrap()
        .nodes
        .iter()
        .map(|node| node.node_id.clone())
        .collect::<Vec<_>>();
    assert!(!expected_nodes.is_empty());
    append(
        &mut fixture.canonical,
        "never-started",
        "human-stop-request",
        TurnContractEvent::RequestTurnStop {
            evidence: original.request_ref().unwrap().clone(),
        },
    );
    append(
        &mut fixture.canonical,
        "never-started",
        "cancelled-close",
        TurnContractEvent::Close {
            closure: TurnClosure::Cancelled,
        },
    );
    let expected = fixture
        .canonical
        .snapshot(&turn_id)
        .unwrap()
        .contract()
        .stop_requested()
        .unwrap()
        .clone();
    let assert_history = |canonical: &SessionExecutionStore, content: &ExecutionContentStore| {
        let before = std::fs::read(canonical.path()).unwrap();
        let history = SessionHistory::from_upgraded(canonical, content).unwrap();
        let Some(SessionHistoryEntry::ExecutionV2(turn)) = history.get("never-started") else {
            panic!("native history")
        };
        assert!(turn.activations.is_empty());
        assert_eq!(turn.stop_requested.as_ref(), Some(&expected));
        assert_eq!(
            turn.stop_requested.as_ref().unwrap().unrun_nodes,
            expected_nodes
        );
        assert!(history
            .export_json(HistoryVisibility::Visible)
            .unwrap()
            .contains("human-stop-request"));
        assert!(serde_json::to_string(&history.transcript())
            .unwrap()
            .contains("unrun_nodes"));
        assert!(history
            .export_markdown()
            .contains("Stopped before starting"));
        let hits = history.search(expected_nodes[0].as_str());
        assert_eq!(hits.len(), 1);
        assert_eq!(
            hits[0].matched_fields,
            vec![axocoatl_session::TurnSearchField::Context]
        );
        assert_eq!(history.search("stopped before starting").len(), 1);
        assert_eq!(std::fs::read(canonical.path()).unwrap(), before);
    };
    assert_history(&fixture.canonical, &fixture.content);
    let Fixture {
        _root,
        guard,
        canonical,
        content,
        legacy,
    } = fixture;
    drop(content);
    drop(canonical);
    drop(legacy);
    let canonical = SessionExecutionStore::open(guard, owner()).unwrap();
    let content = ExecutionContentStore::open_owned(
        canonical
            .component_namespace(ExecutionComponent::ExecutionContent)
            .unwrap(),
    )
    .unwrap();
    assert_history(&canonical, &content);
}
