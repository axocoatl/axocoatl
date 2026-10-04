use super::*;
use axocoatl_session::execution_content::ExecutionContentStore;
use axocoatl_session::execution_ownership::LegacyFormatOwnership;
use axocoatl_session::execution_store::ExecutionStoreOwner;
use axocoatl_session::turn_contract::{
    CommandId, DefinitionSnapshotRef, ExecutionEpochId, GraphNode, GraphSnapshotId,
    TurnContractEnvelope, TurnContractEvent, TURN_CONTRACT_SCHEMA_VERSION,
};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// Four records per segment, so a few turns span many sealed segments.
const SMALL: SegmentSpec = SegmentSpec {
    segment_bytes: 64 * 1024,
    segment_records: 4,
    ..LOG_SPEC
};

fn checkpoint(conversation: &NodeConversationId, version: u64) -> AgentCheckpoint {
    AgentCheckpoint {
        version,
        agent_id: conversation.as_str().into(),
        checkpoint_time: version,
        session_messages: vec![],
        cumulative_token_usage: TokenUsageStats::new(version as usize, 1),
        cumulative_token_usage_known: true,
        behavior_state: None,
    }
}

/// Every record of the store, read from disk in log order.
fn all_events(store: &ActivationStateStore) -> Vec<Event> {
    let mut events = Vec::new();
    for segment in store.log.sealed() {
        events.extend(store.log.read_sealed::<Event>(segment).unwrap());
    }
    events.extend(store.active.iter().map(|event| (**event).clone()));
    events
}

fn count(events: &[Event], kind: fn(&Event) -> bool) -> usize {
    events.iter().filter(|event| kind(event)).count()
}

/// Records in memory are at most one active segment's.
fn assert_bounded(store: &ActivationStateStore, spec: &SegmentSpec) {
    assert!(store.active.len() < spec.segment_records as usize);
    assert_eq!(store.sealed.len(), store.log.sealed().len());
    let sealed_records: u64 = store
        .log
        .sealed()
        .iter()
        .map(|segment| segment.records)
        .sum();
    assert_eq!(
        sealed_records + store.active.len() as u64,
        store.log.next_sequence() - 1
    );
}

// ---------------------------------------------------------------------------
// Synthetic turns through the store's own admission path. Each turn records
// an input, a diagnostic candidate, the accepted candidate, and a promotion
// decision and its completion: five records.

struct Synthetic {
    journal: CanonicalJournal,
    session_id: SessionId,
    slot_id: SessionTeamSlotId,
    conversation: NodeConversationId,
    template: ActivationInputManifest,
}

struct SyntheticTurn {
    input: InputRecord,
    diagnostic: CheckpointRef,
    accepted: CheckpointRef,
    manifest: PromotionManifest,
}

impl Synthetic {
    fn new() -> Self {
        let envelopes: Vec<TurnContractEnvelope> = serde_json::from_str(include_str!(
            "../tests/fixtures/activation_state/running_history.json"
        ))
        .unwrap();
        let TurnContractEvent::StartActivation { input } = &envelopes[1].event else {
            unreachable!()
        };
        Self {
            journal: CanonicalJournal {
                journal_id: "6dc6189a-63e5-4d32-8e0d-71fdd9c33f6b".into(),
                workspace_id: "workspace".into(),
            },
            session_id: input.activation.session_id.clone(),
            slot_id: SessionTeamSlotId::new("slot-a").unwrap(),
            conversation: input.conversation_id.clone(),
            template: (**input).clone(),
        }
    }

    fn open(&self, root: &Path, spec: SegmentSpec) -> ActivationStateStore {
        ActivationStateStore::open_isolated(root, self.session_id.clone(), spec).unwrap()
    }

    fn admit(&self, store: &mut ActivationStateStore, event: Event) {
        let admitted = store.admit(&event, &self.journal).unwrap();
        store.bind_journal(self.journal.clone()).unwrap();
        store.append(event, admitted).unwrap();
    }

