//! Explicit pre-actor-start conversion of real legacy Session stores.
//!
//! The caller closes ingress and joins the old runtime before acquiring/using
//! this seam. It converts stores only; it starts no actor or executor.
//! The caller retains format ownership across failures so a partially imported
//! Session can reopen and repeat the same deterministic conversion.

use super::{reconcile_checkpoint_transactions_from_ledger, ActiveSessionTurn};
use crate::error::DaemonError;
use axocoatl_actor::AgentRegistry;
use axocoatl_core::SecureDir;
use axocoatl_memory::activation_state::{
    ActivationStateStore, LegacyActorProjectionPolicy, LegacyBaselineProjection,
    LegacyRoleAssignment,
};
use axocoatl_memory::legacy_conversation::ToolReplayPolicy;
use axocoatl_memory::CheckpointStore;
use axocoatl_session::execution_content::ExecutionContentStore;
use axocoatl_session::execution_namespace::ExecutionComponent;
use axocoatl_session::execution_ownership::{DataRootFormatOwnership, UpgradedFormatOwnership};
use axocoatl_session::execution_store::{
    DurableLegacySeal, ExecutionStoreOwner, SessionExecutionStore,
};
use axocoatl_session::turn_contract::{NodeConversationId, SessionId, SessionTeamSlotId};
use axocoatl_session::SessionTurnStore;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::Mutex;

type Result<T> = std::result::Result<T, DaemonError>;

/// Historical ownership supplied by the migration caller, never inferred from
/// a renamed or removed current Agent template.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LegacyActorMigration {
    pub checkpoint_agent_id: String,
    pub recorded_agent_id: String,
    pub policy: LegacyActorProjectionPolicy,
    pub tool_replay_policy: ToolReplayPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LegacySessionMigration {
    pub session_id: String,
    pub workspace_id: String,
    pub actors: Vec<LegacyActorMigration>,
}

pub(crate) struct MigratedSessionState {
    pub canonical: SessionExecutionStore,
    pub content: ExecutionContentStore,
    pub activation_state: ActivationStateStore,
    pub seal: DurableLegacySeal,
}

/// Run before the explicit root-format conversion and again at the store join.
/// A stopped status is insufficient: old registry and active-turn ownership
/// must have been retired through the daemon's checked lifecycle first.
pub(super) async fn require_migration_quiescence(
    actors: &AgentRegistry,
    active_turns: &Mutex<HashMap<String, ActiveSessionTurn>>,
) -> Result<()> {
    if !active_turns.lock().await.is_empty() || !actors.list_ids().await.is_empty() {
        return Err(failure("legacy execution still owns actors or active turns; finish checked shutdown before migration"));
    }
    Ok(())
}

