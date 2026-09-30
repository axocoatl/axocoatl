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