    fn turn(
        &self,
        store: &mut ActivationStateStore,
        n: usize,
        previous: Option<CheckpointRef>,
    ) -> SyntheticTurn {
        let turn_id = LogicalTurnId::new(format!("turn-{n}")).unwrap();
        let mut input = self.template.clone();
        input.manifest_id = InputManifestId::new(format!("input-{n}")).unwrap();
        input.activation.turn_id = turn_id.clone();
        input.activation.activation_id = ActivationId::new(format!("activation-{n}")).unwrap();
        input.starting_savepoint = match &previous {
            Some(checkpoint) => ConversationSavepoint::Checkpoint {
                checkpoint: Box::new(checkpoint.clone()),
            },
            None => ConversationSavepoint::Empty,
        };
        let record = InputRecord {
            sha256: digest(&(&self.slot_id, &input)).unwrap(),
            slot_id: self.slot_id.clone(),
            input,
        };
        self.admit(store, Event::Input(record.clone()));
        let diagnostic = store
            .stage_checkpoint(
                &self.journal,
                &record,
                &checkpoint(&self.conversation, 2 * n as u64),
            )
            .unwrap();
        let accepted = store
            .stage_checkpoint(
                &self.journal,
                &record,
                &checkpoint(&self.conversation, 2 * n as u64 + 1),
            )
            .unwrap();
        let closure: ClosedTurnRef = serde_json::from_value(serde_json::json!({
            "session_id": self.session_id,
            "turn_id": turn_id,
            "closure_revision": 4,
            "closure": "completed",
        }))
        .unwrap();
        let contract_sha256 = digest(&n).unwrap();
        let mut selected = vec![PromotedConversation {
            slot_id: self.slot_id.clone(),
            node_id: record.input.activation.node_id.clone(),
            accepted: accepted.clone(),
            committed: accepted.clone(),
            previous_committed: previous,
        }];
        let promotion_id =
            promotion_id(&self.journal, &closure, &contract_sha256, &selected).unwrap();
        selected[0].committed = committed_ref(&promotion_id, &accepted).unwrap();
        let manifest = PromotionManifest {
            journal_id: self.journal.journal_id.clone(),
            workspace_id: self.journal.workspace_id.clone(),
            promotion_id,
            closure,
            contract_sha256,
            selected,
        };
        self.admit(store, Event::PromotionPrepared(manifest.clone()));
        store.finish_pending().unwrap();
        SyntheticTurn {
            input: record,
            diagnostic,
            accepted,
            manifest,
        }
    }

    fn turns(
        &self,
        store: &mut ActivationStateStore,
        range: std::ops::Range<usize>,
    ) -> Vec<SyntheticTurn> {
        let mut previous = store.projection.effective(&self.conversation);
        let mut turns = Vec::new();
        for n in range {
            let turn = self.turn(store, n, previous);
            previous = Some(turn.manifest.selected[0].committed.clone());
            turns.push(turn);
        }
        turns
    }
}

fn assert_reads(store: &ActivationStateStore, synthetic: &Synthetic, turns: &[SyntheticTurn]) {
    let last = turns.last().unwrap();
    assert_eq!(
        store.committed_reference(&synthetic.conversation).unwrap(),
        Some(last.manifest.selected[0].committed.clone())
    );
    for turn in [&turns[0], &turns[turns.len() / 2], last] {
        let n = turns
            .iter()
            .position(|item| item.manifest == turn.manifest)
            .unwrap() as u64;
        assert_eq!(store.checkpoint(&turn.diagnostic).unwrap().version, 2 * n);
        assert_eq!(store.checkpoint(&turn.accepted).unwrap().version, 2 * n + 1);
        assert_eq!(
            store
                .checkpoint(&turn.manifest.selected[0].committed)
                .unwrap()
                .version,
            2 * n + 1
        );
        assert_eq!(
            store
                .promotion_of_turn(turn.manifest.closure.turn_id())
                .unwrap(),
            Some(turn.manifest.clone())
        );
        assert_eq!(
            store.input(&turn.input.input.activation).unwrap(),
            Some(turn.input.clone())
        );
    }
    assert_eq!(
        store.committed_activation(&synthetic.conversation).unwrap(),
        Some(last.input.input.activation.clone())
    );
}

