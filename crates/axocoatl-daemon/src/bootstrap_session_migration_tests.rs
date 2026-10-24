use super::super::DataDirLease;
use super::*;
use axocoatl_actor::{AgentActor, AgentBehavior};
use axocoatl_core::{AgentConfig, AgentId, AgentInput, AgentOutput, MessageRole, TokenUsageStats};
use axocoatl_memory::{AgentCheckpoint, CheckpointPolicy, StoredMessage};
use axocoatl_session::{BeginSessionTurn, SessionTurnLifecycle, TransitionSessionTurn};
use ractor::Actor;

fn checkpoint(agent: &str, version: u64, known: bool) -> AgentCheckpoint {
    AgentCheckpoint {
        version,
        agent_id: agent.to_owned(),
        checkpoint_time: 1,
        session_messages: vec![StoredMessage {
            content_parts: None,
            role: MessageRole::Assistant,
            content: "checkpoint-private partial work".to_owned(),
            timestamp: 1,
            token_count: 7,
            name: None,
            tool_calls: vec![],
            tool_call_id: None,
        }],
        cumulative_token_usage: TokenUsageStats::new(
            1000 * version as usize,
            500 * version as usize,
        ),
        cumulative_token_usage_known: known,
        behavior_state: Some("{\"goal\":\"partial orchestration\",\"completed\":false}".to_owned()),
    }
}

fn spec(session: &str, ordinary: bool) -> LegacySessionMigration {
    let actors = if ordinary {
        vec![("solo", format!("{session}:solo"))]
    } else {
        vec![
            ("lead", format!("{session}:lead")),
            ("reviewer", format!("{session}:lead:worker:reviewer")),
        ]
    };
    LegacySessionMigration {
        session_id: session.to_owned(),
        workspace_id: "workspace".to_owned(),
        actors: actors
            .into_iter()
            .map(|(recorded, scoped)| LegacyActorMigration {
                checkpoint_agent_id: scoped,
                recorded_agent_id: recorded.to_owned(),
                policy: if ordinary {
                    LegacyActorProjectionPolicy::OrdinaryAutonomous
                } else {
                    LegacyActorProjectionPolicy::CompletedPerAgent
                },
                tool_replay_policy: ToolReplayPolicy::CompleteNativeGroups,
            })
            .collect(),
    }
}

fn begin(ledger: &mut SessionTurnStore, session: &str, turn: &str, ordinary: bool, terminal: bool) {
    ledger
        .begin(BeginSessionTurn {
            turn_id: Some(turn.to_owned()),
            session_id: session.to_owned(),
            user_input: format!("request {turn}"),
            agent_id: ordinary.then(|| "solo".to_owned()),
            model: None,
            context: vec![],
            idempotency_key: None,
            metadata: serde_json::Map::new(),
        })
        .unwrap();
    for agent in if ordinary {
        vec!["solo"]
    } else {
        vec!["lead", "reviewer"]
    } {
        ledger
            .record_agent_output(
                turn,
                format!("{turn}:{agent}:output"),
                agent,
                None,
                format!("{agent} own completed answer"),
                None,
            )
            .unwrap();
    }
    if terminal {
        ledger
            .transition(
                turn,
                format!("{turn}:terminal"),
                TransitionSessionTurn {
                    status: SessionTurnLifecycle::Completed,
                    final_output: Some("aggregate output".to_owned()),
                    error: None,
                    metadata: serde_json::Map::new(),
                },
            )
            .unwrap();
    }
}

