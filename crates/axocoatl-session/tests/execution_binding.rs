#![cfg(unix)]

use std::{fs, sync::Arc};

use axocoatl_session::{
    control_authority::ControlAuthority,
    control_command::{ControlCommandOwner, ControlCommandStore},
    execution_content::{ContentResolution, ExecutionContentStore, ExecutionRequestContent},
    execution_namespace::ExecutionComponent,
    execution_ownership::{LegacyFormatOwnership, UpgradedFormatOwnership},
    execution_store::{ExecutionStoreOwner, SessionExecutionStore},
    invocation_audit::{InvocationAudit, InvocationAuditOwner},
    turn_contract::*,
    BeginSessionTurn, SessionTurnLifecycle, SessionTurnStore, TransitionSessionTurn,
};

fn owner() -> ExecutionStoreOwner {
    ExecutionStoreOwner {
        workspace_id: "workspace".into(),
        session_id: SessionId::new("session").unwrap(),
    }
}

fn guard(root: &tempfile::TempDir) -> Arc<UpgradedFormatOwnership> {
    Arc::new(
        LegacyFormatOwnership::acquire(root.path())
            .unwrap()
            .upgrade()
            .unwrap(),
    )
}

fn begin(turn: &str) -> TurnContractEnvelope {
    TurnContractEnvelope {
        schema_version: TURN_CONTRACT_SCHEMA_VERSION,
        command_id: CommandId::new(format!("begin-{turn}")).unwrap(),
        expected_revision: 0,
        session_id: owner().session_id,
        turn_id: LogicalTurnId::new(turn).unwrap(),
        event: TurnContractEvent::Begin {
            epoch_id: ExecutionEpochId::new("epoch").unwrap(),
            graph: TurnGraphSnapshot {
                snapshot_id: GraphSnapshotId::new("graph").unwrap(),
                revision: 1,
                nodes: vec![GraphNode {
                    node_id: TurnNodeId::new("node").unwrap(),
                    slot_id: SessionTeamSlotId::new("slot").unwrap(),
                    definition: DefinitionSnapshotRef {
                        definition_id: AgentDefinitionId::new("agent").unwrap(),
                        snapshot: EvidenceRef::new("definition").unwrap(),
                    },
                    conversation_id: NodeConversationId::new("conversation").unwrap(),
                    starting_savepoint: ConversationSavepoint::Empty,
                    required: true,
                }],
                dependencies: vec![],
                conditions: vec![],
            },
            predecessor: None,
        },
    }
}

fn content(turn: &str, text: &str) -> ExecutionRequestContent {
    ExecutionRequestContent {
        turn_id: LogicalTurnId::new(turn).unwrap(),
        recorded_at_unix_ms: 1_800_000_000_000,
        display_input: text.into(),
        effective_input: format!("Exact context\n{text}"),
        context: vec![],
        target_definition: Some(AgentDefinitionId::new("agent").unwrap()),
        model: None,
    }
}

#[test]
fn request_and_begin_are_one_canonical_binding_across_recovery() {
    let root = tempfile::tempdir().unwrap();
    let ownership = guard(&root);
    let mut journal = SessionExecutionStore::open(ownership.clone(), owner()).unwrap();
    let mut contents = ExecutionContentStore::open_owned(
        journal
            .component_namespace(ExecutionComponent::ExecutionContent)
            .unwrap(),
    )
    .unwrap();
    let request = contents
        .retain_request(content("turn", "Check this candidate"))
        .unwrap();
    let envelope = begin("turn");
    let receipt = journal
        .begin_with_request(envelope.clone(), &request)
        .unwrap();
    assert_eq!(
        journal
            .begin_with_request(envelope.clone(), &request)
            .unwrap(),
        receipt
    );
    let snapshot = journal.snapshot(&envelope.turn_id).unwrap();
    assert_eq!(snapshot.request_ref(), Some(request.reference()));
    assert!(matches!(
        contents.project(&snapshot).unwrap().request,
        ContentResolution::Available { .. }
    ));
    drop(contents);
    drop(journal);
    let mut journal = SessionExecutionStore::open(ownership, owner()).unwrap();
    let contents = ExecutionContentStore::open_owned(
        journal
            .component_namespace(ExecutionComponent::ExecutionContent)
            .unwrap(),
    )
    .unwrap();
    let snapshot = journal.snapshot(&envelope.turn_id).unwrap();
    assert_eq!(
        snapshot.contract().state(),
        Some(LogicalTurnState::NeedsAttention)
    );
    assert_eq!(snapshot.request_ref(), Some(request.reference()));
    assert!(matches!(
        contents.project(&snapshot).unwrap().request,
        ContentResolution::Available { .. }
    ));
    assert_eq!(
        journal.begin_with_request(envelope, &request).unwrap(),
        receipt
    );
}