/// Record `turns` synthetic turns, then check that memory holds one active
/// segment and that every kind of old record is read back, before and after
/// reopening.
fn long_session(turns: usize, spec: SegmentSpec) {
    let root = tempfile::tempdir().unwrap();
    let synthetic = Synthetic::new();
    let mut store = synthetic.open(root.path(), spec);
    let recorded = synthetic.turns(&mut store, 0..turns);
    let events = all_events(&store);
    assert_eq!(count(&events, |e| matches!(e, Event::Input(_))), turns);
    assert_eq!(
        count(&events, |e| matches!(e, Event::Candidate(_))),
        2 * turns
    );
    assert_eq!(
        count(&events, |e| matches!(e, Event::PromotionPrepared(_))),
        turns
    );
    assert_eq!(store.projection.promotions, turns);
    assert!(store.log.sealed().len() as u64 >= 5 * turns as u64 / spec.segment_records - 1);
    assert_bounded(&store, &spec);
    assert_eq!(store.projection.conversations.len(), 1);
    // What grows with history is one filter per sealed segment: about two
    // bytes per key and salt.
    let filter_bytes: usize = store.sealed.iter().map(SealedSummary::bytes).sum();
    assert!(
        filter_bytes <= events.len() * 2 * 3 * 2,
        "{filter_bytes} bytes for {} records",
        events.len()
    );
    assert_reads(&store, &synthetic, &recorded);
    drop(store);
    for reopen_spec in [spec, LOG_SPEC] {
        let store = synthetic.open(root.path(), reopen_spec);
        assert_eq!(store.recovery, Default::default());
        assert_eq!(store.projection.promotions, turns);
        assert_bounded(&store, &spec);
        assert_reads(&store, &synthetic, &recorded);
    }
}

#[test]
fn a_long_session_keeps_one_active_segment_in_memory_and_reads_every_old_record() {
    long_session(
        300,
        SegmentSpec {
            segment_bytes: 64 * 1024,
            segment_records: 16,
            ..LOG_SPEC
        },
    );
}

/// The single-file store refused a 4097th input or promotion and an 8193rd
/// candidate for the whole life of a Session. Every record is a synced
/// append, so this takes minutes where a sync flushes the drive (macOS).
#[test]
#[ignore = "about 20 000 synced appends; run with --ignored"]
fn a_session_records_more_inputs_candidates_and_promotions_than_the_former_caps() {
    const TURNS: usize = 4100;
    const { assert!(TURNS > 4096 && 2 * TURNS > 8192) };
    long_session(
        TURNS,
        SegmentSpec {
            segment_bytes: 1024 * 1024,
            segment_records: 1024,
            ..LOG_SPEC
        },
    );
}