#[tokio::test]
async fn real_host_conversion_reconciles_staged_roles_seals_history_and_reopens_exact_baselines() {
    let root = tempfile::tempdir().unwrap();
    let data = SecureDir::open(root.path()).unwrap();
    let lease = DataDirLease::acquire(&data).unwrap();
    let source =
        CheckpointStore::new_in_secure(&data, "checkpoints", CheckpointPolicy::Manual).unwrap();
    let mut ledger = SessionTurnStore::open_in_secure(&data, "session-history").unwrap();
    let specs = vec![spec("ordinary", true), spec("coordinator", false)];
    for specification in &specs {
        begin(
            &mut ledger,
            &specification.session_id,
            &format!("turn-{}", specification.session_id),
            specification.session_id == "ordinary",
            true,
        );
        for actor in &specification.actors {
            source
                .save(&checkpoint(
                    &actor.checkpoint_agent_id,
                    1,
                    actor.recorded_agent_id != "reviewer",
                ))
                .await
                .unwrap();
        }
    }
    let original = std::fs::read(ledger.path()).unwrap();
    source
        .begin_session_turn("coordinator", "turn-coordinator")
        .await
        .unwrap();
    source
        .scoped_to_session_turn("coordinator", "turn-coordinator")
        .save(&checkpoint("coordinator:lead", 2, true))
        .await
        .unwrap();
    drop(ledger);
    let actors = AgentRegistry::new();
    let active = Mutex::new(HashMap::new());
    require_migration_quiescence(&actors, &active)
        .await
        .unwrap();
    let ownership = lease.ownership.into_upgraded().unwrap();
    let migrated = migrate_held_session_state(
        ownership.clone(),
        &data,
        &source,
        &actors,
        &active,
        &specs,
        &str::len,
    )
    .await
    .unwrap();
    assert!(source.list_session_turn_transactions().unwrap().is_empty());
    let mut expected = Vec::new();
    for (result, spec) in migrated.iter().zip(&specs) {
        assert!(result.canonical.records().unwrap().is_empty());
        assert_eq!(
            result
                .content
                .read_legacy_history(&result.seal)
                .unwrap()
                .turns
                .len(),
            1
        );
        for assignment in &deterministic_assignments(spec).unwrap() {
            let baseline = result
                .activation_state
                .committed_reference(&assignment.conversation_id)
                .unwrap()
                .unwrap();
            let checkpoint = result.activation_state.checkpoint(&baseline).unwrap();
            assert_eq!(
                checkpoint.session_messages[1].content,
                format!("{} own completed answer", assignment.recorded_agent_id)
            );
            assert!(checkpoint.behavior_state.is_none());
            assert_eq!(
                checkpoint.cumulative_token_usage.input_tokens,
                if assignment.recorded_agent_id == "lead" {
                    2000
                } else {
                    1000
                }
            );
            assert_eq!(
                checkpoint.cumulative_token_usage_known,
                assignment.recorded_agent_id != "reviewer"
            );
            assert!(result
                .activation_state
                .legacy_checkpoint_source_archive(&assignment.conversation_id)
                .unwrap()
                .is_some());
            expected.push((assignment.checkpoint_agent_id.clone(), baseline));
        }
    }
    drop(migrated);
    let reopened = migrate_held_session_state(
        ownership.clone(),
        &data,
        &source,
        &actors,
        &active,
        &specs,
        &str::len,
    )
    .await
    .unwrap();
    for (result, spec) in reopened.iter().zip(&specs) {
        for assignment in &deterministic_assignments(spec).unwrap() {
            let actual = result
                .activation_state
                .committed_reference(&assignment.conversation_id)
                .unwrap()
                .unwrap();
            assert_eq!(
                &actual,
                &expected
                    .iter()
                    .find(|(id, _)| id == &assignment.checkpoint_agent_id)
                    .unwrap()
                    .1
            );
        }
    }
    assert_eq!(
        std::fs::read(root.path().join("session-history/turns.v1.jsonl")).unwrap(),
        original
    );
}

/// A 1.0 multi-Agent Session ran its Agents one after another and recorded
/// each Agent's ordinary output. Those turns are refused on a 1.0 root now,
/// and `axocoatl session upgrade --confirm` is how the Session keeps working:
/// every Agent keeps its own completed answer and the request it received.
#[tokio::test]
async fn multi_agent_legacy_session_upgrades_each_agent_output() {
    let root = tempfile::tempdir().unwrap();
    let data = SecureDir::open(root.path()).unwrap();
    let lease = DataDirLease::acquire(&data).unwrap();
    let source =
        CheckpointStore::new_in_secure(&data, "checkpoints", CheckpointPolicy::Manual).unwrap();
    let mut ledger = SessionTurnStore::open_in_secure(&data, "session-history").unwrap();
    let session = "team";
    let turn = "turn-team";
    ledger
        .begin(BeginSessionTurn {
            turn_id: Some(turn.to_owned()),
            session_id: session.to_owned(),
            user_input: "request turn-team".to_owned(),
            agent_id: None,
            model: None,
            context: vec![],
            idempotency_key: None,
            metadata: serde_json::json!({"mode": "custom"})
                .as_object()
                .cloned()
                .unwrap(),
        })
        .unwrap();
    for agent in ["coder", "reviewer"] {
        ledger
            .record_agent_output(
                turn,
                format!("{turn}:{agent}:output"),
                agent,
                None,
                format!("{agent} own completed answer"),
                None,
            )
            .unwrap();
        source
            .save(&checkpoint(&format!("{session}:{agent}"), 1, true))
            .await
            .unwrap();
    }
    ledger
        .transition(
            turn,
            format!("{turn}:terminal"),
            TransitionSessionTurn {
                status: SessionTurnLifecycle::Completed,
                final_output: Some("aggregate output".to_owned()),
                error: None,
                metadata: serde_json::Map::new(),
            },
        )
        .unwrap();
    drop(ledger);
    let specs = vec![LegacySessionMigration {
        session_id: session.to_owned(),
        workspace_id: "workspace".to_owned(),
        actors: ["coder", "reviewer"]
            .into_iter()
            .map(|agent| LegacyActorMigration {
                checkpoint_agent_id: format!("{session}:{agent}"),
                recorded_agent_id: agent.to_owned(),
                policy: LegacyActorProjectionPolicy::CompletedPerAgent,
                tool_replay_policy: ToolReplayPolicy::CompleteNativeGroups,
            })
            .collect(),
    }];
    let actors = AgentRegistry::new();
    let active = Mutex::new(HashMap::new());
    let ownership = lease.ownership.into_upgraded().unwrap();
    let migrated = migrate_held_session_state(
        ownership,
        &data,
        &source,
        &actors,
        &active,
        &specs,
        &str::len,
    )
    .await
    .unwrap();
    assert_eq!(migrated.len(), 1);
    let result = &migrated[0];
    assert_eq!(
        result
            .content
            .read_legacy_history(&result.seal)
            .unwrap()
            .turns
            .len(),
        1
    );
    let assignments = deterministic_assignments(&specs[0]).unwrap();
    assert_eq!(assignments.len(), 2);
    for assignment in &assignments {
        let baseline = result
            .activation_state
            .committed_reference(&assignment.conversation_id)
            .unwrap()
            .unwrap();
        let checkpoint = result.activation_state.checkpoint(&baseline).unwrap();
        assert_eq!(checkpoint.session_messages[0].content, "request turn-team");
        assert_eq!(
            checkpoint.session_messages[1].content,
            format!("{} own completed answer", assignment.recorded_agent_id)
        );
    }
}