/// Convert after explicit format upgrade while its actual kernel-backed owner
/// remains held by the caller. There is no provider dispatch or automatic
/// orphan replay. A real running legacy row is refused until the existing
/// orphan-reconciliation path has durably settled it. The daemon converts
/// through `PreparedStartupMigration`; tests call this directly.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(super) async fn migrate_held_session_state(
    ownership: Arc<UpgradedFormatOwnership>,
    data_root: &SecureDir,
    checkpoint_store: &CheckpointStore,
    actors: &AgentRegistry,
    active_turns: &Mutex<HashMap<String, ActiveSessionTurn>>,
    specifications: &[LegacySessionMigration],
    count_text: &dyn Fn(&str) -> usize,
) -> Result<Vec<MigratedSessionState>> {
    migrate_held_session_state_with_mode(
        ownership,
        data_root,
        checkpoint_store,
        actors,
        active_turns,
        specifications,
        count_text,
        false,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn migrate_held_session_state_with_mode(
    ownership: Arc<UpgradedFormatOwnership>,
    data_root: &SecureDir,
    checkpoint_store: &CheckpointStore,
    actors: &AgentRegistry,
    active_turns: &Mutex<HashMap<String, ActiveSessionTurn>>,
    specifications: &[LegacySessionMigration],
    count_text: &dyn Fn(&str) -> usize,
    existing_only: bool,
) -> Result<Vec<MigratedSessionState>> {
    require_migration_quiescence(actors, active_turns).await?;
    DataRootFormatOwnership::Upgraded(ownership.clone())
        .verify_root(data_root)
        .map_err(|error| failure(error.to_string()))?;
    checkpoint_store
        .verify_legacy_data_root(data_root)
        .map_err(|error| failure(error.to_string()))?;
    let mut sessions = HashSet::new();
    for spec in specifications {
        if !sessions.insert(spec.session_id.as_str()) || spec.workspace_id.is_empty() {
            return Err(failure(
                "migration requires distinct Sessions with recorded Workspace identity",
            ));
        }
        SessionId::new(&spec.session_id).map_err(|error| failure(error.to_string()))?;
    }
    // Reopen from the exact owned root, not a caller-provided in-memory fold.
    // The existing-only check prevents manufacturing an empty source ledger.
    let history_root = data_root
        .existing_child("session-history")
        .map_err(|error| failure(error.to_string()))?;
    history_root
        .open_file_limited("turns.v1.jsonl", 256 * 1024 * 1024)
        .map_err(|error| failure(error.to_string()))?;
    let legacy = Mutex::new(
        SessionTurnStore::open_in_secure(data_root, "session-history")
            .map_err(|error| failure(error.to_string()))?,
    );
    {
        let old = legacy.lock().await;
        for spec in specifications {
            if old
                .list_including_superseded(&spec.session_id)
                .iter()
                .any(|turn| !turn.status.is_terminal())
            {
                return Err(failure("legacy canonical History still has unfinished work; reconcile its exact terminal before conversion"));
            }
        }
    }
    // A crash can leave a Completed canonical row and a pending checkpoint
    // promotion. Resolve the exact real transaction before taking its source
    // snapshot so completed per-agent context and all usage survive together.
    for spec in specifications {
        reconcile_checkpoint_transactions_from_ledger(
            checkpoint_store,
            &legacy,
            Some(&spec.session_id),
        )
        .await?;
    }
    require_migration_quiescence(actors, active_turns).await?;
    let mut converted = Vec::with_capacity(specifications.len());
    for spec in specifications {
        let assignments = deterministic_assignments(spec)?;
        let owner = ExecutionStoreOwner {
            workspace_id: spec.workspace_id.clone(),
            session_id: SessionId::new(&spec.session_id)
                .map_err(|error| failure(error.to_string()))?,
        };
        let mut canonical = if existing_only {
            SessionExecutionStore::open_existing(ownership.clone(), owner)
        } else {
            SessionExecutionStore::open(ownership.clone(), owner)
        }
        .map_err(|error| failure(error.to_string()))?;
        canonical
            .verify_data_root(data_root)
            .map_err(|error| failure(error.to_string()))?;
        if canonical.record_count() != 0 {
            return Err(failure(
                "migration cannot replace a Session that already has v2 work",
            ));
        }
        let mut content = ExecutionContentStore::open_owned(
            if existing_only {
                canonical.existing_component_namespace(
                    ExecutionComponent::ExecutionContent,
                    std::path::Path::new("execution-content.v1.json"),
                )
            } else {
                canonical.component_namespace(ExecutionComponent::ExecutionContent)
            }
            .map_err(|error| failure(error.to_string()))?,
        )
        .map_err(|error| failure(error.to_string()))?;
        let seal = if let Some(seal) = canonical
            .legacy_seal()
            .map_err(|error| failure(error.to_string()))?
        {
            // read_legacy_history verifies the durable content identity and
            // source; reusing the existing seal never recaptures new v1 rows.
            content
                .read_legacy_history(&seal)
                .map_err(|error| failure(error.to_string()))?;
            seal
        } else {
            let history = canonical
                .legacy_history_snapshot()
                .map_err(|error| failure(error.to_string()))?;
            let retained = content
                .retain_legacy_history(&history)
                .map_err(|error| failure(error.to_string()))?;
            canonical
                .seal_legacy_history(&retained)
                .map_err(|error| failure(error.to_string()))?
        };
        let checkpoint_snapshot = checkpoint_store
            .capture_legacy_session_checkpoints(&spec.session_id)
            .map_err(|error| failure(error.to_string()))?;
        let projections = LegacyBaselineProjection::from_captured_session(
            &content,
            &seal,
            checkpoint_store,
            checkpoint_snapshot,
            &assignments,
            count_text,
        )
        .map_err(|error| failure(error.to_string()))?;
        let mut activation_state = ActivationStateStore::open_owned(
            if existing_only {
                canonical.existing_component_namespace(
                    ExecutionComponent::ActivationState,
                    std::path::Path::new("activation-state.json"),
                )
            } else {
                canonical.component_namespace(ExecutionComponent::ActivationState)
            }
            .map_err(|error| failure(error.to_string()))?,
        )
        .map_err(|error| failure(error.to_string()))?;
        for projection in &projections {
            activation_state
                .import_legacy_baseline(&canonical, projection)
                .map_err(|error| failure(error.to_string()))?;
        }
        converted.push(MigratedSessionState {
            canonical,
            content,
            activation_state,
            seal,
        });
    }
    Ok(converted)
}

const STARTUP_MIGRATION_FILE: &str = "execution-migration.v1.json";
const STARTUP_MIGRATION_MAX_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum StartupMigrationState {
    Prepared,
    Started,
    Complete,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct StartupMigrationSession {
    state: StartupMigrationState,
    specification: LegacySessionMigration,
    checkpoint_source_sha256: String,
}

/// This bounded durable record is written before the format exchange while the
/// legacy lease is still held. It permits an interrupted conversion to continue
/// only against its exact original history and captured checkpoint sources.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PreparedStartupMigration {
    schema_version: u32,
    root_inode: String,
    history_inode: String,
    history_sha256: String,
    sessions: Vec<StartupMigrationSession>,
}

#[cfg(unix)]
fn migration_history_identity(data_root: &SecureDir) -> Result<(String, String)> {
    use std::os::unix::fs::MetadataExt;
    let history = data_root
        .existing_child("session-history")
        .map_err(|error| failure(error.to_string()))?;
    let file = history
        .open_file_limited("turns.v1.jsonl", 256 * 1024 * 1024)
        .map_err(|error| failure(error.to_string()))?;
    let metadata = file
        .metadata()
        .map_err(|error| failure(error.to_string()))?;
    let bytes = history
        .read_limited("turns.v1.jsonl", 256 * 1024 * 1024)
        .map_err(|error| failure(error.to_string()))?;
    let actual = history
        .open_file_limited("turns.v1.jsonl", 256 * 1024 * 1024)
        .map_err(|error| failure(error.to_string()))?
        .metadata()
        .map_err(|error| failure(error.to_string()))?;
    if (metadata.dev(), metadata.ino()) != (actual.dev(), actual.ino()) {
        return Err(failure("legacy history identity changed during capture"));
    }
    Ok((
        format!("{}:{}", metadata.dev(), metadata.ino()),
        format!("{:x}", Sha256::digest(bytes)),
    ))
}

#[cfg(not(unix))]
fn migration_history_identity(_: &SecureDir) -> Result<(String, String)> {
    Err(failure(
        "source-bound migration requires Unix file identity",
    ))
}

impl PreparedStartupMigration {
    fn verify_sources(&self, data_root: &SecureDir, checkpoints: &CheckpointStore) -> Result<()> {
        data_root
            .verify_ambient_identity()
            .map_err(|error| failure(error.to_string()))?;
        checkpoints
            .verify_legacy_data_root(data_root)
            .map_err(|error| failure(error.to_string()))?;
        if self.schema_version != 1
            || self.root_inode
                != data_root
                    .inode_identity()
                    .map_err(|error| failure(error.to_string()))?
            || migration_history_identity(data_root)?
                != (self.history_inode.clone(), self.history_sha256.clone())
        {
            return Err(failure("prepared migration no longer matches its exact original data root and legacy history"));
        }
        data_root
            .existing_child("sessions")
            .map_err(|error| failure(error.to_string()))?;
        let mut recorded_sessions =
            axocoatl_session::SessionStore::new_in_secure(data_root, "sessions")
                .map_err(|error| failure(error.to_string()))?;
        recorded_sessions
            .load_all()
            .map_err(|error| failure(error.to_string()))?;
        if recorded_sessions.list().len() != self.sessions.len() {
            return Err(failure(
                "prepared migration does not cover the exact retained Session owners",
            ));
        }
        let legacy = SessionTurnStore::open_read_only_in_secure(data_root, "session-history")
            .map_err(|error| failure(error.to_string()))?;
        let mut sessions = HashSet::new();
        for session in &self.sessions {
            if !sessions.insert(&session.specification.session_id) {
                return Err(failure("prepared migration duplicates a Session"));
            }
            deterministic_assignments(&session.specification)?;
            if recorded_sessions
                .get(&session.specification.session_id)
                .is_none_or(|record| record.workspace_id != session.specification.workspace_id)
            {
                return Err(failure(
                    "prepared migration Session owner differs from its durable record",
                ));
            }
            let source = checkpoints
                .capture_legacy_session_checkpoints(&session.specification.session_id)
                .map_err(|error| failure(error.to_string()))?;
            if source_proven_actors(
                &session.specification.session_id,
                &legacy.list_including_superseded(&session.specification.session_id),
                &source,
            )? != session.specification.actors
            {
                return Err(failure(
                    "prepared migration role mapping differs from its recorded source evidence",
                ));
            }
            if source.source_sha256() != session.checkpoint_source_sha256 {
                return Err(failure(format!(
                    "Session '{}' checkpoint source changed after migration preparation",
                    session.specification.session_id
                )));
            }
        }
        Ok(())
    }

    pub(super) fn load_existing(
        data_root: &SecureDir,
        checkpoints: &CheckpointStore,
    ) -> Result<Option<Self>> {
        let bytes =
            match data_root.read_limited(STARTUP_MIGRATION_FILE, STARTUP_MIGRATION_MAX_BYTES) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(failure(error.to_string())),
            };
        let preparation: Self =
            serde_json::from_slice(&bytes).map_err(|error| failure(error.to_string()))?;
        preparation.verify_sources(data_root, checkpoints)?;
        Ok(Some(preparation))
    }

    /// Call only after role-free orphan and checkpoint transaction settlement,
    /// before any legacy checkpoint projection based on present-day Settings.
    pub(super) fn prepare_source_proven(
        ownership: &DataRootFormatOwnership,
        data_root: &SecureDir,
        checkpoints: &CheckpointStore,
        sessions: &[axocoatl_session::Session],
    ) -> Result<Self> {
        ownership
            .verify_root(data_root)
            .map_err(|error| failure(error.to_string()))?;
        if !matches!(ownership, DataRootFormatOwnership::Legacy(_)) {
            return Err(failure(
                "new migration preparation requires the original held legacy writer boundary",
            ));
        }
        let (history_inode, history_sha256) = migration_history_identity(data_root)?;
        let legacy = SessionTurnStore::open_read_only_in_secure(data_root, "session-history")
            .map_err(|error| failure(error.to_string()))?;
        let mut prepared = Vec::with_capacity(sessions.len());
        for session in sessions {
            let captured = checkpoints
                .capture_legacy_session_checkpoints(&session.id)
                .map_err(|error| failure(error.to_string()))?;
            let turns = legacy.list_including_superseded(&session.id);
            if turns.iter().any(|turn| !turn.status.is_terminal()) {
                return Err(failure(format!(
                    "Session '{}' still has unfinished legacy history",
                    session.id
                )));
            }
            let actors = source_proven_actors(&session.id, &turns, &captured)?;
            prepared.push(StartupMigrationSession {
                state: StartupMigrationState::Prepared,
                specification: LegacySessionMigration {
                    session_id: session.id.clone(),
                    workspace_id: session.workspace_id.clone(),
                    actors,
                },
                checkpoint_source_sha256: captured.source_sha256().into(),
            });
        }
        prepared.sort_by(|a, b| a.specification.session_id.cmp(&b.specification.session_id));
        let preparation = Self {
            schema_version: 1,
            root_inode: data_root
                .inode_identity()
                .map_err(|error| failure(error.to_string()))?,
            history_inode,
            history_sha256,
            sessions: prepared,
        };
        preparation.verify_sources(data_root, checkpoints)?;
        let bytes = serde_json::to_vec(&preparation).map_err(|error| failure(error.to_string()))?;
        if bytes.len() > STARTUP_MIGRATION_MAX_BYTES {
            return Err(failure("migration preparation exceeds its retention bound"));
        }
        match data_root.read_limited(STARTUP_MIGRATION_FILE, STARTUP_MIGRATION_MAX_BYTES) {
            Ok(existing) if existing != bytes => {
                return Err(failure(
                    "an existing migration preparation has different source or role assignments",
                ))
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => data_root
                .atomic_write(STARTUP_MIGRATION_FILE, &bytes)
                .map_err(|error| failure(error.to_string()))?,
            Err(error) => return Err(failure(error.to_string())),
        }
        Ok(preparation)
    }

    pub(super) async fn upgrade_held(
        self,
        lease: super::DataDirLease,
        data_root: &SecureDir,
        checkpoints: &CheckpointStore,
        actors: &AgentRegistry,
        active_turns: &Mutex<HashMap<String, ActiveSessionTurn>>,
    ) -> Result<(super::DataDirLease, Self)> {
        require_migration_quiescence(actors, active_turns).await?;
        lease
            .ownership
            .verify_root(data_root)
            .map_err(|error| failure(error.to_string()))?;
        self.verify_sources(data_root, checkpoints)?;
        // No await or lease release separates source validation from exchange.
        let ownership = lease
            .ownership
            .into_upgraded()
            .map_err(|error| failure(error.to_string()))?;
        Ok((
            super::DataDirLease {
                ownership: DataRootFormatOwnership::Upgraded(ownership),
            },
            self,
        ))
    }

    fn persist(&self, data_root: &SecureDir) -> Result<()> {
        let bytes = serde_json::to_vec(self).map_err(|error| failure(error.to_string()))?;
        if bytes.len() > STARTUP_MIGRATION_MAX_BYTES {
            return Err(failure("migration preparation exceeds its retention bound"));
        }
        data_root
            .atomic_write(STARTUP_MIGRATION_FILE, &bytes)
            .map_err(|error| failure(error.to_string()))
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn resume(
        mut self,
        ownership: Arc<UpgradedFormatOwnership>,
        data_root: &SecureDir,
        checkpoint_store: &CheckpointStore,
        actors: &AgentRegistry,
        active_turns: &Mutex<HashMap<String, ActiveSessionTurn>>,
        registry: &super::session_dispatch::SessionDispatchRegistry,
        count_text: &dyn Fn(&str) -> usize,
    ) -> Result<usize> {
        require_migration_quiescence(actors, active_turns).await?;
        self.verify_sources(data_root, checkpoint_store)?;
        let mut migrated = Vec::with_capacity(self.sessions.len());
        for index in 0..self.sessions.len() {
            let existing_only = self.sessions[index].state != StartupMigrationState::Prepared;
            if !existing_only {
                // Started promises that all three primary journals really exist.
                // A crash before this durable milestone stays Prepared and may
                // finish initialization against the exact unchanged legacy source.
                // Existing journal/initialization markers still refuse a missing
                // previously established primary; no source history is synthesized.
                initialize_migration_session(
                    ownership.clone(),
                    data_root,
                    &self.sessions[index].specification,
                )?;
                self.sessions[index].state = StartupMigrationState::Started;
                self.persist(data_root)?;
            }
            let specification = self.sessions[index].specification.clone();
            let mut current = migrate_held_session_state_with_mode(
                ownership.clone(),
                data_root,
                checkpoint_store,
                actors,
                active_turns,
                &[specification],
                count_text,
                true,
            )
            .await?;
            self.sessions[index].state = StartupMigrationState::Complete;
            self.persist(data_root)?;
            migrated.append(&mut current);
        }
        let count = migrated.len();
        let mut sessions = axocoatl_session::SessionStore::new_in_secure(data_root, "sessions")
            .map_err(|error| failure(error.to_string()))?;
        sessions
            .load_all()
            .map_err(|error| failure(error.to_string()))?;
        for stores in migrated {
            let session_id = stores.canonical.owner().session_id.as_str().to_owned();
            registry.retain_migrated_session(&mut Some(stores))?;
            if sessions
                .get(&session_id)
                .is_some_and(|session| session.status == axocoatl_session::SessionStatus::Closed)
            {
                registry.fence_closed_history(&session_id)?;
            }
        }
        // All owned stores now contain their sealed frontier and immutable
        // baselines. A restart after this removal uses ordinary v2 recovery.
        data_root
            .remove_file(STARTUP_MIGRATION_FILE)
            .map_err(|error| failure(error.to_string()))?;
        data_root
            .sync_all()
            .map_err(|error| failure(error.to_string()))?;
        Ok(count)
    }
}

/// Establish only the empty owned journals before promising existing-only
/// recovery. Repeating a Prepared initialization reopens original identities;
/// store initialization markers reject deletion of established primary files.
fn initialize_migration_session(
    ownership: Arc<UpgradedFormatOwnership>,
    data_root: &SecureDir,
    specification: &LegacySessionMigration,
) -> Result<()> {
    let canonical = SessionExecutionStore::open(
        ownership,
        ExecutionStoreOwner {
            workspace_id: specification.workspace_id.clone(),
            session_id: SessionId::new(&specification.session_id)
                .map_err(|e| failure(e.to_string()))?,
        },
    )
    .map_err(|e| failure(e.to_string()))?;
    canonical
        .verify_data_root(data_root)
        .map_err(|e| failure(e.to_string()))?;
    if canonical.record_count() != 0
        || canonical
            .native_origin()
            .map_err(|e| failure(e.to_string()))?
            .is_some()
    {
        return Err(failure(
            "migration initialization cannot replace native work",
        ));
    }
    let _content = ExecutionContentStore::open_owned(
        canonical
            .component_namespace(ExecutionComponent::ExecutionContent)
            .map_err(|e| failure(e.to_string()))?,
    )
    .map_err(|e| failure(e.to_string()))?;
    let _memory = ActivationStateStore::open_owned(
        canonical
            .component_namespace(ExecutionComponent::ActivationState)
            .map_err(|e| failure(e.to_string()))?,
    )
    .map_err(|e| failure(e.to_string()))?;
    Ok(())
}

/// Podman being absent or dormant is not itself unsettled Session ownership.
/// The caller already performed exact physical cleanup. Only a retained local
/// runtime/creation identity (or unfinished legacy preparation) needs that
/// deferred backend reconciliation before the format can be exchanged.
fn require_migration_runtime_cleanup(
    config: &axocoatl_config::AxocoatlConfig,
    sessions: &[axocoatl_session::Session],
    local_cleanup_pending: bool,
) -> Result<()> {
    if !local_cleanup_pending {
        return Ok(());
    }
    let default_local = config.sandbox.backend != "e2b";
    if sessions.iter().any(|session| {
        super::is_locally_attributable_session(session, default_local)
            && (session
                .environment
                .runtime
                .as_ref()
                .is_some_and(|runtime| !runtime.cleanup_confirmed)
                || session.environment.runtime_creation.is_some()
                || matches!(
                    session.environment.state,
                    axocoatl_session::SessionEnvironmentState::Ready
                        | axocoatl_session::SessionEnvironmentState::Preparing
                ))
    }) {
        return Err(failure(
            "finish the retained local runtime cleanup before upgrading",
        ));
    }
    Ok(())
}

fn source_proven_actors(
    session_id: &str,
    turns: &[axocoatl_session::SessionTurn],
    captured: &axocoatl_memory::LegacySessionCheckpointSnapshot,
) -> Result<Vec<LegacyActorMigration>> {
    let prefix = format!("{session_id}:");
    let checkpoints = captured.checkpoints().collect::<Vec<_>>();
    let mut actors = Vec::with_capacity(checkpoints.len());
    for checkpoint in &checkpoints {
        let scoped = checkpoint
            .agent_id
            .strip_prefix(&prefix)
            .ok_or_else(|| failure("checkpoint has a different Session owner"))?;
        let worker = scoped.rsplit_once(":worker:");
        let recorded = worker.map_or(scoped, |(_, worker)| worker);
        let owns_workers = checkpoints.iter().any(|other| {
            other
                .agent_id
                .starts_with(&format!("{}:worker:", checkpoint.agent_id))
        });
        let recorded_team_member = turns.iter().any(|turn| {
            matches!(
                turn.metadata
                    .get("mode")
                    .and_then(serde_json::Value::as_str),
                Some("lattice" | "custom")
            ) && turn
                .agent_outputs
                .iter()
                .any(|output| output.attempt_id.is_none() && output.agent_id == recorded)
        });
        let (policy, tool_replay_policy) = if worker.is_some() {
            (
                LegacyActorProjectionPolicy::CompletedPerAgent,
                ToolReplayPolicy::CompleteNativeGroups,
            )
        } else if owns_workers {
            (
                LegacyActorProjectionPolicy::CompletedPerAgent,
                ToolReplayPolicy::OmitNativeGroups,
            )
        } else if recorded_team_member {
            (
                LegacyActorProjectionPolicy::CompletedPerAgent,
                ToolReplayPolicy::CompleteNativeGroups,
            )
        } else {
            (
                LegacyActorProjectionPolicy::UnknownRoleHistoryOnly,
                ToolReplayPolicy::OmitNativeGroups,
            )
        };
        actors.push(LegacyActorMigration {
            checkpoint_agent_id: checkpoint.agent_id.clone(),
            recorded_agent_id: recorded.into(),
            policy,
            tool_replay_policy,
        });
    }
    let recorded = actors
        .iter()
        .map(|actor| actor.recorded_agent_id.as_str())
        .collect::<HashSet<_>>();
    if turns
        .iter()
        .flat_map(|turn| {
            turn.agent_id.iter().map(String::as_str).chain(
                turn.agent_outputs
                    .iter()
                    .filter(|output| output.attempt_id.is_none())
                    .map(|output| output.agent_id.as_str()),
            )
        })
        .any(|agent| !recorded.contains(agent))
    {
        return Err(failure(format!("Session '{session_id}' has historical Agent output without its exact checkpoint/accounting source")));
    }
    Ok(actors)
}

fn deterministic_assignments(spec: &LegacySessionMigration) -> Result<Vec<LegacyRoleAssignment>> {
    let mut assignments = Vec::with_capacity(spec.actors.len());
    for actor in &spec.actors {
        let encoded = serde_json::to_vec(&(&spec.session_id, &actor.checkpoint_agent_id))
            .map_err(|error| failure(error.to_string()))?;
        let identity = format!("{:x}", Sha256::digest(encoded));
        assignments.push(LegacyRoleAssignment {
            checkpoint_agent_id: actor.checkpoint_agent_id.clone(),
            recorded_agent_id: actor.recorded_agent_id.clone(),
            slot_id: SessionTeamSlotId::new(format!("legacy-slot-{identity}"))
                .map_err(|error| failure(error.to_string()))?,
            conversation_id: NodeConversationId::new(format!("legacy-conversation-{identity}"))
                .map_err(|error| failure(error.to_string()))?,
            policy: actor.policy,
            tool_replay_policy: actor.tool_replay_policy,
        });
    }
    assignments.sort_by(|left, right| left.checkpoint_agent_id.cmp(&right.checkpoint_agent_id));
    Ok(assignments)
}

fn failure(message: impl Into<String>) -> DaemonError {
    DaemonError::Session(format!("Session execution migration: {}", message.into()))
}

impl super::AxocoatlDaemon {
    /// Explicit offline conversion. Holding the normal data-root leases prevents
    /// racing a daemon; no provider, actor, automation or webhook is started.
    /// The caller must explain the historical-context boundary before invoking.
    pub async fn upgrade_session_storage(config: axocoatl_config::AxocoatlConfig) -> Result<usize> {
        let directory = std::env::var("AXOCOATL_DATA_DIR")
            .map_err(|_| failure("the explicit data directory is required"))?;
        let data = SecureDir::open(&directory).map_err(|e| failure(e.to_string()))?;
        super::admit_and_restrict_data_root(&data)?;
        let (lease, _, _, cleanup_pending) =
            Self::acquire_data_dir_lease_and_reconcile(&config, &data).await?;
        let checkpoints = CheckpointStore::new_in_secure(
            &data,
            "checkpoints",
            axocoatl_memory::CheckpointPolicy::Manual,
        )
        .map_err(|e| failure(e.to_string()))?;
        let actors = AgentRegistry::new();
        let active = Mutex::new(HashMap::new());
        let registry = super::session_dispatch::SessionDispatchRegistry::default();
        let counter =
            axocoatl_token::ApproximateCounter::new().map_err(|e| failure(e.to_string()))?;
        let count = |text: &str| axocoatl_token::TokenCounter::count_text(&counter, text);
        if let DataRootFormatOwnership::Upgraded(ownership) = &lease.ownership {
            let mut sessions = axocoatl_session::SessionStore::new_in_secure(&data, "sessions")
                .map_err(|e| failure(e.to_string()))?;
            sessions.load_all().map_err(|e| failure(e.to_string()))?;
            require_migration_runtime_cleanup(&config, &sessions.list(), cleanup_pending)?;
            return match PreparedStartupMigration::load_existing(&data, &checkpoints)? {
                Some(prepared) => {
                    prepared
                        .resume(
                            ownership.clone(),
                            &data,
                            &checkpoints,
                            &actors,
                            &active,
                            &registry,
                            &count,
                        )
                        .await
                }
                None => Ok(0),
            };
        }
        let mut sessions = axocoatl_session::SessionStore::new_in_secure(&data, "sessions")
            .map_err(|e| failure(e.to_string()))?;
        sessions.load_all().map_err(|e| failure(e.to_string()))?;
        require_migration_runtime_cleanup(&config, &sessions.list(), cleanup_pending)?;
        let mut workspaces = axocoatl_session::WorkspaceStore::new_in_secure(&data, "workspaces")
            .map_err(|e| failure(e.to_string()))?;
        workspaces.load_all().map_err(|e| failure(e.to_string()))?;
        super::migrate_sessions_to_workspaces(&mut sessions, &mut workspaces)
            .map_err(|e| failure(e.to_string()))?;
        let mut legacy = SessionTurnStore::open_in_secure(&data, "session-history")
            .map_err(|e| failure(e.to_string()))?;
        legacy
            .reconcile_orphaned_running(
                "The old executor stopped before storage upgrade; effects are not replayed.",
            )
            .map_err(|e| failure(e.to_string()))?;
        let legacy = Mutex::new(legacy);
        reconcile_checkpoint_transactions_from_ledger(&checkpoints, &legacy, None).await?;
        drop(legacy);
        let preparation = PreparedStartupMigration::prepare_source_proven(
            &lease.ownership,
            &data,
            &checkpoints,
            &sessions.list(),
        )?;
        let (lease, preparation) = preparation
            .upgrade_held(lease, &data, &checkpoints, &actors, &active)
            .await?;
        let DataRootFormatOwnership::Upgraded(ownership) = &lease.ownership else {
            unreachable!("upgrade_held always returns upgraded ownership")
        };
        preparation
            .resume(
                ownership.clone(),
                &data,
                &checkpoints,
                &actors,
                &active,
                &registry,
                &count,
            )
            .await
    }
}

#[cfg(all(test, unix))]
#[path = "bootstrap_session_migration_tests.rs"]
mod tests;