#[test]
fn a_torn_tail_is_removed_and_an_interrupted_seal_is_completed_through_the_store() {
    let root = tempfile::tempdir().unwrap();
    let synthetic = Synthetic::new();
    let mut store = synthetic.open(root.path(), SMALL);
    // Four turns are twenty records: five full segments, none active.
    let mut turns = synthetic.turns(&mut store, 0..4);
    assert!(store.active.is_empty());
    assert_eq!(store.log.sealed().len(), 5);
    drop(store);
    let active = root.path().join(SMALL.active_name());
    let last_sealed = root
        .path()
        .join("segments")
        .join("activation-state.0000000004.jsonl");
    // The crash came after the sealed file was published and before the
    // next active segment replaced the old one.
    let sealed = fs::read(&last_sealed).unwrap();
    let body_end = sealed[..sealed.len() - 1]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .unwrap()
        + 1;
    fs::write(&active, &sealed[..body_end]).unwrap();
    let mut store = synthetic.open(root.path(), SMALL);
    assert!(store.recovery.completed_seal);
    assert_eq!(store.log.sealed().len(), 5);
    assert!(store.active.is_empty());
    assert_reads(&store, &synthetic, &turns);
    turns.extend(synthetic.turns(&mut store, 4..5));
    drop(store);
    // An unfinished last line was never acknowledged.
    let mut file = fs::OpenOptions::new().append(true).open(&active).unwrap();
    std::io::Write::write_all(&mut file, br#"{"record":{"input":{"slot_id":"slot-a","#).unwrap();
    drop(file);
    let mut store = synthetic.open(root.path(), SMALL);
    assert!(store.recovery.torn_bytes > 0);
    assert_reads(&store, &synthetic, &turns);
    turns.extend(synthetic.turns(&mut store, 5..7));
    drop(store);
    let store = synthetic.open(root.path(), SMALL);
    assert_eq!(store.recovery, Default::default());
    assert_eq!(store.projection.promotions, 7);
    assert_bounded(&store, &SMALL);
    assert_reads(&store, &synthetic, &turns);
}

#[test]
fn an_unsealed_record_that_repeats_a_sealed_identity_is_refused_on_open() {
    let root = tempfile::tempdir().unwrap();
    let synthetic = Synthetic::new();
    let mut store = synthetic.open(root.path(), SMALL);
    synthetic.turns(&mut store, 0..3);
    assert!(!store.active.is_empty());
    drop(store);
    let first_sealed = fs::read(
        root.path()
            .join("segments")
            .join("activation-state.0000000000.jsonl"),
    )
    .unwrap();
    // The second line of the first segment is turn 0's input.
    let input_line = first_sealed
        .split_inclusive(|byte| *byte == b'\n')
        .nth(1)
        .unwrap()
        .to_vec();
    assert!(String::from_utf8_lossy(&input_line).contains("\"input\""));
    let active = root.path().join(SMALL.active_name());
    let original = fs::read(&active).unwrap();
    let mut forged = original.clone();
    forged.extend(&input_line);
    fs::write(&active, &forged).unwrap();
    assert!(matches!(
        ActivationStateStore::open_isolated(root.path(), synthetic.session_id.clone(), SMALL),
        Err(ActivationStateError::Invalid(_))
    ));
    assert_eq!(
        fs::read(&active).unwrap(),
        forged,
        "a refused open writes nothing"
    );
    fs::write(&active, &original).unwrap();
    synthetic.open(root.path(), SMALL);
}

// ---------------------------------------------------------------------------
// Real canonical turns through the public API.

struct Owned {
    _root: tempfile::TempDir,
    canonical: SessionExecutionStore,
    content: ExecutionContentStore,
}

impl Owned {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        // An empty single-file history, sealed before the first turn, as a
        // migrated Session has; rewind reads History through its seal.
        drop(
            axocoatl_session::turn_ledger::SessionTurnStore::open(
                SecureDir::open(root.path())
                    .unwrap()
                    .child("session-history")
                    .unwrap()
                    .path(),
            )
            .unwrap(),
        );
        let ownership = Arc::new(
            LegacyFormatOwnership::acquire(root.path())
                .unwrap()
                .upgrade()
                .unwrap(),
        );
        let mut canonical = SessionExecutionStore::open(
            ownership,
            ExecutionStoreOwner {
                workspace_id: "workspace".into(),
                session_id: SessionId::new("session").unwrap(),
            },
        )
        .unwrap();
        let mut content = ExecutionContentStore::open_owned(
            canonical
                .component_namespace(ExecutionComponent::ExecutionContent)
                .unwrap(),
        )
        .unwrap();
        let source = canonical.legacy_history_snapshot().unwrap();
        let retained = content.retain_legacy_history(&source).unwrap();
        canonical.seal_legacy_history(&retained).unwrap();
        Self {
            _root: root,
            canonical,
            content,
        }
    }

    fn memory(&self, spec: SegmentSpec) -> ActivationStateStore {
        ActivationStateStore::open_owned_with(
            self.canonical
                .component_namespace(ExecutionComponent::ActivationState)
                .unwrap(),
            spec,
        )
        .unwrap()
    }

    fn memory_root(&self) -> PathBuf {
        self.canonical
            .path()
            .parent()
            .unwrap()
            .join("activation-state")
    }

    fn append(&mut self, turn: &LogicalTurnId, event: TurnContractEvent) {
        let revision = self
            .canonical
            .turn(turn)
            .unwrap()
            .map_or(0, |contract| contract.revision());
        self.canonical
            .append(TurnContractEnvelope {
                schema_version: TURN_CONTRACT_SCHEMA_VERSION,
                command_id: CommandId::new(format!("{}-{revision}", turn.as_str())).unwrap(),
                expected_revision: revision,
                session_id: SessionId::new("session").unwrap(),
                turn_id: turn.clone(),
                event,
            })
            .unwrap();
    }

    /// One closed and promoted turn of one lead Agent.
    fn turn(
        &mut self,
        memory: &mut ActivationStateStore,
        n: usize,
        start: Option<CheckpointRef>,
    ) -> (ActivationRef, CheckpointRef, PromotionManifest) {
        let turn = LogicalTurnId::new(format!("turn-{n}")).unwrap();
        let conversation = NodeConversationId::new("lead").unwrap();
        let starting_savepoint = match start {
            Some(checkpoint) => ConversationSavepoint::Checkpoint {
                checkpoint: Box::new(checkpoint),
            },
            None => ConversationSavepoint::Empty,
        };
        let definition = DefinitionSnapshotRef {
            definition_id: axocoatl_session::turn_contract::AgentDefinitionId::new("coder")
                .unwrap(),
            snapshot: EvidenceRef::new("definition").unwrap(),
        };
        let node = GraphNode {
            node_id: TurnNodeId::new("lead").unwrap(),
            slot_id: SessionTeamSlotId::new("lead-slot").unwrap(),
            definition: definition.clone(),
            conversation_id: conversation.clone(),
            starting_savepoint: starting_savepoint.clone(),
            required: true,
        };
        self.append(
            &turn,
            TurnContractEvent::Begin {
                epoch_id: ExecutionEpochId::new("epoch").unwrap(),
                graph: TurnGraphSnapshot {
                    snapshot_id: GraphSnapshotId::new(format!("graph-{n}")).unwrap(),
                    revision: 1,
                    nodes: vec![node],
                    dependencies: vec![],
                    conditions: vec![],
                },
                predecessor: None,
            },
        );
        let activation = ActivationRef {
            session_id: SessionId::new("session").unwrap(),
            turn_id: turn.clone(),
            execution_epoch_id: ExecutionEpochId::new("epoch").unwrap(),
            node_id: TurnNodeId::new("lead").unwrap(),
            generation: 1,
            activation_id: ActivationId::new(format!("activation-{n}")).unwrap(),
        };
        self.append(
            &turn,
            TurnContractEvent::StartActivation {
                input: Box::new(ActivationInputManifest {
                    manifest_id: InputManifestId::new(format!("input-{n}")).unwrap(),
                    activation: activation.clone(),
                    definition,
                    conversation_id: conversation.clone(),
                    starting_savepoint,
                    parents: vec![],
                    guidance: vec![EvidenceRef::new("guidance").unwrap()],
                    attachments: vec![],
                    repository: axocoatl_session::turn_contract::RepositoryInput::Recorded {
                        snapshot: EvidenceRef::new("repository").unwrap(),
                    },
                    budget: EvidenceRef::new("budget").unwrap(),
                    grant: None,
                    revision_context: None,
                }),
            },
        );
        memory
            .record_input(&self.canonical.snapshot(&turn).unwrap(), &activation)
            .unwrap();
        let reservation = memory
            .reserve_candidate(&self.canonical, &activation)
            .unwrap();
        let accepted = memory
            .stage_reserved_candidate(&reservation, &checkpoint(&conversation, n as u64))
            .unwrap();
        self.append(
            &turn,
            TurnContractEvent::AcceptActivation {
                activation: activation.clone(),
                checkpoint: Box::new(accepted.clone()),
                output: EvidenceRef::new(format!("output-{n}")).unwrap(),
            },
        );
        memory
            .prepare_close(
                &self.canonical.snapshot(&turn).unwrap(),
                TurnClosure::Completed,
            )
            .unwrap();
        self.append(
            &turn,
            TurnContractEvent::Close {
                closure: TurnClosure::Completed,
            },
        );
        let manifest = memory
            .promote(&self.canonical.snapshot(&turn).unwrap())
            .unwrap();
        (activation, accepted, manifest)
    }
}