struct NoopBehavior;
#[async_trait::async_trait]
impl AgentBehavior for NoopBehavior {
    async fn on_start(
        &mut self,
        _: &AgentConfig,
    ) -> std::result::Result<(), axocoatl_actor::AgentError> {
        Ok(())
    }
    async fn execute(
        &mut self,
        _: AgentInput,
    ) -> std::result::Result<AgentOutput, axocoatl_actor::AgentError> {
        Ok(AgentOutput::text("unused"))
    }
    async fn on_stop(&mut self) -> std::result::Result<(), axocoatl_actor::AgentError> {
        Ok(())
    }
}

#[tokio::test]
async fn a_real_actor_must_be_joined_and_unregistered_before_explicit_upgrade() {
    let root = tempfile::tempdir().unwrap();
    let data = SecureDir::open(root.path()).unwrap();
    let lease = DataDirLease::acquire(&data).unwrap();
    let actors = AgentRegistry::new();
    let active = Mutex::new(HashMap::new());
    let id = AgentId::new("migration-session:agent");
    let (actor, handle) = AgentActor::spawn(
        None,
        AgentActor,
        (
            AgentConfig::default(),
            Box::new(NoopBehavior) as Box<dyn AgentBehavior>,
        ),
    )
    .await
    .unwrap();
    actors.register(id.clone(), actor.clone()).await;
    assert!(require_migration_quiescence(&actors, &active)
        .await
        .is_err());
    // The root stays in its legacy format until the actors are joined.
    assert!(matches!(
        lease.ownership,
        DataRootFormatOwnership::Legacy(_)
    ));
    actor
        .stop_and_wait(None, Some(std::time::Duration::from_secs(3)))
        .await
        .unwrap();
    handle.await.unwrap();
    assert!(require_migration_quiescence(&actors, &active)
        .await
        .is_err());
    actors.remove(&id).await;
    require_migration_quiescence(&actors, &active)
        .await
        .unwrap();
    let _upgraded = lease.ownership.into_upgraded().unwrap();
}

#[tokio::test]
async fn foreign_checkpoint_root_is_refused_before_transaction_reconciliation() {
    let root = tempfile::tempdir().unwrap();
    let data = SecureDir::open(root.path()).unwrap();
    let lease = DataDirLease::acquire(&data).unwrap();
    let foreign = tempfile::tempdir().unwrap();
    let foreign_root = SecureDir::open(foreign.path()).unwrap();
    let source =
        CheckpointStore::new_in_secure(&foreign_root, "checkpoints", CheckpointPolicy::Manual)
            .unwrap();
    source.begin_session_turn("session", "turn").await.unwrap();
    let before = source.list_session_turn_transactions().unwrap();
    let ownership = lease.ownership.into_upgraded().unwrap();
    assert!(migrate_held_session_state(
        ownership,
        &data,
        &source,
        &AgentRegistry::new(),
        &Mutex::new(HashMap::new()),
        &[spec("session", true)],
        &str::len
    )
    .await
    .is_err());
    assert_eq!(source.list_session_turn_transactions().unwrap(), before);
}

#[tokio::test]
async fn actual_running_canonical_work_is_not_migrated_as_completed() {
    let root = tempfile::tempdir().unwrap();
    let data = SecureDir::open(root.path()).unwrap();
    let lease = DataDirLease::acquire(&data).unwrap();
    let source =
        CheckpointStore::new_in_secure(&data, "checkpoints", CheckpointPolicy::Manual).unwrap();
    let mut ledger = SessionTurnStore::open_in_secure(&data, "session-history").unwrap();
    begin(&mut ledger, "session", "running-turn", true, false);
    source
        .save(&checkpoint("session:solo", 1, true))
        .await
        .unwrap();
    let original = std::fs::read(ledger.path()).unwrap();
    let ownership = lease.ownership.into_upgraded().unwrap();
    assert!(migrate_held_session_state(
        ownership,
        &data,
        &source,
        &AgentRegistry::new(),
        &Mutex::new(HashMap::new()),
        &[spec("session", true)],
        &str::len
    )
    .await
    .is_err());
    assert_eq!(std::fs::read(ledger.path()).unwrap(), original);
    assert!(!root.path().join("execution-v2").exists());
}

struct StartupFixture {
    _root: tempfile::TempDir,
    _workspace: tempfile::TempDir,
    data: SecureDir,
    lease: Option<DataDirLease>,
    checkpoints: CheckpointStore,
    session: axocoatl_session::Session,
}