#[test]
fn foreign_requests_and_retroactive_binding_cannot_change_canonical_begin() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let a = SessionExecutionStore::open(guard(&first), owner()).unwrap();
    let mut b = SessionExecutionStore::open(guard(&second), owner()).unwrap();
    let mut contents = ExecutionContentStore::open_owned(
        a.component_namespace(ExecutionComponent::ExecutionContent)
            .unwrap(),
    )
    .unwrap();
    let request = contents
        .retain_request(content("turn", "original"))
        .unwrap();
    assert!(b.begin_with_request(begin("turn"), &request).is_err());
    assert!(b.records().unwrap().is_empty());
    let mut own_contents = ExecutionContentStore::open_owned(
        b.component_namespace(ExecutionComponent::ExecutionContent)
            .unwrap(),
    )
    .unwrap();
    let own_request = own_contents
        .retain_request(content("turn", "original"))
        .unwrap();
    b.append(begin("turn")).unwrap();
    assert!(b.begin_with_request(begin("turn"), &own_request).is_err());
    assert!(b
        .snapshot(&LogicalTurnId::new("turn").unwrap())
        .unwrap()
        .request_ref()
        .is_none());
}

fn legacy_turn(store: &mut SessionTurnStore, id: &str) {
    store
        .begin(BeginSessionTurn {
            turn_id: Some(id.into()),
            session_id: "session".into(),
            user_input: "old request".into(),
            agent_id: Some("agent".into()),
            model: None,
            context: vec![],
            idempotency_key: None,
            metadata: serde_json::Map::new(),
        })
        .unwrap();
    store
        .transition(
            id,
            format!("close-{id}"),
            TransitionSessionTurn {
                status: SessionTurnLifecycle::Completed,
                final_output: Some("old answer".into()),
                error: None,
                metadata: serde_json::Map::new(),
            },
        )
        .unwrap();
}

#[test]
fn legacy_seal_preserves_bytes_and_links_only_the_first_v2_successor() {
    let root = tempfile::tempdir().unwrap();
    let mut legacy = SessionTurnStore::open(root.path().join("session-history")).unwrap();
    let ownership = guard(&root);
    legacy_turn(&mut legacy, "old-turn");
    let before = fs::read(legacy.path()).unwrap();
    let mut journal = SessionExecutionStore::open(ownership, owner()).unwrap();
    let mut contents = ExecutionContentStore::open_owned(
        journal
            .component_namespace(ExecutionComponent::ExecutionContent)
            .unwrap(),
    )
    .unwrap();
    let retained = contents
        .retain_legacy_history(&journal.legacy_history_snapshot().unwrap())
        .unwrap();
    let seal = journal.seal_legacy_history(&retained).unwrap();
    assert_eq!(journal.seal_legacy_history(&retained).unwrap(), seal);
    assert_eq!(seal.last_predecessor().unwrap().turn_id, "old-turn");
    assert_eq!(fs::read(legacy.path()).unwrap(), before);
    let request = contents
        .retain_request(content("turn", "new work"))
        .unwrap();
    journal.begin_with_request(begin("turn"), &request).unwrap();
    let id = LogicalTurnId::new("turn").unwrap();
    assert_eq!(
        journal.snapshot(&id).unwrap().legacy_predecessor(),
        seal.last_predecessor()
    );
    let mut closure = begin("turn");
    closure.command_id = CommandId::new("close-new").unwrap();
    closure.expected_revision = 1;
    closure.event = TurnContractEvent::Close {
        closure: TurnClosure::Finished,
    };
    journal.append(closure).unwrap();
    let predecessor = journal
        .snapshot(&id)
        .unwrap()
        .contract()
        .closed_reference()
        .unwrap();
    let mut next = begin("next");
    if let TurnContractEvent::Begin {
        predecessor: link, ..
    } = &mut next.event
    {
        *link = Some(predecessor);
    }
    let next_request = contents
        .retain_request(content("next", "later work"))
        .unwrap();
    journal
        .begin_with_request(next.clone(), &next_request)
        .unwrap();
    assert!(journal
        .snapshot(&next.turn_id)
        .unwrap()
        .legacy_predecessor()
        .is_none());
    legacy_turn(&mut legacy, "changed-frontier");
    assert!(journal.legacy_history_snapshot().is_err());
}