#[test]
fn records_rotate_into_sealed_segments_and_old_turns_stay_reachable_through_the_api() {
    let mut owned = Owned::new();
    let mut memory = owned.memory(SMALL);
    let lead = NodeConversationId::new("lead").unwrap();
    let mut turns = Vec::new();
    let mut committed = None;
    for n in 0..8 {
        let turn = owned.turn(&mut memory, n, committed.clone());
        committed = Some(turn.2.selected[0].committed.clone());
        turns.push(turn);
    }
    assert!(
        memory.log.sealed().len() >= 8,
        "{}",
        memory.log.sealed().len()
    );
    assert_bounded(&memory, &SMALL);
    let check = |memory: &ActivationStateStore,
                 turns: &[(ActivationRef, CheckpointRef, PromotionManifest)]| {
        for (n, (activation, accepted, manifest)) in turns.iter().enumerate() {
            let snapshot = owned.canonical.snapshot(&activation.turn_id).unwrap();
            assert_eq!(memory.promotion(&snapshot).unwrap(), Some(manifest.clone()));
            assert_eq!(memory.checkpoint(accepted).unwrap().version, n as u64);
            assert_eq!(
                memory
                    .checkpoint(&manifest.selected[0].committed)
                    .unwrap()
                    .version,
                n as u64
            );
            assert!(memory
                .candidate_reservation(&owned.canonical, activation)
                .is_ok());
            if n > 0 {
                assert_eq!(
                    memory
                        .starting_checkpoint(activation)
                        .unwrap()
                        .unwrap()
                        .version,
                    n as u64 - 1
                );
            }
        }
    };
    check(&memory, &turns);
    assert_eq!(
        memory.committed_activation(&lead).unwrap(),
        Some(turns[7].0.clone())
    );
    // Rewind to the first turn: its decision is read back from the oldest
    // sealed segment.
    let rewind = memory
        .rewind_session(
            &owned.canonical,
            &owned.content,
            Some("turn-0"),
            std::slice::from_ref(&lead),
            ToolReplayPolicy::CompleteNativeGroups,
        )
        .unwrap();
    let superseded: Vec<String> = (1..8).map(|n| format!("turn-{n}")).collect();
    assert_eq!(rewind.superseded_turn_ids, superseded);
    assert_eq!(
        memory.committed_reference(&lead).unwrap(),
        Some(turns[0].2.selected[0].committed.clone())
    );
    assert_eq!(memory.superseded_turn_ids().unwrap(), superseded);
    drop(memory);
    for spec in [SMALL, LOG_SPEC] {
        let memory = owned.memory(spec);
        check(&memory, &turns);
        assert_eq!(memory.superseded_turn_ids().unwrap(), superseded);
        assert_eq!(
            memory.committed_activation(&lead).unwrap(),
            Some(turns[0].0.clone())
        );
    }
    let mut memory = owned.memory(SMALL);
    let next = owned.turn(
        &mut memory,
        8,
        Some(turns[0].2.selected[0].committed.clone()),
    );
    assert_eq!(
        next.2.selected[0].previous_committed,
        Some(turns[0].2.selected[0].committed.clone())
    );
    assert_bounded(&memory, &SMALL);
}

