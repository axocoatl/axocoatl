use super::*;
use crate::checkpoint::{CheckpointPolicy, CheckpointStore};
use axocoatl_session::execution_ownership::LegacyFormatOwnership;
use axocoatl_session::execution_store::ExecutionStoreOwner;
use axocoatl_session::turn_ledger::{BeginSessionTurn, SessionTurnStore, TransitionSessionTurn};

struct Fixture {
    root: tempfile::TempDir,
    canonical: SessionExecutionStore,
    content: ExecutionContentStore,
    seal: DurableLegacySeal,
    source: CheckpointStore,
}

async fn fixture() -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let secure = SecureDir::open(root.path()).unwrap();
    let mut old = SessionTurnStore::open(secure.child("session-history").unwrap().path()).unwrap();
    for (id, status) in [
        ("completed", SessionTurnLifecycle::Completed),
        ("failed", SessionTurnLifecycle::Failed),
    ] {
        old.begin(BeginSessionTurn {
            turn_id: Some(id.to_owned()),
            session_id: "session".to_owned(),
            user_input: format!("request {id}"),
            agent_id: None,
            model: None,
            context: vec![],
            idempotency_key: None,
            metadata: serde_json::Map::new(),
        })
        .unwrap();
        for agent in ["coder", "reviewer"] {
            old.record_agent_output(
                id,
                format!("{id}:{agent}"),
                agent,
                None,
                format!("{agent} {id} own output"),
                None,
            )
            .unwrap();
        }
        old.transition(
            id,
            format!("terminal:{id}"),
            TransitionSessionTurn {
                status,
                final_output: Some(
                    "aggregate team output must never become each actor's transcript".to_owned(),
                ),
                error: None,
                metadata: serde_json::Map::new(),
            },
        )
        .unwrap();
    }
    drop(old);
    let source =
        CheckpointStore::new_in_secure(&secure, "checkpoints", CheckpointPolicy::Manual).unwrap();
    for (agent, known) in [("coder", true), ("reviewer", false)] {
        source
            .save(&AgentCheckpoint {
                version: 3,
                agent_id: format!("session:{agent}"),
                checkpoint_time: 12,
                session_messages: vec![StoredMessage {
                    content_parts: None,
                    role: MessageRole::Assistant,
                    content: format!("{agent} unaccepted private checkpoint text"),
                    timestamp: 10,
                    token_count: 6,
                    name: None,
                    tool_calls: vec![],
                    tool_call_id: None,
                }],
                cumulative_token_usage: TokenUsageStats::new(1000, 500),
                cumulative_token_usage_known: known,
                behavior_state: Some(format!(
                    "{{\"goal\":\"unfinished {agent}\",\"completed\":false}}"
                )),
            })
            .await
            .unwrap();
    }
    let ownership = Arc::new(
        LegacyFormatOwnership::acquire(root.path())
            .unwrap()
            .upgrade()
            .unwrap(),
    );
    let mut canonical = SessionExecutionStore::open(
        ownership,
        ExecutionStoreOwner {
            workspace_id: "workspace".to_owned(),
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
    let snapshot = canonical.legacy_history_snapshot().unwrap();
    let retained = content.retain_legacy_history(&snapshot).unwrap();
    let seal = canonical.seal_legacy_history(&retained).unwrap();
    Fixture {
        root,
        canonical,
        content,
        seal,
        source,
    }
}

fn assignments() -> Vec<LegacyRoleAssignment> {
    ["coder", "reviewer"]
        .into_iter()
        .map(|agent| LegacyRoleAssignment {
            checkpoint_agent_id: format!("session:{agent}"),
            recorded_agent_id: agent.to_owned(),
            slot_id: SessionTeamSlotId::new(format!("slot-{agent}")).unwrap(),
            conversation_id: NodeConversationId::new(format!("conversation-{agent}")).unwrap(),
            policy: LegacyActorProjectionPolicy::CompletedPerAgent,
            tool_replay_policy: ToolReplayPolicy::CompleteNativeGroups,
        })
        .collect()
}

fn project(fixture: &Fixture) -> Vec<LegacyBaselineProjection> {
    let captured = fixture
        .source
        .capture_legacy_session_checkpoints("session")
        .unwrap();
    LegacyBaselineProjection::from_captured_session(
        &fixture.content,
        &fixture.seal,
        &fixture.source,
        captured,
        &assignments(),
        &str::len,
    )
    .unwrap()
}

fn memory(fixture: &Fixture) -> ActivationStateStore {
    ActivationStateStore::open_owned(
        fixture
            .canonical
            .component_namespace(ExecutionComponent::ActivationState)
            .unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn captured_roles_import_real_owned_baselines_and_preserve_source_across_reopen() {
    let fixture = fixture().await;
    let original_history =
        std::fs::read(fixture.root.path().join("session-history/turns.v1.jsonl")).unwrap();
    let projections = project(&fixture);
    let mut state = memory(&fixture);
    for (index, projection) in projections.iter().enumerate() {
        let reference = state
            .import_legacy_baseline(&fixture.canonical, projection)
            .unwrap();
        assert_eq!(
            state
                .import_legacy_baseline(&fixture.canonical, projection)
                .unwrap(),
            reference
        );
        let checkpoint = state.checkpoint(&reference).unwrap();
        let agent = if index == 0 { "coder" } else { "reviewer" };
        assert_eq!(checkpoint.session_messages.len(), 2);
        assert_eq!(checkpoint.session_messages[0].content, "request completed");
        assert_eq!(
            checkpoint.session_messages[1].content,
            format!("{agent} completed own output")
        );
        assert!(checkpoint.behavior_state.is_none());
        assert_eq!(
            checkpoint.cumulative_token_usage,
            TokenUsageStats::new(1000, 500)
        );
        assert_eq!(checkpoint.cumulative_token_usage_known, index == 0);
        let details = projection.checkpoint_projection_details().unwrap();
        assert!(details.private_state_reset);
        assert!(details.private_state_sha256.is_some());
        assert_eq!(details.omitted_turn_ids, vec!["failed"]);
        let archive = state
            .legacy_checkpoint_source_archive(&reference.conversation_id)
            .unwrap()
            .unwrap();
        assert_eq!(digest_bytes(&archive), details.archive_sha256);
        assert!(archive
            .windows(b"unfinished coder".len())
            .any(|part| part == b"unfinished coder"));
    }
    assert_eq!(state.projection.baselines.len(), 2);
    drop(state);
    let reopened = memory(&fixture);
    for assignment in assignments() {
        let checkpoint = reopened
            .legacy_baseline_checkpoint(&assignment.conversation_id)
            .unwrap()
            .unwrap();
        assert_eq!(checkpoint.cumulative_token_usage.input_tokens, 1000);
        assert!(reopened
            .legacy_checkpoint_source_archive(&assignment.conversation_id)
            .unwrap()
            .is_some());
    }
    assert_eq!(
        std::fs::read(fixture.root.path().join("session-history/turns.v1.jsonl")).unwrap(),
        original_history
    );
}

#[tokio::test]
async fn same_session_string_cannot_join_a_foreign_data_root() {
    let first = fixture().await;
    let second = fixture().await;
    let captured = second
        .source
        .capture_legacy_session_checkpoints("session")
        .unwrap();
    assert!(LegacyBaselineProjection::from_captured_session(
        &first.content,
        &first.seal,
        &second.source,
        captured,
        &assignments(),
        &str::len
    )
    .is_err());
}

#[tokio::test]
async fn missing_duplicate_or_fabricated_role_mapping_refuses_to_drop_or_duplicate_usage() {
    let fixture = fixture().await;
    for case in ["missing", "duplicate", "wrong-source", "wrong-recorded"] {
        let mut roles = assignments();
        match case {
            "missing" => {
                roles.pop();
            }
            "duplicate" => {
                roles.push(roles[0].clone());
            }
            "wrong-source" => roles[0].checkpoint_agent_id = "session:foreign".to_owned(),
            "wrong-recorded" => roles[0].recorded_agent_id = "reviewer".to_owned(),
            _ => unreachable!(),
        }
        let captured = fixture
            .source
            .capture_legacy_session_checkpoints("session")
            .unwrap();
        assert!(
            LegacyBaselineProjection::from_captured_session(
                &fixture.content,
                &fixture.seal,
                &fixture.source,
                captured,
                &roles,
                &str::len
            )
            .is_err(),
            "{case}"
        );
    }
}

#[tokio::test]
async fn source_change_after_projection_is_rejected_before_baseline_publication() {
    let fixture = fixture().await;
    let projections = project(&fixture);
    let mut changed = fixture
        .source
        .load_latest(&axocoatl_core::AgentId::new("session:coder"))
        .await
        .unwrap()
        .unwrap();
    changed.version += 1;
    changed.cumulative_token_usage.output_tokens += 77;
    fixture.source.save(&changed).await.unwrap();
    let mut state = memory(&fixture);
    assert!(state
        .import_legacy_baseline(&fixture.canonical, &projections[0])
        .is_err());
    assert!(state.projection.baselines.is_empty());
}

#[tokio::test]
async fn missing_retained_private_source_archive_prevents_reopening_a_baseline() {
    let fixture = fixture().await;
    let projections = project(&fixture);
    let mut state = memory(&fixture);
    state
        .import_legacy_baseline(&fixture.canonical, &projections[0])
        .unwrap();
    let details = projections[0].checkpoint_projection_details().unwrap();
    let archive_path = fixture
        .canonical
        .path()
        .parent()
        .unwrap()
        .join("activation-state/objects")
        .join(archive_name(&details.archive_sha256));
    std::fs::remove_file(archive_path).unwrap();
    drop(state);
    assert!(ActivationStateStore::open_owned(
        fixture
            .canonical
            .component_namespace(ExecutionComponent::ActivationState)
            .unwrap()
    )
    .is_err());
}

async fn ordinary_fixture(metadata: serde_json::Map<String, serde_json::Value>) -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let secure = SecureDir::open(root.path()).unwrap();
    let mut old = SessionTurnStore::open_in_secure(&secure, "session-history").unwrap();
    old.begin(BeginSessionTurn {
        turn_id: Some("imported".to_owned()),
        session_id: "session".to_owned(),
        user_input: "Retain this historical request".to_owned(),
        agent_id: Some("coder".to_owned()),
        model: None,
        context: vec![],
        idempotency_key: None,
        metadata,
    })
    .unwrap();
    old.transition(
        "imported",
        "complete-import".to_owned(),
        TransitionSessionTurn {
            status: SessionTurnLifecycle::Completed,
            final_output: Some("Retain this historical answer".to_owned()),
            error: None,
            metadata: serde_json::Map::new(),
        },
    )
    .unwrap();
    drop(old);
    let source =
        CheckpointStore::new_in_secure(&secure, "checkpoints", CheckpointPolicy::Manual).unwrap();
    source
        .save(&AgentCheckpoint {
            version: 14,
            agent_id: "session:coder".to_owned(),
            checkpoint_time: 12,
            session_messages: vec![],
            cumulative_token_usage: TokenUsageStats::new(1000, 500),
            cumulative_token_usage_known: false,
            behavior_state: None,
        })
        .await
        .unwrap();
    let ownership = Arc::new(
        LegacyFormatOwnership::acquire(root.path())
            .unwrap()
            .upgrade()
            .unwrap(),
    );
    let mut canonical = SessionExecutionStore::open(
        ownership,
        ExecutionStoreOwner {
            workspace_id: "workspace".to_owned(),
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
    let captured = canonical.legacy_history_snapshot().unwrap();
    let retained = content.retain_legacy_history(&captured).unwrap();
    let seal = canonical.seal_legacy_history(&retained).unwrap();
    Fixture {
        root,
        canonical,
        content,
        seal,
        source,
    }
}

#[tokio::test]
async fn canonical_checkpoint_import_provenance_preserves_context_and_usage_across_reopen() {
    for encoding in [
        "bincode_v0.1.0",
        "bincode_v0.1.1-v0.1.4",
        "postcard_unframed_launch_candidate",
    ] {
        let fixture = ordinary_fixture(serde_json::json!({
            "source": "actor_checkpoint", "checkpoint_version": 13, "checkpoint_encoding": encoding
        }).as_object().unwrap().clone()).await;
        let mut role = assignments().remove(0);
        role.policy = LegacyActorProjectionPolicy::OrdinaryAutonomous;
        let projections = LegacyBaselineProjection::from_captured_session(
            &fixture.content,
            &fixture.seal,
            &fixture.source,
            fixture
                .source
                .capture_legacy_session_checkpoints("session")
                .unwrap(),
            &[role.clone()],
            &str::len,
        )
        .unwrap();
        let mut state = memory(&fixture);
        let reference = state
            .import_legacy_baseline(&fixture.canonical, &projections[0])
            .unwrap();
        drop(state);
        let reopened = memory(&fixture);
        let checkpoint = reopened.checkpoint(&reference).unwrap();
        assert_eq!(
            checkpoint
                .session_messages
                .iter()
                .map(|message| message.content.as_str())
                .collect::<Vec<_>>(),
            [
                "Retain this historical request",
                "Retain this historical answer",
            ],
            "{encoding}"
        );
        assert_eq!(
            checkpoint.cumulative_token_usage,
            TokenUsageStats::new(1000, 500)
        );
        assert!(!checkpoint.cumulative_token_usage_known);
    }
}

#[tokio::test]
async fn incomplete_or_unknown_checkpoint_import_provenance_refuses_publication() {
    for metadata in [
        serde_json::json!({"source": "actor_checkpoint"}),
        serde_json::json!({"source": "other", "checkpoint_version": 13, "checkpoint_encoding": "bincode_v0.1.0"}),
        serde_json::json!({"source": "actor_checkpoint", "checkpoint_version": "13", "checkpoint_encoding": "bincode_v0.1.0"}),
        serde_json::json!({"source": "actor_checkpoint", "checkpoint_version": 13, "checkpoint_encoding": "unknown"}),
    ] {
        let fixture = ordinary_fixture(metadata.as_object().unwrap().clone()).await;
        let mut role = assignments().remove(0);
        role.policy = LegacyActorProjectionPolicy::OrdinaryAutonomous;
        assert!(
            LegacyBaselineProjection::from_captured_session(
                &fixture.content,
                &fixture.seal,
                &fixture.source,
                fixture
                    .source
                    .capture_legacy_session_checkpoints("session")
                    .unwrap(),
                &[role],
                &str::len,
            )
            .is_err(),
            "{metadata}"
        );
        assert!(memory(&fixture).projection.baselines.is_empty());
    }
}

#[tokio::test]
async fn ordinary_checkpoint_only_context_cannot_silently_become_accounting_only() {
    let fixture = fixture().await;
    let mut roles = assignments();
    roles[0].policy = LegacyActorProjectionPolicy::OrdinaryAutonomous;
    let captured = fixture
        .source
        .capture_legacy_session_checkpoints("session")
        .unwrap();
    assert!(LegacyBaselineProjection::from_captured_session(
        &fixture.content,
        &fixture.seal,
        &fixture.source,
        captured,
        &roles,
        &str::len,
    )
    .is_err());
    assert!(memory(&fixture).projection.baselines.is_empty());
    let mut empty = fixture
        .source
        .load_latest(&axocoatl_core::AgentId::new("session:coder"))
        .await
        .unwrap()
        .unwrap();
    empty.version += 1;
    empty.session_messages.clear();
    fixture.source.save(&empty).await.unwrap();
    let projections = LegacyBaselineProjection::from_captured_session(
        &fixture.content,
        &fixture.seal,
        &fixture.source,
        fixture
            .source
            .capture_legacy_session_checkpoints("session")
            .unwrap(),
        &roles,
        &str::len,
    )
    .unwrap();
    let mut state = memory(&fixture);
    let reference = state
        .import_legacy_baseline(&fixture.canonical, &projections[0])
        .unwrap();
    let checkpoint = state.checkpoint(&reference).unwrap();
    assert!(checkpoint.session_messages.is_empty());
    assert_eq!(
        checkpoint.cumulative_token_usage,
        TokenUsageStats::new(1000, 500)
    );
}

#[tokio::test]
async fn captured_archive_spans_bounded_owned_objects_and_requires_every_chunk_on_reopen() {
    let fixture = fixture().await;
    let mut changed = fixture
        .source
        .load_latest(&axocoatl_core::AgentId::new("session:coder"))
        .await
        .unwrap()
        .unwrap();
    changed.version += 1;
    changed.behavior_state = Some("x".repeat(SOURCE_ARCHIVE_CHUNK_BYTES));
    fixture.source.save(&changed).await.unwrap();
    drop(changed);
    let projections = project(&fixture);
    let details = projections[0].checkpoint_projection_details().unwrap();
    assert!(details.archive_bytes > SOURCE_ARCHIVE_CHUNK_BYTES);
    let mut state = memory(&fixture);
    let reference = state
        .import_legacy_baseline(&fixture.canonical, &projections[0])
        .unwrap();
    let expected_hash = details.archive_sha256.clone();
    let expected_bytes = details.archive_bytes;
    drop(projections);
    drop(state);
    let reopened = memory(&fixture);
    let archive = reopened
        .legacy_checkpoint_source_archive(&reference.conversation_id)
        .unwrap()
        .unwrap();
    assert_eq!(archive.len(), expected_bytes);
    assert_eq!(digest_bytes(&archive), expected_hash);
    drop(archive);
    drop(reopened);
    let objects = fixture
        .canonical
        .path()
        .parent()
        .unwrap()
        .join("activation-state/objects");
    assert_eq!(
        std::fs::metadata(objects.join(archive_name(&expected_hash)))
            .unwrap()
            .len(),
        SOURCE_ARCHIVE_CHUNK_BYTES as u64
    );
    std::fs::remove_file(objects.join(archive_chunk_name(&expected_hash, 1))).unwrap();
    assert!(ActivationStateStore::open_owned(
        fixture
            .canonical
            .component_namespace(ExecutionComponent::ActivationState)
            .unwrap()
    )
    .is_err());
}

#[tokio::test]
async fn worker_checkpoint_only_conversation_refuses_instead_of_publishing_accounting_only() {
    let fixture = fixture().await;
    let mut worker = fixture
        .source
        .load_latest(&axocoatl_core::AgentId::new("session:coder"))
        .await
        .unwrap()
        .unwrap();
    worker.agent_id = "session:lead:worker:worker".to_owned();
    fixture.source.save(&worker).await.unwrap();
    let mut roles = assignments();
    let mut role = roles[0].clone();
    role.checkpoint_agent_id = worker.agent_id;
    role.recorded_agent_id = "worker".to_owned();
    role.slot_id = SessionTeamSlotId::new("worker-slot").unwrap();
    role.conversation_id = NodeConversationId::new("worker-conversation").unwrap();
    roles.push(role);
    assert!(LegacyBaselineProjection::from_captured_session(
        &fixture.content,
        &fixture.seal,
        &fixture.source,
        fixture
            .source
            .capture_legacy_session_checkpoints("session")
            .unwrap(),
        &roles,
        &str::len
    )
    .is_err());
    assert!(memory(&fixture).projection.baselines.is_empty());
}
