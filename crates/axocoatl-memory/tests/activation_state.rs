#![cfg(unix)]

use std::cell::RefCell;
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;

use axocoatl_core::{MessageRole, TokenUsageStats};
use axocoatl_memory::activation_state::{ActivationStateError, ActivationStateStore};
use axocoatl_memory::{AgentCheckpoint, StoredMessage};
use axocoatl_session::execution_ownership::LegacyFormatOwnership;
use axocoatl_session::execution_store::{
    DurableTurnSnapshot, ExecutionStoreOwner, SessionExecutionStore,
};
use axocoatl_session::turn_contract::*;
use serde::Deserialize;
use sha2::{Digest, Sha256};

#[derive(Deserialize)]
struct CheckpointFixture {
    version: u64,
    message: String,
}

fn checkpoint(conversation: &str, fixture: &str) -> AgentCheckpoint {
    let fixtures: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/activation_state/checkpoints.json")).unwrap();
    let fixture: CheckpointFixture = serde_json::from_value(fixtures[fixture].clone()).unwrap();
    AgentCheckpoint {
        version: fixture.version,
        agent_id: conversation.into(),
        checkpoint_time: fixture.version,
        session_messages: vec![StoredMessage {
            content_parts: None,
            role: MessageRole::Assistant,
            content: fixture.message,
            timestamp: fixture.version,
            token_count: 5,
            name: None,
            tool_calls: vec![],
            tool_call_id: None,
        }],
        cumulative_token_usage: TokenUsageStats::new(11, 7),
        cumulative_token_usage_known: true,
        behavior_state: Some("{\"node_private\":true}".into()),
    }
}