// ---------------------------------------------------------------------------
// Migration from the single-file layout.

fn copy_private(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    fs::set_permissions(to, fs::Permissions::from_mode(0o700)).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_private(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).unwrap();
            fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        }
    }
}

fn legacy_fixture() -> (tempfile::TempDir, LegacyState) {
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/activation_state/legacy_store");
    let root = tempfile::tempdir().unwrap();
    copy_private(&fixture, root.path());
    let state = serde_json::from_slice(&fs::read(root.path().join(STATE_FILE)).unwrap()).unwrap();
    (root, state)
}

fn assert_migrated(store: &ActivationStateStore, legacy: &LegacyState) {
    let events = all_events(store);
    assert_eq!(
        count(&events, |e| matches!(e, Event::Input(_))),
        legacy.inputs.len()
    );
    for input in &legacy.inputs {
        assert_eq!(
            store.input(&input.input.activation).unwrap().as_ref(),
            Some(input)
        );
    }
    assert_eq!(
        count(&events, |e| matches!(e, Event::Candidate(_))),
        legacy.candidates.len()
    );
    for candidate in &legacy.candidates {
        assert_eq!(
            store.candidate(&candidate.reference).unwrap().as_ref(),
            Some(candidate)
        );
        store.checkpoint(&candidate.reference).unwrap();
    }
    for promotion in &legacy.promotions {
        assert_eq!(
            store
                .promotion_of_turn(promotion.closure.turn_id())
                .unwrap()
                .as_ref(),
            Some(promotion)
        );
        for selected in &promotion.selected {
            store.checkpoint(&selected.committed).unwrap();
        }
    }
    for turn in &legacy.promotion_reservations {
        assert!(store.turn_reserved(turn).unwrap());
    }
}