async fn source_proven_startup_fixture() -> StartupFixture {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let data = SecureDir::open(root.path()).unwrap();
    let lease = DataDirLease::acquire(&data).unwrap();
    let mut sessions = axocoatl_session::SessionStore::new_in_secure(&data, "sessions").unwrap();
    let session = sessions
        .create_with_environment(
            "Old team",
            "workspace",
            workspace.path(),
            axocoatl_session::SessionMode::Custom {
                agents: vec!["lead".into(), "reviewer".into()],
            },
            vec![],
            vec![],
            None,
            None,
            false,
            true,
        )
        .unwrap();
    let mut ledger = SessionTurnStore::open_in_secure(&data, "session-history").unwrap();
    begin(
        &mut ledger,
        &session.id,
        "historical-team-turn",
        false,
        true,
    );
    let checkpoints =
        CheckpointStore::new_in_secure(&data, "checkpoints", CheckpointPolicy::Manual).unwrap();
    checkpoints
        .save(&checkpoint(&format!("{}:lead", session.id), 1, true))
        .await
        .unwrap();
    let mut worker = checkpoint(&format!("{}:lead:worker:reviewer", session.id), 1, false);
    worker.behavior_state = None;
    checkpoints.save(&worker).await.unwrap();
    StartupFixture {
        _root: root,
        _workspace: workspace,
        data,
        lease: Some(lease),
        checkpoints,
        session,
    }
}

#[tokio::test]
async fn prepared_source_proven_upgrade_resumes_after_restart_without_settings_or_replay() {
    let mut f = source_proven_startup_fixture().await;
    let before_history =
        std::fs::read(f.data.path().join("session-history/turns.v1.jsonl")).unwrap();
    let before_checkpoints = f
        .checkpoints
        .capture_legacy_session_checkpoints(&f.session.id)
        .unwrap()
        .source_sha256()
        .to_owned();
    let preparation = PreparedStartupMigration::prepare_source_proven(
        &f.lease.as_ref().unwrap().ownership,
        &f.data,
        &f.checkpoints,
        std::slice::from_ref(&f.session),
    )
    .unwrap();
    let assignments = deterministic_assignments(&preparation.sessions[0].specification).unwrap();
    let (upgraded, _) = preparation
        .upgrade_held(
            f.lease.take().unwrap(),
            &f.data,
            &f.checkpoints,
            &AgentRegistry::new(),
            &Mutex::new(HashMap::new()),
        )
        .await
        .unwrap();
    assert!(matches!(
        upgraded.ownership,
        DataRootFormatOwnership::Upgraded(_)
    ));
    if let DataRootFormatOwnership::Upgraded(ownership) = &upgraded.ownership {
        let retained = PreparedStartupMigration::load_existing(&f.data, &f.checkpoints)
            .unwrap()
            .unwrap();
        initialize_migration_session(
            ownership.clone(),
            &f.data,
            &retained.sessions[0].specification,
        )
        .unwrap();
    }
    drop(upgraded); // The process died after the exchange, before conversion.
    let restarted = DataDirLease::acquire(&f.data).unwrap();
    let DataRootFormatOwnership::Upgraded(ownership) = &restarted.ownership else {
        panic!("lost upgraded writer fence")
    };
    let preparation = PreparedStartupMigration::load_existing(&f.data, &f.checkpoints)
        .unwrap()
        .unwrap();
    let registry = crate::bootstrap::session_dispatch::SessionDispatchRegistry::default();
    assert_eq!(
        preparation
            .resume(
                ownership.clone(),
                &f.data,
                &f.checkpoints,
                &AgentRegistry::new(),
                &Mutex::new(HashMap::new()),
                &registry,
                &str::len
            )
            .await
            .unwrap(),
        1
    );
    assert!(registry.retains_session(&f.session.id).unwrap());
    let team = registry.session_team_token(&f.session.id).unwrap();
    registry
        .with_session_team_stores(&team, |canonical, _, memory| {
            assert!(canonical.records().unwrap().is_empty());
            assert_eq!(assignments.len(), 2);
            for assignment in &assignments {
                let committed = memory
                    .committed_reference(&assignment.conversation_id)
                    .unwrap()
                    .unwrap();
                let checkpoint = memory.checkpoint(&committed).unwrap();
                assert!(checkpoint.behavior_state.is_none());
                assert_eq!(checkpoint.cumulative_token_usage.input_tokens, 1000);
            }
            Ok(())
        })
        .unwrap();
    assert!(!f.data.path().join(STARTUP_MIGRATION_FILE).exists());
    assert_eq!(
        std::fs::read(f.data.path().join("session-history/turns.v1.jsonl")).unwrap(),
        before_history
    );
    assert_eq!(
        f.checkpoints
            .capture_legacy_session_checkpoints(&f.session.id)
            .unwrap()
            .source_sha256(),
        before_checkpoints
    );
}

/// `cp -a FROM TO`, as the backup page says.
fn copy_tree(from: &std::path::Path, to: &std::path::Path) {
    assert!(std::process::Command::new("cp")
        .arg("-a")
        .arg(from)
        .arg(to)
        .status()
        .unwrap()
        .success());
}

