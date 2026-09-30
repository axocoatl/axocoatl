use super::*;
use axocoatl_session::execution_ownership::LegacyFormatOwnership;
use axocoatl_session::execution_store::{ExecutionStoreOwner, SessionExecutionStore};
use axocoatl_session::turn_contract::*;
use std::sync::Arc;
fn draft(id: &str) -> KnowledgeDraft {
    KnowledgeDraft {
        id: id.into(),
        title: "Repository decision".into(),
        body: "Use a durable journal.\n\n## Why\nRecovery matters.\n".into(),
        kind: KnowledgeKind::Decision,
        links: vec![],
        sources: vec![],
        provenance: KnowledgeProvenance::Human {
            author: Some("maintainer".into()),
        },
    }
}
fn open(path: &std::path::Path) -> KnowledgeStore {
    let mut store = KnowledgeStore::open(SecureDir::open_or_create_all(path).unwrap()).unwrap();
    store.bind_workspace("workspace-a").unwrap();
    store
}
#[test]
fn knowledge_versions_reopen_export_and_revision_conflicts() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(dir.path());
    let one = store.save(draft("decision"), 0).unwrap();
    let mut edited = one.draft();
    edited.body = "Human correction".into();
    assert!(matches!(
        store.save(edited.clone(), 0),
        Err(KnowledgeError::Conflict { .. })
    ));
    let two = store.save(edited, 1).unwrap();
    assert_eq!(two.revision, 2);
    assert_eq!(store.read("decision", Some(1)).unwrap(), one);
    let markdown = store.export("decision", Some(1)).unwrap();
    assert!(markdown.contains("## Why"));
    assert_eq!(parse_knowledge_markdown(&markdown).unwrap(), one);
    drop(store);
    let store = open(dir.path());
    assert_eq!(store.read("decision", None).unwrap(), two);
}
#[test]
fn knowledge_corruption_never_resets() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(dir.path());
    store.save(draft("d"), 0).unwrap();
    let path = store.state.records["d"][0].file.clone();
    drop(store);
    std::fs::write(dir.path().join("v1").join(path), "corrupt").unwrap();
    assert!(KnowledgeStore::open(SecureDir::open(dir.path()).unwrap()).is_err());
    assert!(dir.path().join("v1/knowledge.json").exists());
}
#[test]
fn knowledge_owner_and_single_writer_are_enforced() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(dir.path());
    assert!(store.bind_workspace("other").is_err());
    #[cfg(unix)]
    assert!(KnowledgeStore::open(SecureDir::open(dir.path()).unwrap()).is_err());
    let other = tempfile::tempdir().unwrap();
    let second = open(other.path());
    store.save(draft("private"), 0).unwrap();
    assert!(second.list().unwrap().is_empty());
}
#[test]
fn knowledge_backlinks_search_bounds_and_snapshot_relative_freshness() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(dir.path());
    store.save(draft("first"), 0).unwrap();
    let mut note = draft("second");
    note.links.push(KnowledgeLink {
        kind: KnowledgeLinkKind::Supports,
        target: "first".into(),
    });
    note.sources.push(KnowledgeSource {
        path: "src/main.rs".into(),
        sha256: knowledge_content_hash("old"),
        symbol: Some("main".into()),
        role: Default::default(),
    });
    let record = store.save(note, 0).unwrap();
    assert_eq!(store.backlinks("first").unwrap()[0].id, "second");
    let a = BTreeMap::from([("src/main.rs".into(), knowledge_content_hash("old"))]);
    let b = BTreeMap::from([("src/main.rs".into(), knowledge_content_hash("new"))]);
    assert_eq!(
        record.applicability(&a)[0].status,
        SourceApplicability::Current
    );
    assert_eq!(
        record.applicability(&b)[0].status,
        SourceApplicability::Changed
    );
    assert_eq!(
        record.applicability(&BTreeMap::new())[0].status,
        SourceApplicability::Unavailable
    );
    assert_eq!(store.search("main.rs", &a, 10, 8192).unwrap().len(), 1);
    assert!(store.search("", &a, 10, 2).unwrap().is_empty());
    assert!(store.search("", &a, 51, 8192).is_err());
}
#[test]
fn knowledge_paths_and_duplicate_links_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(dir.path());
    for path in ["../secret", "/tmp/x", "a/../b", "a\\b", "a//b", "a\0b"] {
        let mut note = draft("d");
        note.sources.push(KnowledgeSource {
            path: path.into(),
            sha256: knowledge_content_hash("x"),
            symbol: None,
            role: Default::default(),
        });
        assert!(store.save(note, 0).is_err());
    }
    let mut note = draft("d");
    note.links = vec![
        KnowledgeLink {
            kind: KnowledgeLinkKind::Related,
            target: "a".into()
        };
        2
    ];
    assert!(store.save(note, 0).is_err());
}
fn history(workspace: &str) -> (tempfile::TempDir, SessionExecutionStore) {
    let root = tempfile::tempdir().unwrap();
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
            session_id: SessionId::new("session-a").unwrap(),
        },
    )
    .unwrap();
    (root, store)
}
fn ev(id: &str) -> EvidenceRef {
    EvidenceRef::new(id).unwrap()
}
fn append(store: &mut SessionExecutionStore, event: TurnContractEvent) {
    let turn = LogicalTurnId::new("turn-a").unwrap();
    let revision = store
        .snapshot(&turn)
        .ok()
        .map_or(0, |s| s.contract().revision());
    store
        .append(TurnContractEnvelope {
            schema_version: TURN_CONTRACT_SCHEMA_VERSION,
            command_id: CommandId::new(format!("command-{revision}")).unwrap(),
            expected_revision: revision,
            session_id: SessionId::new("session-a").unwrap(),
            turn_id: turn,
            event,
        })
        .unwrap();
}
fn started(store: &mut SessionExecutionStore) -> ActivationRef {
    let definition = DefinitionSnapshotRef {
        definition_id: AgentDefinitionId::new("coder").unwrap(),
        snapshot: ev("definition"),
    };
    let conversation = NodeConversationId::new("conversation").unwrap();
    append(
        store,
        TurnContractEvent::Begin {
            epoch_id: ExecutionEpochId::new("epoch").unwrap(),
            graph: TurnGraphSnapshot {
                snapshot_id: GraphSnapshotId::new("graph").unwrap(),
                revision: 1,
                nodes: vec![GraphNode {
                    node_id: TurnNodeId::new("node").unwrap(),
                    slot_id: SessionTeamSlotId::new("slot").unwrap(),
                    definition: definition.clone(),
                    conversation_id: conversation.clone(),
                    starting_savepoint: ConversationSavepoint::Empty,
                    required: true,
                }],
                dependencies: vec![],
                conditions: vec![],
            },
            predecessor: None,
        },
    );
    let activation = ActivationRef {
        session_id: SessionId::new("session-a").unwrap(),
        turn_id: LogicalTurnId::new("turn-a").unwrap(),
        execution_epoch_id: ExecutionEpochId::new("epoch").unwrap(),
        node_id: TurnNodeId::new("node").unwrap(),
        generation: 1,
        activation_id: ActivationId::new("activation").unwrap(),
    };
    append(
        store,
        TurnContractEvent::StartActivation {
            input: Box::new(ActivationInputManifest {
                manifest_id: InputManifestId::new("input").unwrap(),
                activation: activation.clone(),
                definition,
                conversation_id: conversation,
                starting_savepoint: ConversationSavepoint::Empty,
                parents: vec![],
                guidance: vec![ev("task")],
                attachments: vec![],
                repository: RepositoryInput::Unavailable,
                budget: ev("budget"),
                grant: None,
                revision_context: None,
            }),
        },
    );
    activation
}
fn accepted(store: &mut SessionExecutionStore, activation: &ActivationRef) {
    append(
        store,
        TurnContractEvent::AcceptActivation {
            activation: activation.clone(),
            checkpoint: Box::new(CheckpointRef {
                checkpoint_id: CheckpointId::new("checkpoint").unwrap(),
                session_id: activation.session_id.clone(),
                conversation_id: NodeConversationId::new("conversation").unwrap(),
                source: CheckpointSource::Accepted {
                    activation: activation.clone(),
                },
            }),
            output: ev("output"),
        },
    );
}
fn proposal(
    store: &mut KnowledgeStore,
    history: &SessionExecutionStore,
    activation: &ActivationRef,
    id: &str,
) -> KnowledgeProposal {
    let mut note = draft(id);
    note.provenance = KnowledgeProvenance::Model {
        journal_id: history.identity().unwrap().journal_id().into(),
        activation: activation.clone(),
    };
    store
        .propose(
            note,
            0,
            activation,
            history.identity().unwrap().journal_id(),
        )
        .unwrap()
}
#[test]
fn knowledge_agent_promotion_requires_closed_exact_accepted_generation() {
    let dir = tempfile::tempdir().unwrap();
    let mut notes = open(dir.path());
    let (_root, mut turns) = history("workspace-a");
    let activation = started(&mut turns);
    let p = proposal(&mut notes, &turns, &activation, "learned");
    let mut unaccepted = activation.clone();
    unaccepted.generation = 2;
    unaccepted.activation_id = ActivationId::new("unaccepted-generation").unwrap();
    let wrong = proposal(&mut notes, &turns, &unaccepted, "wrong-generation");
    drop(notes);
    let mut notes = open(dir.path());
    assert_eq!(
        notes.proposal(&p.id).unwrap().status,
        ProposalStatus::Pending
    );
    assert!(notes.list().unwrap().is_empty());
    assert!(notes
        .publish(&p.id, &turns.snapshot(&activation.turn_id).unwrap())
        .is_err());
    accepted(&mut turns, &activation);
    assert!(notes
        .publish(&p.id, &turns.snapshot(&activation.turn_id).unwrap())
        .is_err());
    append(
        &mut turns,
        TurnContractEvent::Close {
            closure: TurnClosure::Completed,
        },
    );
    let closed = turns.snapshot(&activation.turn_id).unwrap();
    assert!(notes.publish(&wrong.id, &closed).is_err());
    let one = notes.publish(&p.id, &closed).unwrap();
    assert_eq!(one.revision, 1);
    assert_eq!(notes.publish(&p.id, &closed).unwrap(), one);
    drop(notes);
    let mut notes = open(dir.path());
    assert_eq!(notes.publish(&p.id, &closed).unwrap(), one);
    let mut human = one.draft();
    human.provenance = KnowledgeProvenance::Human { author: None };
    human.body = "Human correction".into();
    let two = notes.save(human, 1).unwrap();
    assert_eq!(notes.publish(&p.id, &closed).unwrap(), one);
    assert_eq!(notes.read("learned", None).unwrap(), two);
}
#[test]
fn knowledge_failed_cancelled_or_foreign_workspace_cannot_publish() {
    let dir = tempfile::tempdir().unwrap();
    let mut notes = open(dir.path());
    let (_root, mut turns) = history("workspace-b");
    let activation = started(&mut turns);
    let p = proposal(&mut notes, &turns, &activation, "wrong-workspace");
    accepted(&mut turns, &activation);
    append(
        &mut turns,
        TurnContractEvent::Close {
            closure: TurnClosure::Completed,
        },
    );
    assert!(notes
        .publish(&p.id, &turns.snapshot(&activation.turn_id).unwrap())
        .is_err());
    let (_root, mut turns) = history("workspace-a");
    let activation = started(&mut turns);
    let p = proposal(&mut notes, &turns, &activation, "failed");
    append(
        &mut turns,
        TurnContractEvent::FailActivation {
            activation: activation.clone(),
            evidence: ev("failure"),
        },
    );
    append(
        &mut turns,
        TurnContractEvent::Close {
            closure: TurnClosure::Cancelled,
        },
    );
    assert!(notes
        .publish(&p.id, &turns.snapshot(&activation.turn_id).unwrap())
        .is_err());
}
#[test]
fn knowledge_proposal_human_decisions_preserve_provenance_and_conflicts() {
    let dir = tempfile::tempdir().unwrap();
    let mut notes = open(dir.path());
    let (_root, mut turns) = history("workspace-a");
    let activation = started(&mut turns);
    let p = proposal(&mut notes, &turns, &activation, "candidate");
    let again = notes
        .propose(p.note.clone(), 0, &activation, &p.journal_id)
        .unwrap();
    assert_eq!(p, again);
    let mut changed = p.note.clone();
    changed.body = "changed".into();
    assert!(notes
        .propose(changed, 0, &activation, &p.journal_id)
        .is_err());
    assert!(notes.save(p.note.clone(), 0).is_err());
    assert!(notes.accept_human(&p.id, 1).is_err());
    let human = notes.accept_human(&p.id, 0).unwrap();
    assert_eq!(human.provenance, p.note.provenance);
    assert_eq!(human.acceptance, KnowledgeAcceptance::Human);
    let p = proposal(&mut notes, &turns, &activation, "rejected");
    notes.reject(&p.id).unwrap();
    assert!(notes.accept_human(&p.id, 0).is_err());
    let p = proposal(&mut notes, &turns, &activation, "race");
    notes.save(draft("race"), 0).unwrap();
    assert!(matches!(
        notes.accept_human(&p.id, 0),
        Err(KnowledgeError::Conflict { .. })
    ));
    assert_eq!(
        notes.proposal(&p.id).unwrap().status,
        ProposalStatus::Pending
    );
}
#[test]
fn knowledge_real_parsers_and_unsupported_source_search() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(dir.path());
    let files=vec![SourceFile{path:"a.rs".into(),content:"// fn fake() {}\nuse std::fmt;\npub struct Thing;\nimpl Thing { pub fn run(&self) {} }\n".into()},SourceFile{path:"a.py".into(),content:"# def fake(): pass\nimport os\nclass Thing:\n    def run(self):\n        pass\n".into()},SourceFile{path:"a.js".into(),content:"import {x} from './x.js';\nexport function run() { return x; }".into()},SourceFile{path:"a.ts".into(),content:"import type { X } from './x';\nexport interface Shape { x: number }\nexport function run(x: number): number { return x; }".into()},SourceFile{path:"notes.txt".into(),content:"def not_a_python_definition():\nknowledge text".into()}];
    let index = store
        .rebuild_source_index("session-a:tree-1", &files)
        .unwrap();
    for file in &index.files {
        if file.path.ends_with("txt") {
            assert_eq!(file.parse_status, SourceParseStatus::Unsupported);
            assert!(file.definitions.is_empty())
        } else {
            assert_eq!(
                file.parse_status,
                SourceParseStatus::Parsed,
                "{}",
                file.path
            );
            assert!(!file.imports.is_empty());
            assert!(file.definitions.iter().any(|s| s.name == "run"));
            assert!(!file.definitions.iter().any(|s| s.name == "fake"));
            assert!(file.imports.iter().all(|i| !i.resolved));
        }
    }
    assert_eq!(
        store.source_index("session-a:tree-1").unwrap().unwrap(),
        index
    );
    assert_eq!(
        index.search("knowledge", 10, 8192).unwrap()[0].path,
        "notes.txt"
    );
    assert!(index.repository_map(100).unwrap().len() <= 100);
    assert!(store.source_index("different-checkout").unwrap().is_none());
}
#[test]
fn knowledge_index_rebuild_and_snapshot_separation() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(dir.path());
    let a = store
        .rebuild_source_index(
            "session-a",
            &[SourceFile {
                path: "a.py".into(),
                content: "def first(): pass".into(),
            }],
        )
        .unwrap();
    let b = store
        .rebuild_source_index(
            "session-b",
            &[SourceFile {
                path: "a.py".into(),
                content: "def second(): pass".into(),
            }],
        )
        .unwrap();
    assert_ne!(a.manifest_sha256, b.manifest_sha256);
    assert_eq!(store.source_index("session-a").unwrap().unwrap(), a);
    std::fs::write(
        dir.path().join("v1").join(index::index_path("session-a")),
        "bad",
    )
    .unwrap();
    assert!(store.source_index("session-a").is_err());
    assert!(store
        .rebuild_source_index(
            "session-a",
            &[SourceFile {
                path: "a.py".into(),
                content: "def first(): pass".into()
            }]
        )
        .is_ok());
}