#[test]
fn a_single_file_store_written_by_the_previous_version_migrates_with_all_history() {
    let (root, legacy) = legacy_fixture();
    // The fixture was written by the single-file store, interrupted after
    // its last promotion decision was durable and one pointer was written.
    let pending = legacy.pending.clone().unwrap();
    assert_eq!(legacy.promotions.len(), 3);
    assert_eq!(legacy.promotion_reservations.len(), 1);
    let store = ActivationStateStore::open(root.path(), legacy.session_id.clone()).unwrap();
    let head_bytes = fs::read(root.path().join(STATE_FILE)).unwrap();
    let head: StoreHead = serde_json::from_slice(&head_bytes).unwrap();
    assert_eq!(head.journal, legacy.journal);
    assert!(head.segments.matches(&LOG_SPEC));
    // A daemon that knows only the single-file layout refuses the head.
    assert!(serde_json::from_slice::<LegacyState>(&head_bytes).is_err());
    assert_migrated(&store, &legacy);
    // Opening completed the interrupted decision.
    assert!(store.projection.pending.is_none());
    assert_eq!(store.projection.promotions, legacy.promotions.len() + 1);
    assert_eq!(
        store.promotion_of_turn(pending.closure.turn_id()).unwrap(),
        Some(pending.clone())
    );
    for selected in &pending.selected {
        let conversation = &selected.committed.conversation_id;
        assert_eq!(
            store.committed_reference(conversation).unwrap(),
            Some(selected.committed.clone())
        );
        assert_eq!(
            serde_json::from_slice::<PromotedConversation>(
                &fs::read(root.path().join("heads").join(head_name(conversation))).unwrap()
            )
            .unwrap(),
            *selected
        );
    }
    let conversation_b = NodeConversationId::new("conversation-b").unwrap();
    assert_eq!(
        store.committed_reference(&conversation_b).unwrap(),
        legacy
            .heads
            .iter()
            .find(|head| head.committed.conversation_id == conversation_b)
            .map(|head| head.committed.clone())
    );
    drop(store);
    let store = ActivationStateStore::open(root.path(), legacy.session_id.clone()).unwrap();
    assert_eq!(fs::read(root.path().join(STATE_FILE)).unwrap(), head_bytes);
    assert_migrated(&store, &legacy);
    assert_eq!(store.projection.promotions, legacy.promotions.len() + 1);
}

#[test]
fn an_interrupted_migration_is_redone_from_the_single_file() {
    let (root, legacy) = legacy_fixture();
    let original = fs::read(root.path().join(STATE_FILE)).unwrap();
    // A partial log from a conversion that crashed before the head was
    // written, including a sealed segment and a torn line.
    let mut log = SegmentLog::open(
        SecureDir::open(root.path()).unwrap(),
        SMALL,
        log_meta(&legacy.session_id, None),
        true,
        |_, _: IgnoredAny| Ok::<(), SegmentError>(()),
    )
    .unwrap();
    for event in legacy_events(&legacy).unwrap().iter().take(6) {
        let line = log.encode_record(&**event).unwrap();
        log.append_line(&line).unwrap();
        if log.should_seal() {
            log.seal().unwrap();
        }
    }
    drop(log);
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(root.path().join(SMALL.active_name()))
        .unwrap();
    std::io::Write::write_all(&mut file, b"{\"record\":").unwrap();
    drop(file);
    assert_eq!(fs::read(root.path().join(STATE_FILE)).unwrap(), original);
    let store = ActivationStateStore::open(root.path(), legacy.session_id.clone()).unwrap();
    assert_migrated(&store, &legacy);
    assert_eq!(store.projection.promotions, legacy.promotions.len() + 1);
}

#[test]
fn a_corrupt_single_file_store_is_refused_without_writing() {
    for corruption in ["heads", "candidate_owner", "rewind_order"] {
        let (root, legacy) = legacy_fixture();
        let mut state: serde_json::Value =
            serde_json::from_slice(&fs::read(root.path().join(STATE_FILE)).unwrap()).unwrap();
        match corruption {
            "heads" => state["heads"] = serde_json::json!([]),
            "candidate_owner" => {
                state["candidates"][0]["reference"]["conversation_id"] = "another".into()
            }
            _ => {
                state["rewinds"] = serde_json::json!([{
                    "rewind_id": "rewind:x", "keep_through_turn_id": null,
                    "superseded_turn_ids": [], "after_promotions": 9, "conversations": []
                }])
            }
        }
        let bytes = serde_json::to_vec(&state).unwrap();
        fs::write(root.path().join(STATE_FILE), &bytes).unwrap();
        assert!(
            ActivationStateStore::open(root.path(), legacy.session_id.clone()).is_err(),
            "{corruption}"
        );
        assert_eq!(fs::read(root.path().join(STATE_FILE)).unwrap(), bytes);
        assert!(!root.path().join(LOG_SPEC.active_name()).exists());
    }
}