/// A data root copied in the middle of an interrupted Session storage
/// upgrade (a restored backup, a migrated Mac) holds the same legacy history
/// and checkpoints in other files: when the upgrade stopped before it
/// converted a Session, it records the copy's identities and finishes there,
/// and the original is left as it was. Before, the record pinned the
/// original's inodes and the copy never started. One that stopped half-way
/// through a Session sealed that Session's history as the original's: the
/// copy is refused with what to do, and the original still finishes. A
/// copy whose history differs is still refused.
#[tokio::test]
async fn a_copied_interrupted_upgrade_records_its_new_location_and_finishes() {
    for half_way in [false, true] {
        copied_interrupted_upgrade_finishes(half_way).await;
    }
}

async fn copied_interrupted_upgrade_finishes(half_way: bool) {
    let mut f = source_proven_startup_fixture().await;
    let preparation = PreparedStartupMigration::prepare_source_proven(
        &f.lease.as_ref().unwrap().ownership,
        &f.data,
        &f.checkpoints,
        std::slice::from_ref(&f.session),
    )
    .unwrap();
    let (upgraded, mut preparation) = preparation
        .upgrade_held(
            f.lease.take().unwrap(),
            &f.data,
            &f.checkpoints,
            &AgentRegistry::new(),
            &Mutex::new(HashMap::new()),
        )
        .await
        .unwrap();
    if half_way {
        // The Session's journals were made and its history sealed and
        // imported, and the process died before the record said Complete.
        let DataRootFormatOwnership::Upgraded(ownership) = &upgraded.ownership else {
            unreachable!()
        };
        initialize_migration_session(
            ownership.clone(),
            &f.data,
            &preparation.sessions[0].specification,
        )
        .unwrap();
        preparation.sessions[0].state = StartupMigrationState::Started;
        preparation.persist(&f.data).unwrap();
        drop(
            migrate_held_session_state_with_mode(
                ownership.clone(),
                &f.data,
                &f.checkpoints,
                &AgentRegistry::new(),
                &Mutex::new(HashMap::new()),
                &[preparation.sessions[0].specification.clone()],
                &str::len,
                true,
            )
            .await
            .unwrap(),
        );
    }
    drop(upgraded); // The process died after the exchange.
    let original = std::fs::read(f.data.path().join(STARTUP_MIGRATION_FILE)).unwrap();

    let copies = tempfile::tempdir().unwrap();
    let copy = copies.path().join("data");
    copy_tree(f.data.path(), &copy);
    let damaged = copies.path().join("damaged");
    copy_tree(f.data.path(), &damaged);
    let history = damaged.join("session-history/turns.v1.jsonl");
    let mut changed = std::fs::read(&history).unwrap();
    changed.extend_from_slice(b"\n");
    std::fs::write(&history, changed).unwrap();

    let data = SecureDir::open(&copy).unwrap();
    let lease = DataDirLease::acquire(&data).unwrap();
    let DataRootFormatOwnership::Upgraded(ownership) = &lease.ownership else {
        panic!("lost upgraded writer fence")
    };
    let checkpoints =
        CheckpointStore::new_in_secure(&data, "checkpoints", CheckpointPolicy::Manual).unwrap();
    let doctor = super::super::AxocoatlDaemon::interrupted_upgrade_doctor_line(&copy).unwrap();
    assert_eq!(doctor.is_err(), half_way, "{doctor:?}");
    if half_way {
        let (line, hint) = doctor.unwrap_err();
        assert!(line.contains("stopped half-way"), "{line}");
        assert!(
            hint.contains("finish the upgrade where it started"),
            "{hint}"
        );
    } else {
        assert!(doctor
            .unwrap()
            .contains("records its new location and finishes it"));
    }
    assert!(
        super::super::AxocoatlDaemon::interrupted_upgrade_doctor_line(f.data.path())
            .unwrap()
            .unwrap()
            .ends_with("interrupted; the next start finishes it")
    );
    if half_way {
        let error = PreparedStartupMigration::load_existing(&data, &checkpoints)
            .err()
            .unwrap()
            .to_string();
        assert!(
            error.contains(
                "is a copy of a data directory whose Session storage upgrade stopped half-way"
            ) && error.contains(&format!("started converting Session '{}'", f.session.id))
                && error.contains("finish the upgrade where it started"),
            "{error}"
        );
        assert_eq!(
            std::fs::read(copy.join(STARTUP_MIGRATION_FILE)).unwrap(),
            original
        );
        drop(lease);
        // The original finishes.
        let lease = DataDirLease::acquire(&f.data).unwrap();
        let DataRootFormatOwnership::Upgraded(ownership) = &lease.ownership else {
            panic!("lost upgraded writer fence")
        };
        let registry = crate::bootstrap::session_dispatch::SessionDispatchRegistry::default();
        assert_eq!(
            PreparedStartupMigration::load_existing(&f.data, &f.checkpoints)
                .unwrap()
                .unwrap()
                .resume(
                    ownership.clone(),
                    &f.data,
                    &f.checkpoints,
                    &AgentRegistry::new(),
                    &Mutex::new(HashMap::new()),
                    &registry,
                    &str::len
                )
                .await
                .unwrap(),
            1
        );
        return;
    }
    let preparation = PreparedStartupMigration::load_existing(&data, &checkpoints)
        .unwrap()
        .unwrap();
    assert_eq!(preparation.root_inode, data.inode_identity().unwrap());
    let registry = crate::bootstrap::session_dispatch::SessionDispatchRegistry::default();
    assert_eq!(
        preparation
            .resume(
                ownership.clone(),
                &data,
                &checkpoints,
                &AgentRegistry::new(),
                &Mutex::new(HashMap::new()),
                &registry,
                &str::len
            )
            .await
            .unwrap(),
        1
    );
    assert!(registry.retains_session(&f.session.id).unwrap());
    assert!(!copy.join(STARTUP_MIGRATION_FILE).exists());
    assert_eq!(
        std::fs::read(f.data.path().join(STARTUP_MIGRATION_FILE)).unwrap(),
        original
    );

    let (line, hint) = super::super::AxocoatlDaemon::interrupted_upgrade_doctor_line(&damaged)
        .unwrap()
        .unwrap_err();
    assert!(
        line.contains("no longer the one it recorded") && hint.contains("Restore the backup"),
        "{line}: {hint}"
    );
    let data = SecureDir::open(&damaged).unwrap();
    let _lease = DataDirLease::acquire(&data).unwrap();
    let checkpoints =
        CheckpointStore::new_in_secure(&data, "checkpoints", CheckpointPolicy::Manual).unwrap();
    assert!(PreparedStartupMigration::load_existing(&data, &checkpoints).is_err());
    assert_eq!(
        std::fs::read(damaged.join(STARTUP_MIGRATION_FILE)).unwrap(),
        original
    );
}