#[test]
fn knowledge_natural_language_retrieval_matches_terms_without_stopword_noise() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(dir.path());
    let mut relevant = draft("retry");
    relevant.title = "Retry behavior".into();
    relevant.body = "Preserve exact source inputs before retry.".into();
    store.save(relevant, 0).unwrap();
    let mut unrelated = draft("theme");
    unrelated.title = "Theme palette".into();
    unrelated.body = "The repository has blue panels for this application.".into();
    store.save(unrelated, 0).unwrap();
    for query in [
        "Explain retry behavior for the repository",
        "How should retries work in this codebase?",
    ] {
        let hits = store
            .search(query, &SnapshotSources::new(), 10, 8192)
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "retry");
    }
    assert!(store
        .search("How should colors work?", &SnapshotSources::new(), 10, 8192)
        .unwrap()
        .is_empty());
}
#[test]
fn a_proposal_too_large_to_publish_is_refused_when_proposed() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(dir.path());
    let activation = ActivationRef {
        session_id: SessionId::new("session-a").unwrap(),
        turn_id: LogicalTurnId::new("turn-a").unwrap(),
        execution_epoch_id: ExecutionEpochId::new("epoch").unwrap(),
        node_id: TurnNodeId::new("node").unwrap(),
        generation: 1,
        activation_id: ActivationId::new("activation").unwrap(),
    };
    let mut note = draft("large-finding");
    note.kind = KnowledgeKind::Finding;
    note.provenance = KnowledgeProvenance::Model {
        journal_id: "journal-a".into(),
        activation: activation.clone(),
    };
    // Each part fits its own limit; together the rendered note does not.
    note.body = "x".repeat(60 * 1024);
    note.sources = (0..128)
        .map(|index| KnowledgeSource {
            path: format!("{}/file-{index}.rs", "d".repeat(240)),
            sha256: "a".repeat(64),
            symbol: None,
            role: SourceRole::MustChange,
        })
        .collect();
    assert!(matches!(
        store.check_proposal(&note, 0, &activation, "journal-a"),
        Err(KnowledgeError::Invalid(message)) if message.contains("too large to publish")
    ));
    assert!(matches!(
        store.propose(note.clone(), 0, &activation, "journal-a"),
        Err(KnowledgeError::Invalid(message)) if message.contains("too large to publish")
    ));
    assert!(store.proposals().unwrap().is_empty(), "nothing was staged");
    note.sources.truncate(4);
    store
        .check_proposal(&note, 0, &activation, "journal-a")
        .unwrap();
    store
        .propose(note.clone(), 0, &activation, "journal-a")
        .unwrap();
    // The same id and revision staged again with other content is refused
    // before anything is spent on it.
    let mut other = note.clone();
    other.title = "Another finding".into();
    assert!(matches!(
        store.check_proposal(&other, 0, &activation, "journal-a"),
        Err(KnowledgeError::Invalid(message)) if message.contains("already staged")
    ));
    store
        .check_proposal(&note, 0, &activation, "journal-a")
        .unwrap();
}