/// The single-file state the previous version would have written for the
/// same records, as its `persist` serialized it.
fn legacy_state_of(store: &ActivationStateStore) -> LegacyState {
    let mut state = LegacyState {
        schema_version: SCHEMA,
        session_id: store.head.session_id.clone(),
        journal: store.head.journal.clone(),
        baselines: vec![],
        inputs: vec![],
        candidates: vec![],
        candidate_reservations: vec![],
        heads: vec![],
        promotions: vec![],
        rewinds: vec![],
        promotion_reservations: vec![],
        pending: None,
    };
    for event in all_events(store) {
        match event {
            Event::Baseline(record) => state.baselines.push(record),
            Event::Input(record) => state.inputs.push(record),
            Event::CandidateReserved(record) => state.candidate_reservations.push(record),
            Event::Candidate(record) => state.candidates.push(record),
            Event::PromotionReserved { turn_id } => state.promotion_reservations.push(turn_id),
            Event::PromotionPrepared(manifest) => state.pending = Some(manifest),
            Event::PromotionFinished { .. } => {
                let manifest = state.pending.take().unwrap();
                state
                    .promotion_reservations
                    .retain(|turn| turn != manifest.closure.turn_id());
                state.promotions.push(manifest);
            }
            Event::Rewind(rewind) => state.rewinds.push(rewind),
        }
    }
    state.heads = store
        .projection
        .conversations
        .values()
        .filter_map(|conversation| conversation.head.clone())
        .collect();
    state.heads.sort_by(|a, b| {
        a.committed
            .conversation_id
            .as_str()
            .cmp(b.committed.conversation_id.as_str())
    });
    state
}

#[test]
fn an_owned_single_file_store_with_reservations_and_rewinds_migrates_exactly() {
    let mut owned = Owned::new();
    let mut memory = owned.memory(LOG_SPEC);
    let lead = NodeConversationId::new("lead").unwrap();
    let first = owned.turn(&mut memory, 0, None);
    let second = owned.turn(&mut memory, 1, Some(first.2.selected[0].committed.clone()));
    memory
        .rewind_session(
            &owned.canonical,
            &owned.content,
            Some("turn-0"),
            std::slice::from_ref(&lead),
            ToolReplayPolicy::CompleteNativeGroups,
        )
        .unwrap();
    let third = owned.turn(&mut memory, 2, Some(first.2.selected[0].committed.clone()));
    let legacy = legacy_state_of(&memory);
    assert_eq!(legacy.rewinds.len(), 1);
    assert_eq!(legacy.rewinds[0].after_promotions, 2);
    assert_eq!(legacy.candidate_reservations.len(), 3);
    let superseded = memory.superseded_turn_ids().unwrap();
    drop(memory);
    let root = owned.memory_root();
    fs::write(root.join(STATE_FILE), serde_json::to_vec(&legacy).unwrap()).unwrap();
    SegmentLog::remove(&SecureDir::open(&root).unwrap(), &LOG_SPEC).unwrap();
    let memory = owned.memory(LOG_SPEC);
    assert_eq!(legacy_state_of(&memory), legacy);
    assert_eq!(memory.superseded_turn_ids().unwrap(), superseded);
    assert_eq!(
        memory.committed_reference(&lead).unwrap(),
        Some(third.2.selected[0].committed.clone())
    );
    for (activation, accepted, manifest) in [&first, &second, &third] {
        assert_eq!(
            memory
                .promotion(&owned.canonical.snapshot(&activation.turn_id).unwrap())
                .unwrap()
                .as_ref(),
            Some(manifest)
        );
        memory.checkpoint(accepted).unwrap();
        assert!(memory
            .candidate_reservation(&owned.canonical, activation)
            .is_ok());
    }
}