/// The same preparation made again on a copy taken before the format
/// exchange (the upgrade stopped earlier) is recorded for the copy; one
/// with other contents is still refused.
#[tokio::test]
async fn a_copied_preparation_is_recorded_again_for_the_copy() {
    let f = source_proven_startup_fixture().await;
    PreparedStartupMigration::prepare_source_proven(
        &f.lease.as_ref().unwrap().ownership,
        &f.data,
        &f.checkpoints,
        std::slice::from_ref(&f.session),
    )
    .unwrap();
    drop(f.lease);
    let copies = tempfile::tempdir().unwrap();
    let copy = copies.path().join("data");
    copy_tree(f.data.path(), &copy);
    let data = SecureDir::open(&copy).unwrap();
    let lease = DataDirLease::acquire(&data).unwrap();
    let checkpoints =
        CheckpointStore::new_in_secure(&data, "checkpoints", CheckpointPolicy::Manual).unwrap();
    let preparation = PreparedStartupMigration::prepare_source_proven(
        &lease.ownership,
        &data,
        &checkpoints,
        std::slice::from_ref(&f.session),
    )
    .unwrap();
    assert_eq!(preparation.root_inode, data.inode_identity().unwrap());
    let recorded: PreparedStartupMigration =
        serde_json::from_slice(&std::fs::read(copy.join(STARTUP_MIGRATION_FILE)).unwrap()).unwrap();
    assert_eq!(recorded.root_inode, preparation.root_inode);

    let mut other = recorded.clone();
    other.history_sha256 = "0".repeat(64);
    other.root_inode = "1:1".into();
    other.persist(&data).unwrap();
    assert!(PreparedStartupMigration::prepare_source_proven(
        &lease.ownership,
        &data,
        &checkpoints,
        std::slice::from_ref(&f.session),
    )
    .is_err());
}