fn session() -> SessionId {
    SessionId::new("session-a").unwrap()
}
fn conversation(node: &str) -> NodeConversationId {
    NodeConversationId::new(format!("conversation-{node}")).unwrap()
}
fn evidence(value: &str) -> EvidenceRef {
    EvidenceRef::new(value).unwrap()
}
fn hash(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

struct History {
    store: Rc<RefCell<SessionExecutionStore>>,
    _root: tempfile::TempDir,
}

impl History {
    fn new() -> Self {
        Self::with_workspace("workspace-a")
    }
    fn with_workspace(workspace: &str) -> Self {
        Self::with_root(workspace, tempfile::tempdir().unwrap())
    }
    fn from_legacy(legacy: &axocoatl_session::turn_ledger::SessionTurnStore) -> Self {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("session-history");
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(
            path.join("turns.v1.jsonl"),
            fs::read(legacy.path()).unwrap(),
        )
        .unwrap();
        fs::set_permissions(
            path.join("turns.v1.jsonl"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        Self::with_root("workspace-a", root)
    }
    fn with_root(workspace: &str, root: tempfile::TempDir) -> Self {
        let ownership = Arc::new(
            LegacyFormatOwnership::acquire(root.path())
                .unwrap()
                .upgrade()
                .unwrap(),
        );
        let store = SessionExecutionStore::open(
            ownership,
            ExecutionStoreOwner {
                workspace_id: workspace.into(),
                session_id: session(),
            },
        )
        .unwrap();
        Self {
            store: Rc::new(RefCell::new(store)),
            _root: root,
        }
    }
}

struct Turn {
    history: Rc<RefCell<SessionExecutionStore>>,
    fold: TurnContract,
    id: LogicalTurnId,
    graph: TurnGraphSnapshot,
}

impl Turn {
    fn new(history: &History, id: &str, nodes: &[(&str, Option<CheckpointRef>)]) -> Self {
        let graph = TurnGraphSnapshot {
            snapshot_id: GraphSnapshotId::new(format!("graph-{id}")).unwrap(),
            revision: 1,
            nodes: nodes
                .iter()
                .map(|(node, start)| GraphNode {
                    node_id: TurnNodeId::new(*node).unwrap(),
                    slot_id: SessionTeamSlotId::new(format!("slot-{node}")).unwrap(),
                    definition: DefinitionSnapshotRef {
                        definition_id: AgentDefinitionId::new("shared-coder-template").unwrap(),
                        snapshot: evidence("definition-v1"),
                    },
                    conversation_id: conversation(node),
                    starting_savepoint: start
                        .clone()
                        .map(|checkpoint| ConversationSavepoint::Checkpoint {
                            checkpoint: Box::new(checkpoint),
                        })
                        .unwrap_or(ConversationSavepoint::Empty),
                    required: true,
                })
                .collect(),
            dependencies: vec![],
            conditions: vec![],
        };
        Self::for_graph(history, id, graph)
    }

    fn for_graph(history: &History, id: &str, graph: TurnGraphSnapshot) -> Self {
        let mut this = Self {
            history: history.store.clone(),
            fold: TurnContract::default(),
            id: LogicalTurnId::new(id).unwrap(),
            graph,
        };
        this.apply(TurnContractEvent::Begin {
            epoch_id: ExecutionEpochId::new("epoch-1").unwrap(),
            graph: this.graph.clone(),
            predecessor: None,
        });
        this
    }

    fn apply(&mut self, event: TurnContractEvent) {
        self.history
            .borrow_mut()
            .append(TurnContractEnvelope {
                schema_version: TURN_CONTRACT_SCHEMA_VERSION,
                command_id: CommandId::new(format!(
                    "{}-command-{}",
                    self.id.as_str(),
                    self.fold.revision()
                ))
                .unwrap(),
                expected_revision: self.fold.revision(),
                session_id: session(),
                turn_id: self.id.clone(),
                event,
            })
            .unwrap();
        self.fold = self.snapshot().contract().clone();
    }

    fn snapshot(&self) -> DurableTurnSnapshot {
        self.history.borrow().snapshot(&self.id).unwrap()
    }

    fn input(&self, node: &str, generation: u32) -> ActivationInputManifest {
        let declared = self
            .graph
            .nodes
            .iter()
            .find(|n| n.node_id.as_str() == node)
            .unwrap();
        ActivationInputManifest {
            manifest_id: InputManifestId::new(format!(
                "{}-{node}-input-{generation}",
                self.id.as_str()
            ))
            .unwrap(),
            activation: ActivationRef {
                session_id: session(),
                turn_id: self.id.clone(),
                execution_epoch_id: ExecutionEpochId::new("epoch-1").unwrap(),
                node_id: declared.node_id.clone(),
                generation,
                activation_id: ActivationId::new(format!(
                    "{}-{node}-activation-{generation}",
                    self.id.as_str()
                ))
                .unwrap(),
            },
            definition: declared.definition.clone(),
            conversation_id: declared.conversation_id.clone(),
            starting_savepoint: declared.starting_savepoint.clone(),
            parents: vec![],
            guidance: vec![evidence("original-guidance")],
            attachments: vec![],
            repository: RepositoryInput::Recorded {
                snapshot: evidence("repo-snapshot"),
            },
            budget: evidence("budget"),
            grant: None,
            revision_context: None,
        }
    }

    fn start(&mut self, store: &mut ActivationStateStore, node: &str) -> ActivationRef {
        let input = self.input(node, 1);
        let activation = input.activation.clone();
        self.apply(TurnContractEvent::StartActivation {
            input: Box::new(input),
        });
        store.record_input(&self.snapshot(), &activation).unwrap();
        activation
    }

    fn candidate(
        &self,
        store: &mut ActivationStateStore,
        activation: &ActivationRef,
        fixture: &str,
    ) -> CheckpointRef {
        let checkpoint = checkpoint(
            self.fold
                .activations()
                .iter()
                .find(|item| item.activation == *activation)
                .unwrap()
                .conversation_id
                .as_str(),
            fixture,
        );
        // Compatibility fixtures use an isolated root; owned fixtures exercise
        // the same explicit reservation required by the controller.
        if let Ok(reservation) = store.reserve_candidate(&self.history.borrow(), activation) {
            store
                .stage_reserved_candidate(&reservation, &checkpoint)
                .unwrap()
        } else {
            store
                .stage_candidate(&self.snapshot(), activation, &checkpoint)
                .unwrap()
        }
    }

    fn accept(&mut self, activation: &ActivationRef, checkpoint: &CheckpointRef) {
        self.apply(TurnContractEvent::AcceptActivation {
            activation: activation.clone(),
            checkpoint: Box::new(checkpoint.clone()),
            output: evidence(&format!("output-{}", activation.activation_id.as_str())),
        });
    }

    fn fail(&mut self, activation: &ActivationRef) {
        self.apply(TurnContractEvent::FailActivation {
            activation: activation.clone(),
            evidence: evidence("failed-result"),
        });
    }

    fn close(&mut self, closure: TurnClosure) {
        self.apply(TurnContractEvent::Close { closure });
    }
}

fn open(root: &Path) -> ActivationStateStore {
    ActivationStateStore::open(root, session()).unwrap()
}
const ACTIVE_SEGMENT: &str = "activation-state.active.jsonl";

/// The store's head file: its identity and journal binding.
fn read_head(root: &Path) -> serde_json::Value {
    serde_json::from_slice(&fs::read(root.join("activation-state.json")).unwrap()).unwrap()
}
fn segment_files(root: &Path) -> Vec<std::path::PathBuf> {
    let mut sealed: Vec<_> = fs::read_dir(root.join("segments"))
        .into_iter()
        .flatten()
        .map(|entry| entry.unwrap().path())
        .collect();
    sealed.sort();
    sealed.push(root.join(ACTIVE_SEGMENT));
    sealed
}
/// Every journal record, in order, from the sealed and active segments.
fn records(root: &Path) -> Vec<serde_json::Value> {
    segment_files(root)
        .iter()
        .flat_map(|path| {
            fs::read(path)
                .unwrap()
                .split(|byte| *byte == b'\n')
                .filter(|line| !line.is_empty())
                .map(|line| serde_json::from_slice::<serde_json::Value>(line).unwrap())
                .collect::<Vec<_>>()
        })
        .filter_map(|line| line.get("record").cloned())
        .collect()
}
fn count(records: &[serde_json::Value], kind: &str) -> usize {
    records
        .iter()
        .filter(|record| record.get(kind).is_some())
        .count()
}
/// A promotion decision whose pointers were not all written yet.
fn pending(root: &Path) -> Option<serde_json::Value> {
    records(root)
        .last()
        .and_then(|record| record.get("promotion_prepared").cloned())
}
/// The bytes of the head and every segment, to show that nothing was written.
fn journal_bytes(root: &Path) -> Vec<u8> {
    let mut bytes = fs::read(root.join("activation-state.json")).unwrap();
    for path in segment_files(root) {
        bytes.extend(fs::read(path).unwrap());
    }
    bytes
}
/// Make the next journal append fail.
fn block_journal(root: &Path) -> std::path::PathBuf {
    let saved = root.join("saved-segment");
    fs::rename(root.join(ACTIVE_SEGMENT), &saved).unwrap();
    fs::create_dir(root.join(ACTIVE_SEGMENT)).unwrap();
    saved
}
fn unblock_journal(root: &Path, saved: std::path::PathBuf) {
    fs::remove_dir(root.join(ACTIVE_SEGMENT)).unwrap();
    fs::rename(saved, root.join(ACTIVE_SEGMENT)).unwrap();
}
fn object_path(root: &Path, reference: &CheckpointRef) -> std::path::PathBuf {
    root.join("objects").join(format!(
        "{}.checkpoint",
        hash(reference.checkpoint_id.as_str())
    ))
}

fn accepted_turn(
    history: &History,
    store: &mut ActivationStateStore,
    id: &str,
    nodes: &[(&str, Option<CheckpointRef>)],
) -> Turn {
    let mut turn = Turn::new(history, id, nodes);
    for (node, _) in nodes {
        let activation = turn.start(store, node);
        let reference = turn.candidate(store, &activation, "accepted");
        turn.accept(&activation, &reference);
    }
    turn.close(TurnClosure::Completed);
    turn
}

#[test]
fn exact_accepted_checkpoint_wins_over_a_newer_unselected_file_and_reopens() {
    let root = tempfile::tempdir().unwrap();
    let history = History::new();
    let mut store = open(root.path());
    let mut turn = Turn::new(&history, "turn-a", &[("a", None)]);
    let activation = turn.start(&mut store, "a");
    let accepted = turn.candidate(&mut store, &activation, "accepted");
    let newer = turn.candidate(&mut store, &activation, "unselected_newer");
    assert_ne!(accepted, newer);
    turn.accept(&activation, &accepted);
    turn.close(TurnClosure::Completed);
    let manifest = store.promote(&turn.snapshot()).unwrap();
    assert_eq!(manifest.selected[0].accepted, accepted);
    assert_eq!(
        store
            .committed_checkpoint(&conversation("a"))
            .unwrap()
            .unwrap()
            .version,
        2
    );
    assert_eq!(store.checkpoint(&newer).unwrap().version, 900);
    drop(store);
    let mut store = open(root.path());
    assert_eq!(store.promote(&turn.snapshot()).unwrap(), manifest);
    assert_eq!(
        store
            .committed_checkpoint(&conversation("a"))
            .unwrap()
            .unwrap()
            .version,
        2
    );
}

#[test]
fn shared_template_nodes_have_distinct_cache_owners_and_failed_branch_keeps_prior_head() {
    let root = tempfile::tempdir().unwrap();
    let history = History::new();
    let mut store = open(root.path());
    let first = accepted_turn(
        &history,
        &mut store,
        "turn-first",
        &[("a", None), ("b", None)],
    );
    store.promote(&first.snapshot()).unwrap();
    let prior_a = store
        .committed_reference(&conversation("a"))
        .unwrap()
        .unwrap();
    let prior_b = store
        .committed_reference(&conversation("b"))
        .unwrap()
        .unwrap();
    assert_ne!(prior_a.conversation_id, prior_b.conversation_id);
    let mut second = Turn::new(
        &history,
        "turn-second",
        &[("a", Some(prior_a)), ("b", Some(prior_b.clone()))],
    );
    let a = second.start(&mut store, "a");
    let b = second.start(&mut store, "b");
    assert_eq!(
        store.starting_checkpoint(&b).unwrap().unwrap().agent_id,
        "conversation-b"
    );
    let a_new = second.candidate(&mut store, &a, "replacement");
    let failed = second.candidate(&mut store, &b, "failed");
    second.accept(&a, &a_new);
    second.fail(&b);
    assert!(store
        .stage_candidate(
            &second.snapshot(),
            &b,
            &checkpoint("conversation-b", "replacement")
        )
        .is_err());
    second.close(TurnClosure::Finished);
    // Even a corrupt, numerically newer failed artifact cannot make promotion
    // choose it or silently fall back; it remains unavailable audit evidence.
    fs::write(
        object_path(root.path(), &failed),
        b"corrupt failed artifact",
    )
    .unwrap();
    let promotion = store.promote(&second.snapshot()).unwrap();
    assert_eq!(promotion.selected.len(), 1);
    assert_eq!(
        store.committed_reference(&conversation("b")).unwrap(),
        Some(prior_b)
    );
    assert_eq!(
        store
            .committed_checkpoint(&conversation("a"))
            .unwrap()
            .unwrap()
            .version,
        1
    );
    assert_eq!(
        store
            .committed_checkpoint(&conversation("b"))
            .unwrap()
            .unwrap()
            .version,
        2
    );
    assert!(store.checkpoint(&failed).is_err());
    drop(store);
    assert_eq!(
        open(root.path())
            .committed_checkpoint(&conversation("a"))
            .unwrap()
            .unwrap()
            .agent_id,
        "conversation-a"
    );
}

#[test]
fn superseded_generation_retains_artifact_but_never_becomes_current_promotion() {
    let root = tempfile::tempdir().unwrap();
    let history = History::new();
    let mut store = open(root.path());
    let mut turn = Turn::new(&history, "turn-revise", &[("a", None)]);
    let old = turn.start(&mut store, "a");
    let old_ref = turn.candidate(&mut store, &old, "unselected_newer");
    turn.accept(&old, &old_ref);
    let mut input = turn.input("a", 2);
    input.guidance.push(evidence("explicit-revision"));
    let next = input.activation.clone();
    turn.apply(TurnContractEvent::ReviseAccepted {
        previous: old.clone(),
        input: Box::new(input),
        invalidated_descendants: vec![],
        evidence: evidence("human-revision"),
    });
    store.record_input(&turn.snapshot(), &next).unwrap();
    assert!(store.starting_checkpoint(&next).unwrap().is_none());
    turn.apply(TurnContractEvent::StartPreparedActivation {
        activation: next.clone(),
    });
    assert!(store
        .stage_candidate(
            &turn.snapshot(),
            &old,
            &checkpoint("conversation-a", "failed")
        )
        .is_err());
    let next_ref = turn.candidate(&mut store, &next, "replacement");
    turn.accept(&next, &next_ref);
    turn.close(TurnClosure::Completed);
    assert_eq!(
        turn.fold.activations()[0].state,
        ActivationState::Superseded
    );
    let manifest = store.promote(&turn.snapshot()).unwrap();
    assert_eq!(manifest.selected[0].accepted, next_ref);
    assert_eq!(store.checkpoint(&old_ref).unwrap().version, 900);
    assert_eq!(
        store
            .committed_checkpoint(&conversation("a"))
            .unwrap()
            .unwrap()
            .version,
        1
    );
}

#[test]
fn partial_multi_node_promotion_blocks_restore_and_reopen_finishes_exact_manifest() {
    let root = tempfile::tempdir().unwrap();
    let history = History::new();
    let mut store = open(root.path());
    let turn = accepted_turn(
        &history,
        &mut store,
        "turn-partial",
        &[("a", None), ("b", None)],
    );
    let blocked_head = root
        .path()
        .join("heads")
        .join(format!("{}.json", hash("conversation-b")));
    fs::create_dir(&blocked_head).unwrap();
    assert!(store.promote(&turn.snapshot()).is_err());
    assert!(matches!(
        store.committed_checkpoint(&conversation("a")),
        Err(ActivationStateError::RecoveryRequired)
    ));
    assert_eq!(
        pending(root.path()).unwrap()["selected"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(count(&records(root.path()), "promotion_finished"), 0);
    assert!(root
        .path()
        .join("heads")
        .join(format!("{}.json", hash("conversation-a")))
        .is_file());
    drop(store);
    fs::remove_dir(&blocked_head).unwrap();
    let store = open(root.path());
    for node in ["a", "b"] {
        assert_eq!(
            store
                .committed_checkpoint(&conversation(node))
                .unwrap()
                .unwrap()
                .agent_id,
            format!("conversation-{node}")
        );
    }
    assert!(pending(root.path()).is_none());
    assert_eq!(count(&records(root.path()), "promotion_prepared"), 1);
    assert_eq!(count(&records(root.path()), "promotion_finished"), 1);
}

#[test]
fn recovery_refuses_missing_selected_artifact_until_exact_bytes_are_restored() {
    let root = tempfile::tempdir().unwrap();
    let history = History::new();
    let mut store = open(root.path());
    let turn = accepted_turn(
        &history,
        &mut store,
        "turn-missing",
        &[("a", None), ("b", None)],
    );
    let selected = turn.fold.current_accepted_activations()[0]
        .checkpoint
        .as_ref()
        .unwrap()
        .clone();
    let artifact = object_path(root.path(), &selected);
    let bytes = fs::read(&artifact).unwrap();
    let blocked = root
        .path()
        .join("heads")
        .join(format!("{}.json", hash("conversation-b")));
    fs::create_dir(&blocked).unwrap();
    assert!(store.promote(&turn.snapshot()).is_err());
    drop(store);
    fs::remove_dir(blocked).unwrap();
    fs::remove_file(&artifact).unwrap();
    assert!(ActivationStateStore::open(root.path(), session()).is_err());
    assert!(pending(root.path()).is_some());
    fs::write(artifact, bytes).unwrap();
    assert!(open(root.path())
        .committed_checkpoint(&conversation("b"))
        .unwrap()
        .is_some());
}

#[test]
fn ownership_aliases_digest_corruption_and_template_cache_are_refused() {
    let root = tempfile::tempdir().unwrap();
    let history = History::new();
    let mut store = open(root.path());
    let mut turn = Turn::new(&history, "turn-owners", &[("a", None), ("b", None)]);
    let a = turn.start(&mut store, "a");
    let reference = turn.candidate(&mut store, &a, "accepted");
    assert!(store
        .stage_candidate(
            &turn.snapshot(),
            &a,
            &checkpoint("shared-coder-template", "accepted")
        )
        .is_err());
    for change in ["session", "conversation", "activation"] {
        let mut foreign = reference.clone();
        match change {
            "session" => foreign.session_id = SessionId::new("another-session").unwrap(),
            "conversation" => foreign.conversation_id = conversation("b"),
            _ => {
                if let CheckpointSource::Accepted { activation } = &mut foreign.source {
                    activation.generation += 1;
                }
            }
        }
        assert!(store.checkpoint(&foreign).is_err());
    }
    turn.accept(&a, &reference);
    turn.close(TurnClosure::Finished);
    fs::write(object_path(root.path(), &reference), b"different bytes").unwrap();
    assert!(store.promote(&turn.snapshot()).is_err());
    assert_eq!(count(&records(root.path()), "promotion_prepared"), 0);
}

#[test]
fn immutable_input_cannot_be_rebound_by_another_canonical_journal() {
    let root = tempfile::tempdir().unwrap();
    let history = History::new();
    let mut store = open(root.path());
    let mut first = Turn::new(&history, "turn-input", &[("a", None)]);
    let activation = first.start(&mut store, "a");
    let before = journal_bytes(root.path());
    let foreign = History::new();
    let mut other = Turn::new(&foreign, "turn-input", &[("a", None)]);
    let mut changed = other.input("a", 1);
    changed.guidance.push(evidence("different-input"));
    other.apply(TurnContractEvent::StartActivation {
        input: Box::new(changed),
    });
    assert!(store.record_input(&other.snapshot(), &activation).is_err());
    assert_eq!(journal_bytes(root.path()), before);
    assert!(store.starting_checkpoint(&activation).unwrap().is_none());
}

#[test]
fn cancelled_generation_and_replayed_older_turns_do_not_replace_current_committed_state() {
    let root = tempfile::tempdir().unwrap();
    let history = History::new();
    let mut store = open(root.path());
    let first = accepted_turn(&history, &mut store, "turn-first", &[("a", None)]);
    store.promote(&first.snapshot()).unwrap();
    let old = store
        .committed_reference(&conversation("a"))
        .unwrap()
        .unwrap();
    let second = accepted_turn(
        &history,
        &mut store,
        "turn-second",
        &[("a", Some(old.clone()))],
    );
    store.promote(&second.snapshot()).unwrap();
    let current = store
        .committed_reference(&conversation("a"))
        .unwrap()
        .unwrap();
    assert_ne!(old, current);
    store.promote(&first.snapshot()).unwrap();
    assert_eq!(
        store.committed_reference(&conversation("a")).unwrap(),
        Some(current.clone())
    );
    let mut cancelled = Turn::new(&history, "turn-cancelled", &[("a", Some(current.clone()))]);
    let activation = cancelled.start(&mut store, "a");
    let candidate = cancelled.candidate(&mut store, &activation, "failed");
    cancelled.fail(&activation);
    cancelled.close(TurnClosure::Cancelled);
    assert_eq!(
        store.checkpoint(&candidate).unwrap().session_messages[0].content,
        checkpoint("conversation-a", "failed").session_messages[0].content
    );
    assert!(store
        .promote(&cancelled.snapshot())
        .unwrap()
        .selected
        .is_empty());
    assert_eq!(
        store.committed_reference(&conversation("a")).unwrap(),
        Some(current)
    );
}

#[test]
fn stale_starting_state_cannot_overwrite_a_conversation_that_advanced() {
    let root = tempfile::tempdir().unwrap();
    let history = History::new();
    let mut store = open(root.path());
    let first = accepted_turn(&history, &mut store, "turn-first", &[("a", None)]);
    store.promote(&first.snapshot()).unwrap();
    let old = store
        .committed_reference(&conversation("a"))
        .unwrap()
        .unwrap();
    let stale = accepted_turn(
        &history,
        &mut store,
        "turn-stale",
        &[("a", Some(old.clone()))],
    );
    let current = accepted_turn(&history, &mut store, "turn-current", &[("a", Some(old))]);
    store.promote(&current.snapshot()).unwrap();
    let before = journal_bytes(root.path());
    assert!(store.promote(&stale.snapshot()).is_err());
    assert_eq!(journal_bytes(root.path()), before);
}

#[test]
fn failure_before_durable_promotion_intent_leaves_all_conversation_heads_untouched() {
    let root = tempfile::tempdir().unwrap();
    let history = History::new();
    let mut store = open(root.path());
    let turn = accepted_turn(
        &history,
        &mut store,
        "turn-conflict",
        &[("a", None), ("b", None)],
    );
    let saved = block_journal(root.path());
    assert!(store.promote(&turn.snapshot()).is_err());
    assert!(fs::read_dir(root.path().join("heads"))
        .unwrap()
        .next()
        .is_none());
    assert!(matches!(
        store.committed_reference(&conversation("a")),
        Err(ActivationStateError::RecoveryRequired)
    ));
    drop(store);
    unblock_journal(root.path(), saved);
    let mut store = open(root.path());
    assert!(store
        .committed_reference(&conversation("a"))
        .unwrap()
        .is_none());
    assert_eq!(store.promote(&turn.snapshot()).unwrap().selected.len(), 2);
}

#[test]
fn corrupt_future_or_misbound_store_and_linked_paths_fail_closed() {
    for corruption in ["future", "foreign_session", "candidate_owner", "heads"] {
        let root = tempfile::tempdir().unwrap();
        let history = History::new();
        let mut store = open(root.path());
        let turn = accepted_turn(&history, &mut store, "turn-corrupt", &[("a", None)]);
        store.promote(&turn.snapshot()).unwrap();
        drop(store);
        let mut head = read_head(root.path());
        match corruption {
            "future" => head["schema_version"] = 2.into(),
            "foreign_session" => head["session_id"] = "another-session".into(),
            "candidate_owner" => {
                let active = fs::read(root.path().join(ACTIVE_SEGMENT)).unwrap();
                let mut changed = Vec::new();
                for line in active.split_inclusive(|byte| *byte == b'\n') {
                    let mut value: serde_json::Value = serde_json::from_slice(line).unwrap();
                    if let Some(candidate) = value
                        .get_mut("record")
                        .and_then(|record| record.get_mut("candidate"))
                    {
                        candidate["reference"]["conversation_id"] = "another-conversation".into();
                        changed.extend(serde_json::to_vec(&value).unwrap());
                        changed.push(b'\n');
                    } else {
                        changed.extend(line);
                    }
                }
                assert_ne!(changed, active);
                fs::write(root.path().join(ACTIVE_SEGMENT), changed).unwrap();
            }
            _ => fs::remove_file(
                root.path()
                    .join("heads")
                    .join(format!("{}.json", hash("conversation-a"))),
            )
            .unwrap(),
        }
        fs::write(
            root.path().join("activation-state.json"),
            serde_json::to_vec(&head).unwrap(),
        )
        .unwrap();
        let bytes = journal_bytes(root.path());
        assert!(
            ActivationStateStore::open(root.path(), session()).is_err(),
            "{corruption}"
        );
        assert_eq!(journal_bytes(root.path()), bytes, "{corruption}");
    }
    let parent = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    symlink(target.path(), parent.path().join("linked")).unwrap();
    assert!(ActivationStateStore::open(parent.path().join("linked"), session()).is_err());
    assert!(fs::read_dir(target.path()).unwrap().next().is_none());
}

#[test]
fn exclusive_store_and_current_pointer_validation_prevent_parallel_or_stale_restore() {
    let root = tempfile::tempdir().unwrap();
    let history = History::new();
    let mut store = open(root.path());
    assert!(ActivationStateStore::open(root.path(), session()).is_err());
    let turn = accepted_turn(&history, &mut store, "turn-pointer", &[("a", None)]);
    store.promote(&turn.snapshot()).unwrap();
    fs::write(
        root.path()
            .join("heads")
            .join(format!("{}.json", hash("conversation-a"))),
        b"{}",
    )
    .unwrap();
    assert!(store.committed_checkpoint(&conversation("a")).is_err());
    drop(store);
    assert!(ActivationStateStore::open(root.path(), session()).is_err());
}

#[test]
fn successor_turn_can_change_node_identity_while_retaining_slot_conversation() {
    let root = tempfile::tempdir().unwrap();
    let history = History::new();
    let mut store = open(root.path());
    let first = accepted_turn(&history, &mut store, "first", &[("a", None)]);
    store.promote(&first.snapshot()).unwrap();
    let prior = store
        .committed_reference(&conversation("a"))
        .unwrap()
        .unwrap();
    let mut graph = first.graph.clone();
    graph.snapshot_id = GraphSnapshotId::new("second-graph").unwrap();
    graph.nodes[0].node_id = TurnNodeId::new("renamed-node").unwrap();
    graph.nodes[0].starting_savepoint = ConversationSavepoint::Checkpoint {
        checkpoint: Box::new(prior.clone()),
    };
    let mut second = Turn::for_graph(&history, "second", graph);
    let activation = second.start(&mut store, "renamed-node");
    assert_eq!(
        store
            .starting_checkpoint(&activation)
            .unwrap()
            .unwrap()
            .agent_id,
        "conversation-a"
    );
    let candidate = second.candidate(&mut store, &activation, "replacement");
    second.accept(&activation, &candidate);
    second.close(TurnClosure::Completed);
    let promoted = store.promote(&second.snapshot()).unwrap();
    assert_eq!(promoted.selected[0].slot_id.as_str(), "slot-a");
    assert_eq!(promoted.selected[0].node_id.as_str(), "renamed-node");
    assert_eq!(promoted.selected[0].previous_committed, Some(prior));
    drop(store);
    assert_eq!(
        open(root.path())
            .committed_checkpoint(&conversation("a"))
            .unwrap()
            .unwrap()
            .version,
        1
    );
}

#[test]
fn different_slot_cannot_claim_a_retained_conversation_in_a_successor() {
    let root = tempfile::tempdir().unwrap();
    let history = History::new();
    let mut store = open(root.path());
    let first = accepted_turn(&history, &mut store, "first", &[("a", None)]);
    store.promote(&first.snapshot()).unwrap();
    let prior = store
        .committed_reference(&conversation("a"))
        .unwrap()
        .unwrap();
    let mut graph = first.graph.clone();
    graph.snapshot_id = GraphSnapshotId::new("second-graph").unwrap();
    graph.nodes[0].slot_id = SessionTeamSlotId::new("foreign-slot").unwrap();
    graph.nodes[0].starting_savepoint = ConversationSavepoint::Checkpoint {
        checkpoint: Box::new(prior),
    };
    let mut second = Turn::for_graph(&history, "second", graph);
    let input = second.input("a", 1);
    let activation = input.activation.clone();
    second.apply(TurnContractEvent::StartActivation {
        input: Box::new(input),
    });
    let before = journal_bytes(root.path());
    assert!(store.record_input(&second.snapshot(), &activation).is_err());
    assert_eq!(journal_bytes(root.path()), before);
}

#[test]
fn foreign_canonical_journals_cannot_stage_promote_or_alias_artifact_identity() {
    for workspace in ["workspace-a", "other-workspace"] {
        let root = tempfile::tempdir().unwrap();
        let history = History::new();
        let mut store = open(root.path());
        let mut first = Turn::new(&history, "same-turn", &[("a", None)]);
        let activation = first.start(&mut store, "a");
        let reference = first.candidate(&mut store, &activation, "accepted");
        let foreign_root = tempfile::tempdir().unwrap();
        let foreign_history = History::with_workspace(workspace);
        let mut foreign_store = open(foreign_root.path());
        let mut other = Turn::new(&foreign_history, "same-turn", &[("a", None)]);
        let other_activation = other.start(&mut foreign_store, "a");
        assert_eq!(activation, other_activation);
        let before = journal_bytes(root.path());
        assert!(store.record_input(&other.snapshot(), &activation).is_err());
        assert!(store
            .stage_candidate(
                &other.snapshot(),
                &activation,
                &checkpoint("conversation-a", "accepted")
            )
            .is_err());
        let foreign_reference = other.candidate(&mut foreign_store, &activation, "accepted");
        assert_ne!(reference.checkpoint_id, foreign_reference.checkpoint_id);
        assert!(store.checkpoint(&foreign_reference).is_err());
        other.accept(&activation, &foreign_reference);
        other.close(TurnClosure::Completed);
        assert!(store.promote(&other.snapshot()).is_err());
        assert_eq!(journal_bytes(root.path()), before);
    }
}

#[test]
fn owned_open_binds_canonical_identity_before_any_input_and_holds_all_leases() {
    use axocoatl_session::execution_namespace::ExecutionComponent;
    use axocoatl_session::execution_ownership::UpgradedFormatOwnership;
    let History {
        store: history,
        _root: root,
    } = History::new();
    let identity = history.borrow().identity().unwrap();
    let path = history
        .borrow()
        .path()
        .parent()
        .unwrap()
        .join("activation-state");
    let namespace = history
        .borrow()
        .component_namespace(ExecutionComponent::ActivationState)
        .unwrap();
    let store = ActivationStateStore::open_owned(namespace).unwrap();
    let head = read_head(&path);
    assert_eq!(head["journal"]["journal_id"], identity.journal_id());
    assert_eq!(
        head["journal"]["workspace_id"],
        identity.owner().workspace_id
    );
    assert!(records(&path).is_empty());
    assert!(history
        .borrow()
        .component_namespace(ExecutionComponent::ActivationState)
        .is_err());
    drop(history);
    assert!(UpgradedFormatOwnership::open(root.path()).is_err());
    assert!(store
        .committed_reference(&conversation("a"))
        .unwrap()
        .is_none());
    drop(store);
    UpgradedFormatOwnership::open(root.path()).unwrap();
}

#[test]
fn owned_open_refuses_wrong_component_child_and_foreign_journal_without_rebinding() {
    use axocoatl_session::execution_namespace::ExecutionComponent;
    let history = History::new();
    let wrong = history
        .store
        .borrow()
        .component_namespace(ExecutionComponent::ExecutionContent)
        .unwrap();
    assert!(ActivationStateStore::open_owned(wrong).is_err());
    let namespace = history
        .store
        .borrow()
        .component_namespace(ExecutionComponent::ActivationState)
        .unwrap();
    let child = namespace.child("not-root").unwrap();
    assert!(ActivationStateStore::open_owned(child).is_err());
    // The extra directory is deliberate unavailable-identity evidence, not a
    // valid empty store. Remove only this test fixture before initial binding.
    let path = history
        .store
        .borrow()
        .path()
        .parent()
        .unwrap()
        .join("activation-state");
    fs::remove_dir(path.join("not-root")).unwrap();
    let store = ActivationStateStore::open_owned(namespace).unwrap();
    drop(store);
    let mut head = read_head(&path);
    head["journal"]["journal_id"] = "11111111-1111-4111-8111-111111111111".into();
    let bytes = serde_json::to_vec(&head).unwrap();
    fs::write(path.join("activation-state.json"), &bytes).unwrap();
    let namespace = history
        .store
        .borrow()
        .component_namespace(ExecutionComponent::ActivationState)
        .unwrap();
    assert!(ActivationStateStore::open_owned(namespace).is_err());
    assert_eq!(fs::read(path.join("activation-state.json")).unwrap(), bytes);
}

#[derive(Clone, Deserialize)]
struct LegacyIngress {
    begin: axocoatl_session::turn_ledger::BeginSessionTurn,
    execution: axocoatl_session::turn_ledger::RecordTurnExecution,
    agent_output: LegacyOutput,
    #[serde(default)]
    tool_events: Vec<axocoatl_session::turn_ledger::RecordTurnExecution>,
    complete: axocoatl_session::turn_ledger::TransitionSessionTurn,
}
#[derive(Clone, Deserialize)]
struct LegacyOutput {
    agent_id: String,
    model: Option<String>,
    output: String,
}

fn legacy_fixture() -> Vec<LegacyIngress> {
    serde_json::from_str(include_str!(
        "fixtures/activation_state/ordinary_v1_ingress.json"
    ))
    .unwrap()
}
fn append_legacy(
    store: &mut axocoatl_session::turn_ledger::SessionTurnStore,
    fixture: &LegacyIngress,
) {
    let id = fixture.begin.turn_id.as_ref().unwrap();
    store.begin(fixture.begin.clone()).unwrap();
    store
        .record_execution(id, format!("run-started:{id}"), fixture.execution.clone())
        .unwrap();
    for (index, event) in fixture.tool_events.iter().enumerate() {
        store
            .record_execution(id, format!("tool:{id}:{index}"), event.clone())
            .unwrap();
    }
    store
        .record_agent_output(
            id,
            format!("agent-output:{id}:0:coder"),
            &fixture.agent_output.agent_id,
            fixture.agent_output.model.clone(),
            fixture.agent_output.output.clone(),
            None,
        )
        .unwrap();
    store
        .transition(id, format!("terminal:{id}"), fixture.complete.clone())
        .unwrap();
}
fn seal_legacy(
    history: &History,
) -> (
    axocoatl_session::execution_content::ExecutionContentStore,
    axocoatl_session::execution_store::DurableLegacySeal,
) {
    use axocoatl_session::execution_namespace::ExecutionComponent;
    let namespace = history
        .store
        .borrow()
        .component_namespace(ExecutionComponent::ExecutionContent)
        .unwrap();
    let mut content =
        axocoatl_session::execution_content::ExecutionContentStore::open_owned(namespace).unwrap();
    let source = history.store.borrow().legacy_history_snapshot().unwrap();
    let retained = content.retain_legacy_history(&source).unwrap();
    let seal = history
        .store
        .borrow_mut()
        .seal_legacy_history(&retained)
        .unwrap();
    (content, seal)
}
fn memory_owned(history: &History) -> ActivationStateStore {
    let namespace = history
        .store
        .borrow()
        .component_namespace(
            axocoatl_session::execution_namespace::ExecutionComponent::ActivationState,
        )
        .unwrap();
    ActivationStateStore::open_owned(namespace).unwrap()
}

#[test]
fn owned_memory_marker_refuses_missing_primary_even_without_any_retained_artifacts() {
    let history = History::new();
    let store = memory_owned(&history);
    let path = history
        .store
        .borrow()
        .path()
        .parent()
        .unwrap()
        .join("activation-state");
    drop(store);
    let original = fs::read(path.join("activation-state.json")).unwrap();
    fs::remove_file(path.join("activation-state.json")).unwrap();
    fs::remove_dir(path.join("objects")).unwrap();
    fs::remove_dir(path.join("heads")).unwrap();
    let namespace = history
        .store
        .borrow()
        .component_namespace(
            axocoatl_session::execution_namespace::ExecutionComponent::ActivationState,
        )
        .unwrap();
    assert!(ActivationStateStore::open_owned(namespace).is_err());
    assert!(!path.join("activation-state.json").exists());
    // Recovery here has exact prior bytes from the private fixture; ordinary
    // open must never infer those missing bytes or reset the identity itself.
    fs::write(path.join("activation-state.json"), original).unwrap();
    assert!(memory_owned(&history)
        .committed_reference(&conversation("a"))
        .unwrap()
        .is_none());
}

fn baseline_projection(
    content: &axocoatl_session::execution_content::ExecutionContentStore,
    seal: &axocoatl_session::execution_store::DurableLegacySeal,
) -> axocoatl_memory::activation_state::LegacyBaselineProjection {
    axocoatl_memory::activation_state::LegacyBaselineProjection::from_sealed_history(
        content,
        seal,
        SessionTeamSlotId::new("slot-a").unwrap(),
        conversation("a"),
    )
    .unwrap()
}

#[test]
fn real_legacy_ingress_becomes_immutable_baseline_then_exact_v2_starting_state() {
    let legacy_root = tempfile::tempdir().unwrap();
    let mut legacy =
        axocoatl_session::turn_ledger::SessionTurnStore::open(legacy_root.path()).unwrap();
    let fixtures = legacy_fixture();
    for fixture in &fixtures {
        append_legacy(&mut legacy, fixture);
    }
    let original = fs::read(legacy.path()).unwrap();
    let history = History::from_legacy(&legacy);
    let (content, seal) = seal_legacy(&history);
    let projection = baseline_projection(&content, &seal);
    let mut store = memory_owned(&history);
    let baseline = store
        .import_legacy_baseline(&history.store.borrow(), &projection)
        .unwrap();
    assert_eq!(
        store
            .import_legacy_baseline(&history.store.borrow(), &projection)
            .unwrap(),
        baseline
    );
    assert!(matches!(
        baseline.source,
        CheckpointSource::Committed { .. }
    ));
    assert_eq!(projection.frontier(), seal.reference());
    let checkpoint = store
        .committed_checkpoint(&conversation("a"))
        .unwrap()
        .unwrap();
    assert_eq!(checkpoint.agent_id, "conversation-a");
    assert_eq!(
        checkpoint
            .session_messages
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>(),
        fixtures
            .iter()
            .flat_map(|fixture| [
                fixture.begin.user_input.as_str(),
                fixture.agent_output.output.as_str()
            ])
            .collect::<Vec<_>>()
    );
    assert_eq!(
        checkpoint.cumulative_token_usage,
        TokenUsageStats::new(30, 15).with_reasoning(6)
    );
    assert!(checkpoint.cumulative_token_usage_known);
    assert!(checkpoint.behavior_state.is_none());
    let path = history
        .store
        .borrow()
        .path()
        .parent()
        .unwrap()
        .join("activation-state");
    let records = records(&path);
    assert_eq!(count(&records, "baseline"), 1);
    assert_eq!(count(&records, "promotion_prepared"), 0);
    assert_eq!(count(&records, "candidate"), 0);
    drop(store);
    let mut store = memory_owned(&history);
    let mut turn = Turn::new(&history, "v2-turn", &[("a", Some(baseline.clone()))]);
    // A stale projection cannot be imported after canonical Begin, even when
    // no physical v2 input has been recorded yet.
    assert!(store
        .import_legacy_baseline(&history.store.borrow(), &projection)
        .is_err());
    let activation = turn.start(&mut store, "a");
    assert_eq!(
        store
            .starting_checkpoint(&activation)
            .unwrap()
            .unwrap()
            .session_messages
            .len(),
        4
    );
    let candidate = turn.candidate(&mut store, &activation, "replacement");
    turn.accept(&activation, &candidate);
    turn.close(TurnClosure::Completed);
    let promotion = store.promote(&turn.snapshot()).unwrap();
    assert_eq!(promotion.selected[0].previous_committed, Some(baseline));
    let legacy_baseline = store
        .legacy_baseline_checkpoint(&conversation("a"))
        .unwrap()
        .unwrap();
    assert_eq!(legacy_baseline.session_messages.len(), 4);
    assert_eq!(
        legacy_baseline.cumulative_token_usage,
        TokenUsageStats::new(30, 15).with_reasoning(6)
    );
    let promoted = store
        .committed_checkpoint(&conversation("a"))
        .unwrap()
        .unwrap();
    assert_eq!(promoted.session_messages.len(), 1);
    assert_eq!(
        promoted.session_messages[0].content,
        "Explicitly revised accepted answer"
    );
    assert_ne!(
        promoted.session_messages[0].content,
        legacy_baseline.session_messages[0].content
    );
    assert_eq!(fs::read(legacy.path()).unwrap(), original);
}

#[test]
fn rewind_excludes_superseded_conversation_but_keeps_dispatched_usage_and_unknown_subtotals() {
    let legacy_root = tempfile::tempdir().unwrap();
    let mut legacy =
        axocoatl_session::turn_ledger::SessionTurnStore::open(legacy_root.path()).unwrap();
    let mut fixtures = legacy_fixture();
    fixtures[1].complete.metadata.remove("token_usage_known");
    for fixture in &fixtures {
        append_legacy(&mut legacy, fixture);
    }
    legacy
        .rewind("session-a", Some("legacy-turn-1"), "rewind")
        .unwrap();
    let history = History::from_legacy(&legacy);
    let (content, seal) = seal_legacy(&history);
    assert_eq!(content.read_legacy_history(&seal).unwrap().turns.len(), 2);
    let projection = baseline_projection(&content, &seal);
    let mut store = memory_owned(&history);
    store
        .import_legacy_baseline(&history.store.borrow(), &projection)
        .unwrap();
    let checkpoint = store
        .committed_checkpoint(&conversation("a"))
        .unwrap()
        .unwrap();
    assert_eq!(checkpoint.session_messages.len(), 2);
    assert_eq!(
        checkpoint.session_messages[1].content,
        fixtures[0].agent_output.output
    );
    assert_eq!(
        checkpoint.cumulative_token_usage,
        TokenUsageStats::new(30, 15).with_reasoning(6)
    );
    assert!(!checkpoint.cumulative_token_usage_known);
}

#[test]
fn legacy_context_tools_private_state_and_mixed_agents_are_explicitly_unsupported() {
    use axocoatl_memory::activation_state::LegacyBaselineProjection;
    for case in ["context", "tool", "private", "mixed", "failed", "empty"] {
        let legacy_root = tempfile::tempdir().unwrap();
        let mut legacy =
            axocoatl_session::turn_ledger::SessionTurnStore::open(legacy_root.path()).unwrap();
        let mut fixtures = legacy_fixture();
        match case {
            "context" => fixtures[0].begin.context = serde_json::from_value(serde_json::json!([{
                "reference_id":"context-1", "display_name":"file", "kind":"code_selection", "scope":"this_turn",
                "metadata":{"content":"must not silently disappear"}
            }])).unwrap(),
            "tool" => fixtures[0].execution.kind = "tool_started".into(),
            "private" => { fixtures[0].begin.metadata.insert("behavior_state".into(), "private".into()); },
            "mixed" => { fixtures[1].begin.agent_id = Some("reviewer".into()); fixtures[1].agent_output.agent_id = "reviewer".into(); },
            "failed" => fixtures[0].complete.status = axocoatl_session::turn_ledger::SessionTurnLifecycle::Failed,
            "empty" => fixtures.clear(),
            _ => unreachable!(),
        }
        for fixture in &fixtures {
            append_legacy(&mut legacy, fixture);
        }
        let original = if case == "empty" {
            Vec::new()
        } else {
            fs::read(legacy.path()).unwrap()
        };
        let history = History::from_legacy(&legacy);
        let (content, seal) = seal_legacy(&history);
        assert!(
            matches!(
                LegacyBaselineProjection::from_sealed_history(
                    &content,
                    &seal,
                    SessionTeamSlotId::new("slot-a").unwrap(),
                    conversation("a")
                ),
                Err(ActivationStateError::UnsupportedLegacy(_))
            ),
            "{case}"
        );
        assert_eq!(fs::read(legacy.path()).unwrap_or_default(), original);
    }
}

#[test]
fn legacy_baseline_rejects_foreign_journal_and_changed_slot_conversation_binding() {
    use axocoatl_memory::activation_state::LegacyBaselineProjection;
    let legacy_root = tempfile::tempdir().unwrap();
    let mut legacy =
        axocoatl_session::turn_ledger::SessionTurnStore::open(legacy_root.path()).unwrap();
    append_legacy(&mut legacy, &legacy_fixture()[0]);
    let history = History::from_legacy(&legacy);
    let (content, seal) = seal_legacy(&history);
    let projection = baseline_projection(&content, &seal);
    let foreign = History::from_legacy(&legacy);
    let (_foreign_content, _foreign_seal) = seal_legacy(&foreign);
    let mut foreign_memory = memory_owned(&foreign);
    assert!(foreign_memory
        .import_legacy_baseline(&foreign.store.borrow(), &projection)
        .is_err());
    assert!(foreign_memory
        .committed_reference(&conversation("a"))
        .unwrap()
        .is_none());
    let mut store = memory_owned(&history);
    let reference = store
        .import_legacy_baseline(&history.store.borrow(), &projection)
        .unwrap();
    let changed = LegacyBaselineProjection::from_sealed_history(
        &content,
        &seal,
        SessionTeamSlotId::new("slot-a").unwrap(),
        conversation("b"),
    )
    .unwrap();
    assert!(store
        .import_legacy_baseline(&history.store.borrow(), &changed)
        .is_err());
    assert_eq!(
        store.committed_reference(&conversation("a")).unwrap(),
        Some(reference)
    );
    assert!(store
        .committed_reference(&conversation("b"))
        .unwrap()
        .is_none());
}

#[test]
fn corrupt_or_missing_committed_baseline_bytes_are_never_replaced_by_idempotent_import() {
    for missing in [false, true] {
        let legacy_root = tempfile::tempdir().unwrap();
        let mut legacy =
            axocoatl_session::turn_ledger::SessionTurnStore::open(legacy_root.path()).unwrap();
        append_legacy(&mut legacy, &legacy_fixture()[0]);
        let history = History::from_legacy(&legacy);
        let (content, seal) = seal_legacy(&history);
        let projection = baseline_projection(&content, &seal);
        let mut store = memory_owned(&history);
        let reference = store
            .import_legacy_baseline(&history.store.borrow(), &projection)
            .unwrap();
        let root = history
            .store
            .borrow()
            .path()
            .parent()
            .unwrap()
            .join("activation-state");
        let artifact = object_path(&root, &reference);
        let original = fs::read(&artifact).unwrap();
        if missing {
            fs::remove_file(&artifact).unwrap();
        } else {
            fs::write(&artifact, b"corrupt baseline").unwrap();
        }
        assert!(store
            .import_legacy_baseline(&history.store.borrow(), &projection)
            .is_err());
        assert!(store.committed_checkpoint(&conversation("a")).is_err());
        drop(store);
        let namespace = history
            .store
            .borrow()
            .component_namespace(
                axocoatl_session::execution_namespace::ExecutionComponent::ActivationState,
            )
            .unwrap();
        assert!(ActivationStateStore::open_owned(namespace).is_err());
        if missing {
            assert!(!artifact.exists());
        } else {
            assert_eq!(fs::read(&artifact).unwrap(), b"corrupt baseline");
        }
        fs::write(&artifact, original).unwrap();
        assert!(memory_owned(&history)
            .committed_checkpoint(&conversation("a"))
            .unwrap()
            .is_some());
    }
}

#[test]
fn interrupted_baseline_import_reuses_exact_orphan_bytes_only_after_reopen() {
    let legacy_root = tempfile::tempdir().unwrap();
    let mut legacy =
        axocoatl_session::turn_ledger::SessionTurnStore::open(legacy_root.path()).unwrap();
    append_legacy(&mut legacy, &legacy_fixture()[0]);
    let history = History::from_legacy(&legacy);
    let (content, seal) = seal_legacy(&history);
    let projection = baseline_projection(&content, &seal);
    let mut store = memory_owned(&history);
    let root = history
        .store
        .borrow()
        .path()
        .parent()
        .unwrap()
        .join("activation-state");
    let saved = block_journal(&root);
    assert!(store
        .import_legacy_baseline(&history.store.borrow(), &projection)
        .is_err());
    assert!(matches!(
        store.committed_reference(&conversation("a")),
        Err(ActivationStateError::RecoveryRequired)
    ));
    let artifact = object_path(&root, projection.reference());
    let orphan = fs::read(&artifact).unwrap();
    drop(store);
    unblock_journal(&root, saved);
    let mut store = memory_owned(&history);
    assert!(store
        .committed_reference(&conversation("a"))
        .unwrap()
        .is_none());
    store
        .import_legacy_baseline(&history.store.borrow(), &projection)
        .unwrap();
    assert_eq!(fs::read(artifact).unwrap(), orphan);
    assert_eq!(
        store.committed_reference(&conversation("a")).unwrap(),
        Some(projection.reference().clone())
    );
}

fn tool_context_fixture() -> LegacyIngress {
    serde_json::from_str(include_str!(
        "fixtures/activation_state/ordinary_v1_tool_context_ingress.json"
    ))
    .unwrap()
}

fn ordinary_projection(
    content: &axocoatl_session::execution_content::ExecutionContentStore,
    seal: &axocoatl_session::execution_store::DurableLegacySeal,
    policy: axocoatl_memory::legacy_conversation::ToolReplayPolicy,
) -> axocoatl_memory::activation_state::LegacyBaselineProjection {
    axocoatl_memory::activation_state::LegacyBaselineProjection::from_ordinary_sealed_history(
        content,
        seal,
        SessionTeamSlotId::new("slot-a").unwrap(),
        conversation("a"),
        policy,
        &str::len,
    )
    .unwrap()
}

#[test]
fn ordinary_baseline_preserves_inline_context_native_groups_and_exact_provider_arguments() {
    use axocoatl_memory::legacy_conversation::{
        checkpoint_visible_matches, project_history, ToolReplayPolicy,
    };
    let root = tempfile::tempdir().unwrap();
    let mut legacy = axocoatl_session::turn_ledger::SessionTurnStore::open(root.path()).unwrap();
    let fixture = tool_context_fixture();
    append_legacy(&mut legacy, &fixture);
    let source = fs::read(legacy.path()).unwrap();
    let history = History::from_legacy(&legacy);
    let (content, seal) = seal_legacy(&history);
    let projection = ordinary_projection(&content, &seal, ToolReplayPolicy::CompleteNativeGroups);
    let details = projection.projection_details().unwrap();
    assert_eq!(details.source_tool_starts, 2);
    assert_eq!(details.retained_tool_calls, 2);
    assert!(details.omitted_turns.is_empty());
    assert!(!details.history_truncated);
    let mut memory = memory_owned(&history);
    let reference = memory
        .import_legacy_baseline(&history.store.borrow(), &projection)
        .unwrap();
    let checkpoint = memory
        .committed_checkpoint(&conversation("a"))
        .unwrap()
        .unwrap();
    let messages = &checkpoint.session_messages;
    assert_eq!(
        messages
            .iter()
            .map(|message| message.role.clone())
            .collect::<Vec<_>>(),
        vec![
            MessageRole::User,
            MessageRole::Assistant,
            MessageRole::Tool,
            MessageRole::Tool,
            MessageRole::Assistant
        ]
    );
    assert_eq!(
        messages[0].content,
        concat!(
            "## Context the user attached:\n\n### File: `src/main.rs` (lines 3-4)\n",
            "```rust\nfn main() {\n    existing();\n```\n\n",
            "### DOM element on http://localhost:8080/\nSelector: `button.save`\n",
            "```html\n<button class=\"save\">Save</button>\n```\n\n\n",
            "Explain these attached selections and inspect the source."
        )
    );
    assert_eq!(messages[1].content, "I will inspect both files.");
    assert_eq!(messages[1].tool_calls.len(), 2);
    for index in 0..2 {
        let call = &messages[1].tool_calls[index];
        assert_eq!(call.id, format!("call-{}", index + 1));
        assert_eq!(call.name, "read_file");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&call.arguments_json).unwrap(),
            serde_json::json!({"path":format!("src/{index}.rs")})
        );
        assert_eq!(
            call.provider_metadata["gemini.thought_signature"],
            "retained-signature"
        );
        assert_eq!(
            messages[index + 2].tool_call_id.as_deref(),
            Some(call.id.as_str())
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&messages[index + 2].content).unwrap(),
            serde_json::json!({"content":format!("file {} contents", index + 1)})
        );
    }
    assert_eq!(messages[4].content, fixture.agent_output.output);
    let retained = content.read_legacy_history(&seal).unwrap();
    assert_eq!(
        retained.turns[0].execution_events[1].event.metadata["arguments"]["path"],
        "/resolved/src/0.rs"
    );
    assert!(checkpoint_visible_matches(
        messages,
        &project_history(
            &str::len,
            &retained.turns,
            ToolReplayPolicy::CompleteNativeGroups
        )
    ));
    drop(memory);
    let mut memory = memory_owned(&history);
    let reopened = memory
        .committed_checkpoint(&conversation("a"))
        .unwrap()
        .unwrap();
    assert!(checkpoint_visible_matches(
        messages,
        &reopened.session_messages
    ));
    assert_eq!(
        memory
            .import_legacy_baseline(&history.store.borrow(), &projection)
            .unwrap(),
        reference
    );
    let alternate = ordinary_projection(&content, &seal, ToolReplayPolicy::OmitNativeGroups);
    assert!(memory
        .import_legacy_baseline(&history.store.borrow(), &alternate)
        .is_err());
    assert_eq!(fs::read(legacy.path()).unwrap(), source);
}

#[test]
fn ordinary_baseline_omits_whole_unreplayable_groups_and_records_selected_policy() {
    use axocoatl_memory::legacy_conversation::ToolReplayPolicy;
    for case in [
        "omit-policy",
        "missing-result",
        "truncated-result",
        "duplicate-index",
        "missing-provider-metadata",
    ] {
        let root = tempfile::tempdir().unwrap();
        let mut legacy =
            axocoatl_session::turn_ledger::SessionTurnStore::open(root.path()).unwrap();
        let mut fixture = tool_context_fixture();
        match case {
            "missing-result" => {
                fixture.tool_events.pop();
            }
            "truncated-result" => {
                fixture.tool_events[2]
                    .metadata
                    .insert("result_truncated".into(), true.into());
            }
            "duplicate-index" => {
                fixture.tool_events[1]
                    .metadata
                    .insert("provider_call_index".into(), 0.into());
            }
            "missing-provider-metadata" => {
                fixture.tool_events[0].metadata.remove("provider_metadata");
            }
            "omit-policy" => {}
            _ => unreachable!(),
        }
        append_legacy(&mut legacy, &fixture);
        let original = fs::read(legacy.path()).unwrap();
        let history = History::from_legacy(&legacy);
        let (content, seal) = seal_legacy(&history);
        let policy = if case == "omit-policy" {
            ToolReplayPolicy::OmitNativeGroups
        } else {
            ToolReplayPolicy::CompleteNativeGroups
        };
        let projection = ordinary_projection(&content, &seal, policy);
        let details = projection.projection_details().unwrap();
        assert_eq!(details.tool_replay_policy, policy);
        assert_eq!(details.source_tool_starts, 2, "{case}");
        assert_eq!(details.retained_tool_calls, 0, "{case}");
        assert!(details.omitted_turns.is_empty());
        let mut memory = memory_owned(&history);
        memory
            .import_legacy_baseline(&history.store.borrow(), &projection)
            .unwrap();
        drop(memory);
        let memory = memory_owned(&history);
        let checkpoint = memory
            .committed_checkpoint(&conversation("a"))
            .unwrap()
            .unwrap();
        assert_eq!(checkpoint.session_messages.len(), 2, "{case}");
        assert_eq!(
            checkpoint.session_messages[1].content,
            fixture.agent_output.output
        );
        assert!(checkpoint
            .session_messages
            .iter()
            .all(|message| message.tool_calls.is_empty() && message.tool_call_id.is_none()));
        assert_eq!(
            content.read_legacy_history(&seal).unwrap().turns[0]
                .execution_events
                .len(),
            fixture.tool_events.len() + 1
        );
        assert_eq!(fs::read(legacy.path()).unwrap(), original);
    }
}

fn append_terminal_legacy(
    legacy: &mut axocoatl_session::turn_ledger::SessionTurnStore,
    id: &str,
    status: axocoatl_session::turn_ledger::SessionTurnLifecycle,
) {
    let mut fixture = legacy_fixture().remove(0);
    fixture.begin.turn_id = Some(id.into());
    fixture.begin.idempotency_key = Some(id.into());
    fixture.begin.user_input = format!("request:{id}");
    fixture.execution.execution_id = Some(id.into());
    fixture.complete.status = status;
    fixture.complete.final_output = None;
    legacy.begin(fixture.begin).unwrap();
    legacy
        .record_execution(id, format!("run:{id}"), fixture.execution)
        .unwrap();
    legacy
        .append_output(id, format!("partial:{id}"), format!("partial:{id}"))
        .unwrap();
    legacy
        .transition(id, format!("finish:{id}"), fixture.complete)
        .unwrap();
}

#[test]
fn ordinary_baseline_preserves_failed_interrupted_context_and_all_recorded_incurred_usage() {
    use axocoatl_memory::activation_state::LegacyTurnOmission;
    use axocoatl_memory::legacy_conversation::ToolReplayPolicy;
    use axocoatl_session::turn_ledger::SessionTurnLifecycle as Status;
    let root = tempfile::tempdir().unwrap();
    let mut legacy = axocoatl_session::turn_ledger::SessionTurnStore::open(root.path()).unwrap();
    for (id, status) in [
        ("completed", Status::Completed),
        ("failed", Status::Failed),
        ("interrupted", Status::Interrupted),
        ("cancelled", Status::Cancelled),
        ("rewound", Status::Completed),
    ] {
        append_terminal_legacy(&mut legacy, id, status);
    }
    legacy
        .rewind("session-a", Some("cancelled"), "rewind")
        .unwrap();
    let history = History::from_legacy(&legacy);
    let (content, seal) = seal_legacy(&history);
    let projection = ordinary_projection(&content, &seal, ToolReplayPolicy::CompleteNativeGroups);
    assert_eq!(
        projection.projection_details().unwrap().omitted_turns,
        vec![
            LegacyTurnOmission::Cancelled {
                turn_id: "cancelled".into()
            },
            LegacyTurnOmission::Superseded {
                turn_id: "rewound".into()
            },
        ]
    );
    let mut memory = memory_owned(&history);
    memory
        .import_legacy_baseline(&history.store.borrow(), &projection)
        .unwrap();
    drop(memory);
    let checkpoint = memory_owned(&history)
        .committed_checkpoint(&conversation("a"))
        .unwrap()
        .unwrap();
    assert_eq!(
        checkpoint
            .session_messages
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>(),
        vec![
            "request:completed",
            "partial:completed",
            "request:failed",
            "partial:failed",
            "request:interrupted",
            "partial:interrupted"
        ]
    );
    assert_eq!(
        checkpoint.cumulative_token_usage,
        TokenUsageStats::new(50, 25).with_reasoning(10)
    );
    assert!(checkpoint.cumulative_token_usage_known);
    assert_eq!(content.read_legacy_history(&seal).unwrap().turns.len(), 5);
}

#[test]
fn ordinary_empty_conversation_is_explicitly_committed_only_when_supported_source_was_omitted() {
    use axocoatl_memory::activation_state::LegacyTurnOmission;
    use axocoatl_memory::legacy_conversation::{ToolReplayPolicy, SESSION_CHECKPOINT_MESSAGE_CAP};
    use axocoatl_session::turn_ledger::SessionTurnLifecycle;
    for bounded in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let mut legacy =
            axocoatl_session::turn_ledger::SessionTurnStore::open(root.path()).unwrap();
        if bounded {
            let mut fixture = legacy_fixture().remove(0);
            fixture.begin.user_input = "x".repeat(SESSION_CHECKPOINT_MESSAGE_CAP + 1);
            append_legacy(&mut legacy, &fixture);
        } else {
            append_terminal_legacy(&mut legacy, "cancelled", SessionTurnLifecycle::Cancelled);
        }
        let history = History::from_legacy(&legacy);
        let (content, seal) = seal_legacy(&history);
        let projection =
            ordinary_projection(&content, &seal, ToolReplayPolicy::CompleteNativeGroups);
        let details = projection.projection_details().unwrap();
        assert_eq!(details.history_truncated, bounded);
        assert_eq!(
            details.omitted_turns,
            vec![if bounded {
                LegacyTurnOmission::BoundedTail {
                    turn_id: "legacy-turn-1".into(),
                }
            } else {
                LegacyTurnOmission::Cancelled {
                    turn_id: "cancelled".into(),
                }
            }]
        );
        let mut memory = memory_owned(&history);
        let reference = memory
            .import_legacy_baseline(&history.store.borrow(), &projection)
            .unwrap();
        assert!(matches!(
            reference.source,
            CheckpointSource::Committed { .. }
        ));
        drop(memory);
        let memory = memory_owned(&history);
        assert_eq!(
            memory.committed_reference(&conversation("a")).unwrap(),
            Some(reference)
        );
        let checkpoint = memory
            .committed_checkpoint(&conversation("a"))
            .unwrap()
            .unwrap();
        assert!(checkpoint.session_messages.is_empty());
        assert_eq!(
            checkpoint.cumulative_token_usage,
            TokenUsageStats::new(10, 5).with_reasoning(2)
        );
        assert_eq!(content.read_legacy_history(&seal).unwrap().turns.len(), 1);
    }
}

#[test]
fn richer_ordinary_policy_refuses_coordinated_private_and_unresolved_blob_context_even_if_rewound()
{
    use axocoatl_memory::activation_state::LegacyBaselineProjection;
    use axocoatl_memory::legacy_conversation::ToolReplayPolicy;
    for case in [
        "coordinator",
        "private",
        "foreign-tool-agent",
        "unknown-tool-field",
        "upload",
        "mixed-agent",
        "attempt",
        "empty",
    ] {
        let root = tempfile::tempdir().unwrap();
        let mut legacy =
            axocoatl_session::turn_ledger::SessionTurnStore::open(root.path()).unwrap();
        let mut fixture = tool_context_fixture();
        match case {
            "coordinator" => {
                fixture
                    .begin
                    .metadata
                    .insert("mode".into(), "coordinated".into());
            }
            "private" => {
                fixture
                    .begin
                    .metadata
                    .insert("behavior_state".into(), "private".into());
            }
            "foreign-tool-agent" => {
                fixture.tool_events[0]
                    .metadata
                    .insert("agent_id".into(), "reviewer".into());
            }
            "unknown-tool-field" => {
                fixture.tool_events[0]
                    .metadata
                    .insert("private_role".into(), "coordinator".into());
            }
            "upload" => {
                fixture.begin.context[0].kind = "upload".into();
            }
            "mixed-agent" => {
                fixture.agent_output.agent_id = "reviewer".into();
            }
            "attempt" => {
                fixture.tool_events[0].attempt_id = Some("way-a".into());
            }
            "empty" => {}
            _ => unreachable!(),
        }
        if case != "empty" {
            append_legacy(&mut legacy, &fixture);
            legacy.rewind("session-a", None, "rewind-all").unwrap();
        }
        let original = fs::read(legacy.path()).unwrap();
        let history = History::from_legacy(&legacy);
        let (content, seal) = seal_legacy(&history);
        assert!(
            matches!(
                LegacyBaselineProjection::from_ordinary_sealed_history(
                    &content,
                    &seal,
                    SessionTeamSlotId::new("slot-a").unwrap(),
                    conversation("a"),
                    ToolReplayPolicy::CompleteNativeGroups,
                    &str::len
                ),
                Err(ActivationStateError::UnsupportedLegacy(_))
            ),
            "{case}"
        );
        assert_eq!(fs::read(legacy.path()).unwrap(), original);
    }
}

#[test]
fn owned_candidate_reservation_is_exact_idempotent_and_has_one_immutable_settlement() {
    let history = History::new();
    let mut memory = memory_owned(&history);
    let mut turn = Turn::new(&history, "reserved", &[("a", None), ("b", None)]);
    let a = turn.start(&mut memory, "a");
    let b = turn.start(&mut memory, "b");
    let payload = checkpoint("conversation-a", "accepted");
    assert!(memory
        .stage_candidate(&turn.snapshot(), &a, &payload)
        .is_err());
    assert!(memory
        .candidate_reservation(&history.store.borrow(), &a)
        .is_err());
    let reservation = memory
        .reserve_candidate(&history.store.borrow(), &a)
        .unwrap();
    assert_eq!(reservation.activation(), &a);
    assert_eq!(reservation.conversation_id(), &conversation("a"));
    assert_eq!(
        reservation.max_checkpoint_bytes(),
        axocoatl_memory::MAX_CHECKPOINT_BYTES
    );
    assert!(memory
        .starting_checkpoint_for(&reservation)
        .unwrap()
        .is_none());
    let repeated = memory
        .reserve_candidate(&history.store.borrow(), &a)
        .unwrap();
    let foreign = memory
        .reserve_candidate(&history.store.borrow(), &b)
        .unwrap();
    assert!(memory.stage_reserved_candidate(&foreign, &payload).is_err());
    let retained = memory
        .stage_reserved_candidate(&reservation, &payload)
        .unwrap();
    assert_eq!(
        memory
            .stage_reserved_candidate(&repeated, &payload)
            .unwrap(),
        retained
    );
    assert!(memory
        .stage_reserved_candidate(&reservation, &checkpoint("conversation-a", "replacement"))
        .is_err());
    assert_eq!(
        memory.checkpoint(&retained).unwrap().session_messages[0].content,
        payload.session_messages[0].content
    );
    // Immutable starting input still resolves Empty, never this newer candidate.
    assert!(memory
        .starting_checkpoint_for(&reservation)
        .unwrap()
        .is_none());
}

#[test]
fn reserved_late_checkpoint_survives_reopen_and_closed_turn_without_becoming_accepted() {
    let history = History::new();
    let mut memory = memory_owned(&history);
    let mut first = Turn::new(&history, "first", &[("a", None)]);
    let first_activation = first.start(&mut memory, "a");
    let first_reservation = memory
        .reserve_candidate(&history.store.borrow(), &first_activation)
        .unwrap();
    let baseline = checkpoint("conversation-a", "accepted");
    let first_candidate = memory
        .stage_reserved_candidate(&first_reservation, &baseline)
        .unwrap();
    first.accept(&first_activation, &first_candidate);
    first.close(TurnClosure::Completed);
    let committed = memory.promote(&first.snapshot()).unwrap().selected[0]
        .committed
        .clone();
    let mut late = Turn::new(&history, "late", &[("a", Some(committed.clone()))]);
    let activation = late.start(&mut memory, "a");
    let reservation = memory
        .reserve_candidate(&history.store.borrow(), &activation)
        .unwrap();
    late.apply(TurnContractEvent::InterruptEpoch {
        epoch_id: activation.execution_epoch_id.clone(),
    });
    late.close(TurnClosure::Cancelled);
    memory.promote(&late.snapshot()).unwrap();
    assert!(memory
        .reserve_candidate(&history.store.borrow(), &activation)
        .is_err());
    drop(memory);
    let mut memory = memory_owned(&history);
    let recovered = memory
        .candidate_reservation(&history.store.borrow(), &activation)
        .unwrap();
    let before_revision = late.snapshot().contract().revision();
    let mut diagnostic = checkpoint("conversation-a", "replacement");
    diagnostic.cumulative_token_usage = TokenUsageStats::new(500, 200);
    diagnostic.cumulative_token_usage_known = false;
    let retained = memory
        .stage_reserved_candidate(&recovered, &diagnostic)
        .unwrap();
    assert_eq!(
        memory
            .stage_reserved_candidate(&reservation, &diagnostic)
            .unwrap(),
        retained
    );
    assert_eq!(
        memory
            .starting_checkpoint_for(&recovered)
            .unwrap()
            .unwrap()
            .session_messages[0]
            .content,
        baseline.session_messages[0].content
    );
    assert_eq!(
        memory.committed_reference(&conversation("a")).unwrap(),
        Some(committed)
    );
    assert_eq!(
        memory
            .committed_checkpoint(&conversation("a"))
            .unwrap()
            .unwrap()
            .cumulative_token_usage,
        baseline.cumulative_token_usage
    );
    assert_eq!(
        memory.checkpoint(&retained).unwrap().cumulative_token_usage,
        diagnostic.cumulative_token_usage
    );
    assert!(
        !memory
            .checkpoint(&retained)
            .unwrap()
            .cumulative_token_usage_known
    );
    assert_eq!(late.snapshot().contract().revision(), before_revision);
    assert!(late
        .snapshot()
        .contract()
        .current_accepted_activations()
        .is_empty());
    drop(memory);
    let memory = memory_owned(&history);
    assert_eq!(
        memory.checkpoint(&retained).unwrap().cumulative_token_usage,
        diagnostic.cumulative_token_usage
    );
}

#[test]
fn reservations_reject_foreign_owned_journals_and_corrupt_or_missing_candidate_bytes() {
    for missing in [false, true] {
        let history = History::new();
        let foreign_history = History::new();
        let mut memory = memory_owned(&history);
        let mut foreign_memory = memory_owned(&foreign_history);
        let mut turn = Turn::new(&history, "reserved", &[("a", None)]);
        let activation = turn.start(&mut memory, "a");
        let mut foreign = Turn::new(&foreign_history, "reserved", &[("a", None)]);
        foreign.start(&mut foreign_memory, "a");
        let reservation = memory
            .reserve_candidate(&history.store.borrow(), &activation)
            .unwrap();
        assert!(memory
            .reserve_candidate(&foreign_history.store.borrow(), &activation)
            .is_err());
        assert!(foreign_memory
            .starting_checkpoint_for(&reservation)
            .is_err());
        assert!(foreign_memory
            .stage_reserved_candidate(&reservation, &checkpoint("conversation-a", "accepted"))
            .is_err());
        let payload = checkpoint("conversation-a", "accepted");
        let candidate = memory
            .stage_reserved_candidate(&reservation, &payload)
            .unwrap();
        let root = history
            .store
            .borrow()
            .path()
            .parent()
            .unwrap()
            .join("activation-state");
        let artifact = object_path(&root, &candidate);
        if missing {
            fs::remove_file(&artifact).unwrap();
        } else {
            fs::write(&artifact, b"corrupt").unwrap();
        }
        assert!(memory
            .stage_reserved_candidate(&reservation, &payload)
            .is_err());
        if missing {
            assert!(!artifact.exists());
        } else {
            assert_eq!(fs::read(&artifact).unwrap(), b"corrupt");
        }
    }
}

#[test]
fn owned_cancelled_turn_promotes_exact_accepted_parent_and_preserves_stopped_child_head_and_usage()
{
    let history = History::new();
    let mut memory = memory_owned(&history);
    let initial = accepted_turn(
        &history,
        &mut memory,
        "committed-before-stop",
        &[("a", None), ("b", None)],
    );
    let initial_promotion = memory.promote(&initial.snapshot()).unwrap();
    let parent_base = memory
        .committed_reference(&conversation("a"))
        .unwrap()
        .unwrap();
    let child_base = memory
        .committed_reference(&conversation("b"))
        .unwrap()
        .unwrap();
    let mut graph = initial.graph.clone();
    graph.snapshot_id = GraphSnapshotId::new("stopped-dependent-graph").unwrap();
    graph.nodes[0].starting_savepoint = ConversationSavepoint::Checkpoint {
        checkpoint: Box::new(parent_base.clone()),
    };
    graph.nodes[1].starting_savepoint = ConversationSavepoint::Checkpoint {
        checkpoint: Box::new(child_base.clone()),
    };
    graph.dependencies.push(DependencyEdge {
        parent: TurnNodeId::new("a").unwrap(),
        child: TurnNodeId::new("b").unwrap(),
    });
    let mut stopped = Turn::for_graph(&history, "stopped-after-parent-acceptance", graph);
    let parent = stopped.start(&mut memory, "a");
    let accepted_parent = stopped.candidate(&mut memory, &parent, "replacement");
    stopped.accept(&parent, &accepted_parent);
    let accepted_output = stopped.snapshot().contract().current_accepted_activations()[0]
        .output
        .clone()
        .unwrap();
    let mut child_input = stopped.input("b", 1);
    child_input.parents = vec![AcceptedParentInput {
        activation: parent.clone(),
        checkpoint: accepted_parent.clone(),
        output: accepted_output,
    }];
    let child = child_input.activation.clone();
    stopped.apply(TurnContractEvent::StartActivation {
        input: Box::new(child_input),
    });
    memory.record_input(&stopped.snapshot(), &child).unwrap();
    let reservation = memory
        .reserve_candidate(&history.store.borrow(), &child)
        .unwrap();
    let mut partial = checkpoint("conversation-b", "failed");
    partial.cumulative_token_usage = TokenUsageStats::new(800, 100);
    partial.cumulative_token_usage_known = false;
    let stopped_candidate = memory
        .stage_reserved_candidate(&reservation, &partial)
        .unwrap();
    stopped.apply(TurnContractEvent::RequestTurnStop {
        evidence: evidence("authenticated-whole-turn-stop"),
    });
    stopped.fail(&child);
    memory
        .prepare_close(&stopped.snapshot(), TurnClosure::Cancelled)
        .unwrap();
    stopped.close(TurnClosure::Cancelled);
    let before = stopped.snapshot();
    let promoted = memory.promote(&before).unwrap();
    assert_eq!(promoted.closure.closure(), TurnClosure::Cancelled);
    assert_eq!(promoted.selected.len(), 1);
    assert_eq!(promoted.selected[0].accepted, accepted_parent);
    assert_eq!(promoted.selected[0].previous_committed, Some(parent_base));
    assert_eq!(promoted.selected[0].node_id, parent.node_id);
    assert_eq!(
        memory.committed_reference(&conversation("a")).unwrap(),
        Some(promoted.selected[0].committed.clone())
    );
    assert_eq!(
        memory.committed_reference(&conversation("b")).unwrap(),
        Some(child_base.clone())
    );
    assert_eq!(
        memory
            .checkpoint(&stopped_candidate)
            .unwrap()
            .cumulative_token_usage,
        partial.cumulative_token_usage
    );
    assert!(
        !memory
            .checkpoint(&stopped_candidate)
            .unwrap()
            .cumulative_token_usage_known
    );
    assert_eq!(
        before.contract().current_accepted_activations()[0].checkpoint,
        Some(accepted_parent.clone())
    );
    drop(memory);
    let mut reopened = memory_owned(&history);
    assert_eq!(
        reopened.promotion(&stopped.snapshot()).unwrap(),
        Some(promoted.clone())
    );
    assert_eq!(reopened.promote(&stopped.snapshot()).unwrap(), promoted);
    assert_eq!(
        reopened.committed_reference(&conversation("b")).unwrap(),
        Some(child_base)
    );
    assert_eq!(
        reopened
            .checkpoint(&stopped_candidate)
            .unwrap()
            .cumulative_token_usage,
        partial.cumulative_token_usage
    );
    // Historical promotion replay cannot move the already advanced parent head.
    assert_eq!(
        reopened.promote(&initial.snapshot()).unwrap(),
        initial_promotion
    );
    assert_eq!(
        reopened.committed_reference(&conversation("a")).unwrap(),
        Some(promoted.selected[0].committed.clone())
    );
    assert_eq!(
        stopped.snapshot().contract().revision(),
        before.contract().revision()
    );
}

#[test]
fn native_rewind_keeps_raw_promotions_and_restarts_from_exact_selected_checkpoint() {
    use axocoatl_memory::legacy_conversation::ToolReplayPolicy;
    use axocoatl_session::session_history::{HistoryVisibility, SessionHistory};
    let legacy_root = tempfile::tempdir().unwrap();
    let legacy = axocoatl_session::turn_ledger::SessionTurnStore::open(legacy_root.path()).unwrap();
    let history = History::from_legacy(&legacy);
    let (content, _) = seal_legacy(&history);
    let mut store = memory_owned(&history);
    let first = accepted_turn(&history, &mut store, "rewind-first", &[("a", None)]);
    let first_promotion = store.promote(&first.snapshot()).unwrap();
    let original = store
        .committed_reference(&conversation("a"))
        .unwrap()
        .unwrap();
    let second = accepted_turn(
        &history,
        &mut store,
        "rewind-second",
        &[("a", Some(original.clone()))],
    );
    let second_promotion = store.promote(&second.snapshot()).unwrap();
    let revision = history.store.borrow().records().unwrap().len();
    let rewind = store
        .rewind_session(
            &history.store.borrow(),
            &content,
            Some("rewind-first"),
            &[conversation("a")],
            ToolReplayPolicy::CompleteNativeGroups,
        )
        .unwrap();
    assert_eq!(rewind.superseded_turn_ids, vec!["rewind-second"]);
    assert_eq!(
        store.committed_reference(&conversation("a")).unwrap(),
        Some(original.clone())
    );
    assert_eq!(
        store
            .committed_activation(&conversation("a"))
            .unwrap()
            .unwrap()
            .turn_id,
        first.id
    );
    assert_eq!(
        store.promotion(&second.snapshot()).unwrap(),
        Some(second_promotion)
    );
    assert_eq!(
        store.promotion(&first.snapshot()).unwrap(),
        Some(first_promotion)
    );
    assert_eq!(history.store.borrow().records().unwrap().len(), revision);
    assert_eq!(
        store
            .rewind_session(
                &history.store.borrow(),
                &content,
                Some("rewind-first"),
                &[conversation("a")],
                ToolReplayPolicy::CompleteNativeGroups
            )
            .unwrap(),
        rewind
    );
    drop(store);
    let mut store = memory_owned(&history);
    assert_eq!(
        store.committed_reference(&conversation("a")).unwrap(),
        Some(original.clone())
    );
    let mut view = SessionHistory::from_upgraded(&history.store.borrow(), &content).unwrap();
    view.apply_superseded(&store.superseded_turn_ids().unwrap())
        .unwrap();
    assert_eq!(view.entries(HistoryVisibility::Visible).len(), 1);
    assert!(!view.get("rewind-second").unwrap().is_visible());
    let third = accepted_turn(
        &history,
        &mut store,
        "rewind-third",
        &[("a", Some(original.clone()))],
    );
    let third_promotion = store.promote(&third.snapshot()).unwrap();
    assert_eq!(
        third_promotion.selected[0].previous_committed,
        Some(original)
    );
    assert_eq!(
        store
            .committed_activation(&conversation("a"))
            .unwrap()
            .unwrap()
            .turn_id,
        third.id
    );
    store
        .rewind_session(
            &history.store.borrow(),
            &content,
            None,
            &[conversation("a")],
            ToolReplayPolicy::CompleteNativeGroups,
        )
        .unwrap();
    assert!(store
        .committed_reference(&conversation("a"))
        .unwrap()
        .is_none());
    assert!(store
        .committed_activation(&conversation("a"))
        .unwrap()
        .is_none());
    assert!(store
        .checkpoint(&third_promotion.selected[0].committed)
        .is_ok());
    drop(store);
    assert!(memory_owned(&history)
        .committed_checkpoint(&conversation("a"))
        .unwrap()
        .is_none());
}

#[test]
fn migrated_rewind_projects_only_kept_prefix_without_rewriting_sealed_history_or_usage() {
    use axocoatl_memory::legacy_conversation::ToolReplayPolicy;
    let legacy_root = tempfile::tempdir().unwrap();
    let mut legacy =
        axocoatl_session::turn_ledger::SessionTurnStore::open(legacy_root.path()).unwrap();
    let fixtures = legacy_fixture();
    for fixture in &fixtures {
        append_legacy(&mut legacy, fixture);
    }
    let history = History::from_legacy(&legacy);
    let (content, seal) = seal_legacy(&history);
    let projection = baseline_projection(&content, &seal);
    let mut store = memory_owned(&history);
    store
        .import_legacy_baseline(&history.store.borrow(), &projection)
        .unwrap();
    let before = store
        .committed_checkpoint(&conversation("a"))
        .unwrap()
        .unwrap();
    store
        .rewind_session(
            &history.store.borrow(),
            &content,
            Some("legacy-turn-1"),
            &[conversation("a")],
            ToolReplayPolicy::CompleteNativeGroups,
        )
        .unwrap();
    let selected = store
        .committed_checkpoint(&conversation("a"))
        .unwrap()
        .unwrap();
    assert_eq!(selected.session_messages.len(), 2);
    assert_eq!(
        selected.session_messages[1].content,
        fixtures[0].agent_output.output
    );
    assert_eq!(
        selected.cumulative_token_usage,
        before.cumulative_token_usage
    );
    assert_eq!(content.read_legacy_history(&seal).unwrap().turns.len(), 2);
    drop(store);
    let store = memory_owned(&history);
    assert_eq!(
        store
            .committed_checkpoint(&conversation("a"))
            .unwrap()
            .unwrap()
            .session_messages
            .len(),
        2
    );
    assert_eq!(
        store
            .legacy_baseline_checkpoint(&conversation("a"))
            .unwrap()
            .unwrap()
            .session_messages
            .len(),
        4
    );
}

#[test]
fn successor_reset_keeps_previous_slot_conversation_artifacts_and_new_head_after_restart() {
    let legacy_root = tempfile::tempdir().unwrap();
    let legacy = axocoatl_session::turn_ledger::SessionTurnStore::open(legacy_root.path()).unwrap();
    let history = History::from_legacy(&legacy);
    let (_content, _) = seal_legacy(&history);
    let mut store = memory_owned(&history);
    let first = accepted_turn(&history, &mut store, "before-team-reset", &[("a", None)]);
    let first_promotion = store.promote(&first.snapshot()).unwrap();
    let original = store
        .committed_reference(&conversation("a"))
        .unwrap()
        .unwrap();
    let reset_conversation = NodeConversationId::new("explicit-reset-conversation").unwrap();
    let mut graph = first.graph.clone();
    graph.snapshot_id = GraphSnapshotId::new("reset-graph").unwrap();
    graph.nodes[0].conversation_id = reset_conversation.clone();
    graph.nodes[0].starting_savepoint = ConversationSavepoint::Empty;
    store.validate_starting_savepoints(&graph).unwrap();
    let mut reset = Turn::for_graph(&history, "after-team-reset", graph);
    let activation = reset.start(&mut store, "a");
    assert!(store.starting_checkpoint(&activation).unwrap().is_none());
    let checkpoint = reset.candidate(&mut store, &activation, "replacement");
    reset.accept(&activation, &checkpoint);
    reset.close(TurnClosure::Completed);
    let promoted = store.promote(&reset.snapshot()).unwrap();
    assert_eq!(promoted.selected[0].previous_committed, None);
    assert_eq!(
        store.committed_reference(&conversation("a")).unwrap(),
        Some(original.clone())
    );
    assert_ne!(
        store.committed_reference(&reset_conversation).unwrap(),
        Some(original.clone())
    );
    assert_eq!(
        store.promotion(&first.snapshot()).unwrap(),
        Some(first_promotion)
    );
    drop(store);
    let store = memory_owned(&history);
    assert_eq!(
        store.committed_reference(&conversation("a")).unwrap(),
        Some(original)
    );
    assert_eq!(
        store.committed_activation(&reset_conversation).unwrap(),
        Some(activation)
    );
    let mut stolen = reset.graph.clone();
    stolen.nodes[0].slot_id = SessionTeamSlotId::new("different-slot").unwrap();
    stolen.nodes[0].starting_savepoint = ConversationSavepoint::Checkpoint {
        checkpoint: Box::new(
            store
                .committed_reference(&reset_conversation)
                .unwrap()
                .unwrap(),
        ),
    };
    assert!(store.validate_starting_savepoints(&stolen).is_err());
}