#[test]
fn owned_audit_authority_and_commands_retain_writer_and_reject_unowned_reopen() {
    let root = tempfile::tempdir().unwrap();
    let ownership = guard(&root);
    let journal = SessionExecutionStore::open(ownership.clone(), owner()).unwrap();
    let turn = LogicalTurnId::new("turn").unwrap();
    let audit = InvocationAudit::open_owned(
        journal
            .component_namespace(ExecutionComponent::InvocationAudit)
            .unwrap(),
    )
    .unwrap();
    let audit_path = audit.path();
    let authority = ControlAuthority::open_owned(
        journal
            .component_namespace(ExecutionComponent::ControlAuthority {
                turn_id: turn.clone(),
            })
            .unwrap(),
    )
    .unwrap();
    let commands = ControlCommandStore::open_owned(
        journal
            .component_namespace(ExecutionComponent::ControlCommands {
                turn_id: turn.clone(),
            })
            .unwrap(),
    )
    .unwrap();
    let commands_path = commands.path();
    drop(journal);
    assert!(SessionExecutionStore::open(ownership.clone(), owner()).is_err());
    assert_eq!(authority.revision().unwrap(), 0);
    assert!(audit.records().unwrap().is_empty());
    drop(audit);
    drop(authority);
    drop(commands);
    assert!(InvocationAudit::open(
        audit_path.parent().unwrap(),
        InvocationAuditOwner {
            workspace_id: "workspace".into(),
            session_id: owner().session_id
        }
    )
    .is_err());
    assert!(ControlCommandStore::open(
        commands_path.parent().unwrap(),
        ControlCommandOwner {
            workspace_id: "workspace".into(),
            session_id: owner().session_id,
            turn_id: turn
        }
    )
    .is_err());
    let journal = SessionExecutionStore::open(ownership, owner()).unwrap();
    assert!(InvocationAudit::open_owned(
        journal
            .component_namespace(ExecutionComponent::InvocationAudit)
            .unwrap()
    )
    .is_ok());
}

#[test]
fn duplicate_request_owner_in_journal_fails_without_overwriting_evidence() {
    let root = tempfile::tempdir().unwrap();
    let ownership = guard(&root);
    let mut journal = SessionExecutionStore::open(ownership.clone(), owner()).unwrap();
    let mut contents = ExecutionContentStore::open_owned(
        journal
            .component_namespace(ExecutionComponent::ExecutionContent)
            .unwrap(),
    )
    .unwrap();
    let request = contents
        .retain_request(content("turn", "original"))
        .unwrap();
    journal.begin_with_request(begin("turn"), &request).unwrap();
    // The Begin and its request binding are one record of the active segment.
    let path = journal
        .path()
        .parent()
        .unwrap()
        .join("execution.active.jsonl");
    drop(contents);
    drop(journal);
    let mut log = fs::read_to_string(&path).unwrap();
    let mut duplicate: serde_json::Value =
        serde_json::from_str(log.lines().nth(1).unwrap()).unwrap();
    assert_eq!(duplicate["record"]["kind"], "begin");
    duplicate["record"]["envelope"]["command_id"] = "begin-again".into();
    log.push_str(&format!("{duplicate}\n"));
    fs::write(&path, &log).unwrap();
    assert!(SessionExecutionStore::open(ownership, owner()).is_err());
    assert_eq!(fs::read_to_string(path).unwrap(), log);
}

#[test]
fn owned_component_journals_cannot_reset_after_the_primary_file_disappears() {
    let root = tempfile::tempdir().unwrap();
    let journal = SessionExecutionStore::open(guard(&root), owner()).unwrap();
    let turn = LogicalTurnId::new("turn").unwrap();
    let audit = InvocationAudit::open_owned(
        journal
            .component_namespace(ExecutionComponent::InvocationAudit)
            .unwrap(),
    )
    .unwrap();
    let audit_path = audit.path();
    drop(audit);
    let commands = ControlCommandStore::open_owned(
        journal
            .component_namespace(ExecutionComponent::ControlCommands {
                turn_id: turn.clone(),
            })
            .unwrap(),
    )
    .unwrap();
    let commands_path = commands.path();
    drop(commands);
    let authority = ControlAuthority::open_owned(
        journal
            .component_namespace(ExecutionComponent::ControlAuthority {
                turn_id: turn.clone(),
            })
            .unwrap(),
    )
    .unwrap();
    drop(authority);
    let authority_dir = fs::read_dir(journal.path().parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("control-authority-")
        })
        .unwrap();
    let authority_path = authority_dir.join("control-authority.v1.json");
    for path in [&audit_path, &commands_path, &authority_path] {
        fs::remove_file(path).unwrap();
    }
    assert!(InvocationAudit::open_owned(
        journal
            .component_namespace(ExecutionComponent::InvocationAudit)
            .unwrap()
    )
    .is_err());
    assert!(ControlCommandStore::open_owned(
        journal
            .component_namespace(ExecutionComponent::ControlCommands {
                turn_id: turn.clone()
            })
            .unwrap()
    )
    .is_err());
    assert!(ControlAuthority::open_owned(
        journal
            .component_namespace(ExecutionComponent::ControlAuthority { turn_id: turn })
            .unwrap()
    )
    .is_err());
    for path in [&audit_path, &commands_path, &authority_path] {
        assert!(
            !path.exists(),
            "missing retained history must not become an empty journal"
        );
    }
}