/// `axocoatl session record-location` records a copied root's new location
/// in its ownership record and in its interrupted upgrade's, and nothing
/// else; it refuses a directory without an ownership record, and a root a
/// daemon holds.
#[tokio::test]
async fn record_location_records_a_copied_root_and_changes_nothing_else() {
    let mut f = source_proven_startup_fixture().await;
    let preparation = PreparedStartupMigration::prepare_source_proven(
        &f.lease.as_ref().unwrap().ownership,
        &f.data,
        &f.checkpoints,
        std::slice::from_ref(&f.session),
    )
    .unwrap();
    let (upgraded, _) = preparation
        .upgrade_held(
            f.lease.take().unwrap(),
            &f.data,
            &f.checkpoints,
            &AgentRegistry::new(),
            &Mutex::new(HashMap::new()),
        )
        .await
        .unwrap();
    drop(upgraded);
    let copies = tempfile::tempdir().unwrap();
    let copy = copies.path().join("data");
    copy_tree(f.data.path(), &copy);
    let listing = |root: &std::path::Path| {
        let mut names: Vec<String> = std::fs::read_dir(root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    };
    let before = listing(&copy);
    // The digest of the path the original recorded, which its volumes carry.
    let original: serde_json::Value = serde_json::from_slice(
        &std::fs::read(copy.join(".axocoatl-daemon.lock/format.json")).unwrap(),
    )
    .unwrap();
    let original = original["root_authority_sha256"]
        .as_str()
        .unwrap()
        .to_string();
    let recorded = super::super::AxocoatlDaemon::record_data_root_location(&copy, false)
        .unwrap()
        .location
        .unwrap();
    assert!(
        recorded.contains("its new location (it is a copy at another path)")
            && recorded.contains("its interrupted Session storage upgrade"),
        "{recorded}"
    );
    // ...and the runtime authority of the data root it was copied from, as a
    // start records it.
    let copied_from = "copied-from-runtime-authorities.json";
    let mut expected = before.clone();
    expected.push(copied_from.to_string());
    expected.sort();
    assert_eq!(listing(&copy), expected);
    assert_eq!(
        serde_json::from_slice::<Vec<String>>(&std::fs::read(copy.join(copied_from)).unwrap())
            .unwrap(),
        std::slice::from_ref(&original)
    );
    let data = SecureDir::open(&copy).unwrap();
    let migration: PreparedStartupMigration =
        serde_json::from_slice(&std::fs::read(copy.join(STARTUP_MIGRATION_FILE)).unwrap()).unwrap();
    assert_eq!(migration.root_inode, data.inode_identity().unwrap());
    // Recorded: a second time records nothing.
    assert_eq!(
        super::super::AxocoatlDaemon::record_data_root_location(&copy, false).unwrap(),
        super::RecordedLocation {
            location: None,
            adopted: None,
            in_use: 0,
        }
    );
    // The data root it was copied from was deleted: its authority becomes
    // this one's previous one, as a move's.
    let adopted = super::super::AxocoatlDaemon::record_data_root_location(&copy, true).unwrap();
    assert_eq!(adopted.location, None);
    assert_eq!((adopted.adopted, adopted.in_use), (Some(1), 0));
    assert!(!copy.join(copied_from).exists());
    let data_copy = SecureDir::open(&copy).unwrap();
    assert_eq!(
        super::super::runtime_volumes::previous_runtime_authorities(&data_copy),
        std::slice::from_ref(&original)
    );
    assert_eq!(
        super::super::AxocoatlDaemon::record_data_root_location(&copy, true)
            .unwrap()
            .adopted,
        Some(0)
    );
    drop(data_copy);
    // A daemon holds the root.
    let lease = DataDirLease::acquire(&data).unwrap();
    let error = super::super::AxocoatlDaemon::record_data_root_location(&copy, false)
        .unwrap_err()
        .to_string();
    assert!(error.contains("already owned by another daemon"), "{error}");
    drop(lease);
    // A directory without an ownership record is left as it is.
    let empty = copies.path().join("empty");
    std::fs::create_dir(&empty).unwrap();
    std::fs::set_permissions(&empty, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    let error = super::super::AxocoatlDaemon::record_data_root_location(&empty, true)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("holds no Axocoatl data directory ownership record"),
        "{error}"
    );
    assert!(listing(&empty).is_empty());
    assert!(super::super::AxocoatlDaemon::record_data_root_location(
        &copies.path().join("missing"),
        false
    )
    .is_err());
}

#[tokio::test]
async fn prepared_upgrade_refuses_changed_role_mapping_or_checkpoint_source() {
    let f = source_proven_startup_fixture().await;
    let preparation = PreparedStartupMigration::prepare_source_proven(
        &f.lease.as_ref().unwrap().ownership,
        &f.data,
        &f.checkpoints,
        std::slice::from_ref(&f.session),
    )
    .unwrap();
    let mut forged = preparation.clone();
    forged.sessions[0].specification.actors[0].policy =
        LegacyActorProjectionPolicy::OrdinaryAutonomous;
    forged.persist(&f.data).unwrap();
    assert!(PreparedStartupMigration::load_existing(&f.data, &f.checkpoints).is_err());
    preparation.persist(&f.data).unwrap();
    f.checkpoints
        .save(&checkpoint(&format!("{}:lead", f.session.id), 2, true))
        .await
        .unwrap();
    assert!(PreparedStartupMigration::load_existing(&f.data, &f.checkpoints).is_err());
    assert!(!f.data.path().join("execution-v2").exists());
}

#[tokio::test]
async fn started_migration_never_recreates_missing_previously_initialized_v2_history() {
    let mut f = source_proven_startup_fixture().await;
    let mut preparation = PreparedStartupMigration::prepare_source_proven(
        &f.lease.as_ref().unwrap().ownership,
        &f.data,
        &f.checkpoints,
        std::slice::from_ref(&f.session),
    )
    .unwrap();
    let (lease, retained) = preparation
        .upgrade_held(
            f.lease.take().unwrap(),
            &f.data,
            &f.checkpoints,
            &AgentRegistry::new(),
            &Mutex::new(HashMap::new()),
        )
        .await
        .unwrap();
    preparation = retained;
    let DataRootFormatOwnership::Upgraded(ownership) = &lease.ownership else {
        unreachable!()
    };
    preparation.sessions[0].state = StartupMigrationState::Started;
    preparation.persist(&f.data).unwrap();
    let converted = migrate_held_session_state(
        ownership.clone(),
        &f.data,
        &f.checkpoints,
        &AgentRegistry::new(),
        &Mutex::new(HashMap::new()),
        &[preparation.sessions[0].specification.clone()],
        &str::len,
    )
    .await
    .unwrap();
    let canonical_path = converted[0].canonical.path();
    drop(converted);
    std::fs::remove_file(&canonical_path).unwrap();
    let registry = crate::bootstrap::session_dispatch::SessionDispatchRegistry::default();
    assert!(preparation
        .resume(
            ownership.clone(),
            &f.data,
            &f.checkpoints,
            &AgentRegistry::new(),
            &Mutex::new(HashMap::new()),
            &registry,
            &str::len
        )
        .await
        .is_err());
    assert!(!canonical_path.exists());
    assert!(f.data.path().join(STARTUP_MIGRATION_FILE).exists());
    assert!(!registry.retains_session(&f.session.id).unwrap());
}

#[tokio::test]
async fn unknown_role_upgrade_keeps_history_archive_and_usage_without_replaying_private_state() {
    for private_state in [None, Some("unproved private orchestration".to_owned())] {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let data = SecureDir::open(root.path()).unwrap();
        let lease = DataDirLease::acquire(&data).unwrap();
        let mut sessions =
            axocoatl_session::SessionStore::new_in_secure(&data, "sessions").unwrap();
        let session = sessions
            .create_with_environment(
                "Historical solo",
                "workspace",
                workspace.path(),
                axocoatl_session::SessionMode::SingleAgent {
                    agent_id: "solo".into(),
                },
                vec![],
                vec![],
                None,
                None,
                false,
                true,
            )
            .unwrap();
        let mut ledger = SessionTurnStore::open_in_secure(&data, "session-history").unwrap();
        begin(&mut ledger, &session.id, "original", true, true);
        let history_bytes = std::fs::read(ledger.path()).unwrap();
        drop(ledger);
        let checkpoints =
            CheckpointStore::new_in_secure(&data, "checkpoints", CheckpointPolicy::Manual).unwrap();
        let mut original = checkpoint(&format!("{}:solo", session.id), 1, false);
        original.behavior_state = private_state;
        checkpoints.save(&original).await.unwrap();
        let capture = checkpoints
            .capture_legacy_session_checkpoints(&session.id)
            .unwrap();
        let source_bytes = capture.archive_bytes().to_vec();
        let preparation = PreparedStartupMigration::prepare_source_proven(
            &lease.ownership,
            &data,
            &checkpoints,
            std::slice::from_ref(&session),
        )
        .unwrap();
        assert_eq!(
            preparation.sessions[0].specification.actors[0].policy,
            LegacyActorProjectionPolicy::UnknownRoleHistoryOnly
        );
        let assignments =
            deterministic_assignments(&preparation.sessions[0].specification).unwrap();
        let (lease, _) = preparation
            .upgrade_held(
                lease,
                &data,
                &checkpoints,
                &AgentRegistry::new(),
                &Mutex::new(HashMap::new()),
            )
            .await
            .unwrap();
        drop(lease);
        let lease = DataDirLease::acquire(&data).unwrap();
        let DataRootFormatOwnership::Upgraded(ownership) = &lease.ownership else {
            panic!("lost writer fence")
        };
        let registry = crate::bootstrap::session_dispatch::SessionDispatchRegistry::default();
        PreparedStartupMigration::load_existing(&data, &checkpoints)
            .unwrap()
            .unwrap()
            .resume(
                ownership.clone(),
                &data,
                &checkpoints,
                &AgentRegistry::new(),
                &Mutex::new(HashMap::new()),
                &registry,
                &str::len,
            )
            .await
            .unwrap();
        let team = registry.session_team_token(&session.id).unwrap();
        registry
            .with_session_team_stores(&team, |canonical, content, memory| {
                let assignment = &assignments[0];
                let baseline = memory
                    .legacy_baseline_checkpoint(&assignment.conversation_id)
                    .unwrap()
                    .unwrap();
                assert!(baseline.session_messages.is_empty());
                assert!(baseline.behavior_state.is_none());
                assert_eq!(
                    baseline.cumulative_token_usage,
                    original.cumulative_token_usage
                );
                assert!(!baseline.cumulative_token_usage_known);
                assert_eq!(
                    memory
                        .legacy_checkpoint_source_archive(&assignment.conversation_id)
                        .unwrap()
                        .unwrap(),
                    source_bytes
                );
                let seal = canonical.legacy_seal().unwrap().unwrap();
                let history = content.read_legacy_history(&seal).unwrap();
                assert_eq!(history.turns.len(), 1);
                assert_eq!(history.turns[0].status, SessionTurnLifecycle::Completed);
                assert_eq!(
                    history.turns[0].agent_outputs[0].output,
                    "solo own completed answer"
                );
                assert!(memory
                    .rewind_session(
                        canonical,
                        content,
                        Some("original"),
                        std::slice::from_ref(&assignment.conversation_id),
                        ToolReplayPolicy::CompleteNativeGroups
                    )
                    .is_err());
                Ok(())
            })
            .unwrap();
        assert_eq!(
            std::fs::read(data.path().join("session-history/turns.v1.jsonl")).unwrap(),
            history_bytes
        );
        assert_eq!(
            checkpoints
                .capture_legacy_session_checkpoints(&session.id)
                .unwrap()
                .archive_bytes(),
            source_bytes
        );
    }
}
