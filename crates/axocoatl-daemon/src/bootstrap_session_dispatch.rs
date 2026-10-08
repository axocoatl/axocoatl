//! Daemon lifetime for registered schema-2 repository work. The registry keeps
//! canonical ownership even when every external waiter disappears. This does
//! not enable upgraded-root startup or install a second execution ingress.

use super::{session_repository::SessionRepositoryOwner, AxocoatlDaemon};
use crate::error::DaemonError;
use crate::session_dispatch::{SessionDispatchController, SuccessorTurn};
use axocoatl_memory::activation_state::PromotionManifest;
use axocoatl_session::execution_store::DurableSessionIdentity;
use axocoatl_session::turn_contract::{EvidenceRef, LogicalTurnId};
use std::collections::{HashMap, HashSet};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::Duration;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

#[path = "bootstrap_session_pending.rs"]
mod pending;
pub(crate) use pending::PendingSessionToken;
#[path = "bootstrap_session_team_registry.rs"]
mod team;
pub(crate) use team::SessionTeamToken;
#[path = "bootstrap_session_native_turn.rs"]
mod native_turn;
pub(crate) use native_turn::NativeFirstTurnExisting;

type Result<T> = std::result::Result<T, DaemonError>;

/// Absence inside retained canonical history is distinct from a legacy host.
pub(crate) enum RegisteredControlPlane {
    NotRegistered,
    MissingTurn,
    Found(Box<crate::session_control_plane::SessionTurnControlPlane>),
}

impl RegisteredControlPlane {
    /// The selected canonical result is authoritative even when it is empty.
    /// Invoke the legacy reader only for an actually legacy, unregistered host.
    pub(super) async fn resolve_with_legacy<F, Fut>(
        self,
        ownership: &axocoatl_session::execution_ownership::DataRootFormatOwnership,
        legacy: F,
    ) -> Result<Option<crate::session_control_plane::SessionTurnControlPlane>>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<
            Output = Result<Option<crate::session_control_plane::SessionTurnControlPlane>>,
        >,
    {
        match self {
            Self::Found(view) => Ok(Some(*view)),
            Self::MissingTurn => Ok(None),
            Self::NotRegistered => {
                if matches!(
                    ownership,
                    axocoatl_session::execution_ownership::DataRootFormatOwnership::Upgraded(_)
                ) {
                    return Err(failure("upgraded Session history is not retained by this host; legacy history cannot substitute for it"));
                }
                legacy().await
            }
        }
    }
}

fn failure(message: impl Into<String>) -> DaemonError {
    DaemonError::SessionConflict(message.into())
}

/// What a Session's lifecycle cleanup waits for, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CleanupWait {
    /// Another lifecycle action of the same Session.
    Cleanup,
    /// The Session's registered executions settling.
    Executions,
    /// The Workspace, which another Session's turn or another operation of
    /// the Workspace holds.
    Workspace,
    /// The Session's own turn reaching a safe point after Stop.
    Boundary,
}

/// The refusal of a lifecycle action whose cleanup did not finish within
/// `timeout`: what it waited for, and who holds the Workspace when that was
/// it. The Session itself is not blamed for another Session's turn.
fn timeout_message(
    session_id: &str,
    stage: CleanupWait,
    timeout: Duration,
    holders: &[String],
) -> String {
    let waited = match stage {
        CleanupWait::Cleanup => "another lifecycle action of this Session to finish".to_string(),
        CleanupWait::Executions => "this Session's executions to settle".to_string(),
        CleanupWait::Workspace if holders.is_empty() => {
            "its Workspace, which another operation of the Workspace holds".to_string()
        }
        CleanupWait::Workspace => format!(
            "its Workspace, which {} holds; let that turn finish or stop it",
            holders.join("; ")
        ),
        CleanupWait::Boundary => "this Session's stopped turn to reach a safe point".to_string(),
    };
    format!(
        "Session {session_id} was not changed: its cleanup waited {} s for {waited}. Its \
         controller and Workspace ownership remain retained; retry the action",
        timeout.as_secs()
    )
}

/// Only this module constructs the token. The controller retains a Weak token;
/// the registry entry holds both its strong token and strong canonical owner.
pub(crate) struct RepositoryRegistrationGate {
    identity: DurableSessionIdentity,
    open: AtomicBool,
}

impl RepositoryRegistrationGate {
    pub(crate) fn permits(&self, identity: &DurableSessionIdentity) -> bool {
        self.open.load(Ordering::SeqCst) && &self.identity == identity
    }

    pub(crate) fn close(&self) {
        self.open.store(false, Ordering::SeqCst);
    }
}

struct RegisteredEntry {
    controller: SessionDispatchController,
    owner: Mutex<SessionRepositoryOwner>,
    gate: Mutex<Arc<RepositoryRegistrationGate>>,
    /// A retained closure/promotion proof, never an idle-status guess.
    between_turns: Mutex<Option<PromotionManifest>>,
    reference: Mutex<Option<EvidenceRef>>,
    cleanup: Arc<AsyncMutex<()>>,
    /// The registry keeps the actual Workspace mutex after process retirement.
    /// Lifecycle waiters receive shared holders, never its sole ownership.
    operation: Mutex<Option<Arc<OwnedMutexGuard<()>>>>,
    retired: AtomicBool,
}

impl RegisteredEntry {
    fn owner(&self) -> Result<SessionRepositoryOwner> {
        self.owner
            .lock()
            .map(|owner| owner.clone())
            .map_err(|_| failure("registered repository owner failed"))
    }
}

/// Process-local exact-entry token. It grants no dispatch, may be dropped, and
/// must be revalidated after asynchronous Workspace/runtime acquisition.
pub(crate) struct RepositoryReacquisition {
    session_id: String,
    entry: Arc<RegisteredEntry>,
    finalized: PromotionManifest,
    prior_owner: SessionRepositoryOwner,
}

#[derive(Default)]
struct RegistryState {
    closed: bool,
    closing_sessions: HashSet<String>,
    entries: HashMap<String, Arc<RegisteredEntry>>,
    pending: HashMap<String, Arc<pending::PendingSessionEntry>>,
}

#[derive(Default)]
pub(crate) struct SessionDispatchRegistry {
    hooks: Option<Arc<axocoatl_tools::HookRegistry>>,
    state: Mutex<RegistryState>,
    #[cfg(test)]
    fail_reacquisition_ack: AtomicBool,
}

pub(crate) struct SessionDispatchCleanup {
    session_id: String,
    entry: Option<Arc<RegisteredEntry>>,
    pending: Option<Arc<pending::PendingSessionEntry>>,
    operation: Option<SessionDispatchOperation>,
    _cleanup: Option<Arc<OwnedMutexGuard<()>>>,
}

impl SessionDispatchCleanup {
    pub(crate) fn take_operation(&mut self) -> Option<SessionDispatchOperation> {
        self.operation.take()
    }
}

/// How a Session's lifecycle cleanup has the Session's Workspace.
pub(crate) enum CleanupWorkspace {
    /// Wait for it, and for another lifecycle action of the Session, within
    /// the cleanup's timeout (daemon shutdown, several Ways).
    Wait,
    /// A lifecycle action a person waits on (Close, Delete, an environment
    /// change): it never waits for the Workspace or for another lifecycle
    /// action of the Session. It holds the Workspace already when it could
    /// take it.
    Lifecycle(Option<OwnedMutexGuard<()>>),
}

/// The detail of a lifecycle action's cleanup refused because another
/// operation holds the Session's Workspace between turns; the daemon names
/// that operation in its refusal.
pub(crate) const LIFECYCLE_WORKSPACE_HELD: &str = "another operation holds the Session's Workspace";

fn lifecycle_workspace_held() -> DaemonError {
    DaemonError::WorkspaceBusy(LIFECYCLE_WORKSPACE_HELD.into())
}

/// Holds the actual Workspace gate and, for registered work, the exact cleanup
/// serialization gate. Dropping a failed/cancelled caller leaves the registry's
/// parked Workspace owner intact. This token makes no process-settlement claim.
pub(crate) struct SessionDispatchOperation {
    _operation: Arc<OwnedMutexGuard<()>>,
    _cleanup: Option<Arc<OwnedMutexGuard<()>>>,
}

impl From<OwnedMutexGuard<()>> for SessionDispatchOperation {
    fn from(operation: OwnedMutexGuard<()>) -> Self {
        Self {
            _operation: Arc::new(operation),
            _cleanup: None,
        }
    }
}

impl SessionDispatchRegistry {
    pub(crate) fn with_hooks(hooks: Arc<axocoatl_tools::HookRegistry>) -> Self {
        Self {
            hooks: Some(hooks),
            ..Default::default()
        }
    }

    pub(crate) fn retains_session(&self, session_id: &str) -> Result<bool> {
        let state = self
            .state
            .lock()
            .map_err(|_| failure("Session dispatch registry failed"))?;
        Ok(state.entries.contains_key(session_id) || state.pending.contains_key(session_id))
    }

    /// Whether `session_id`'s own registration holds its Workspace
    /// operation: its registered (or first, pending) repository owner, while
    /// its turn is open or its first turn is being prepared, or the
    /// operation a lifecycle action that did not finish parked for its retry.
    pub(crate) fn holds_workspace(&self, session_id: &str) -> Result<bool> {
        let state = self
            .state
            .lock()
            .map_err(|_| failure("Session dispatch registry failed"))?;
        let parked = |operation: &Mutex<Option<Arc<OwnedMutexGuard<()>>>>| {
            operation
                .lock()
                .map(|operation| operation.is_some())
                .unwrap_or(true)
        };
        if let Some(entry) = state.entries.get(session_id) {
            let held = if entry.retired.load(Ordering::SeqCst) {
                parked(&entry.operation)
            } else {
                entry.owner()?.holds_workspace_operation()
            };
            if held {
                return Ok(true);
            }
        }
        if let Some(pending) = state.pending.get(session_id) {
            let held = if pending.retired.load(Ordering::SeqCst) {
                parked(&pending.operation)
            } else {
                pending.holds_workspace_operation()?
            };
            if held {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub(crate) fn live_native_turns(&self) -> Result<Vec<(String, String)>> {
        let state = self
            .state
            .lock()
            .map_err(|_| failure("Session dispatch registry failed"))?;
        let mut live = Vec::new();
        for (session_id, entry) in &state.entries {
            if entry.retired.load(Ordering::SeqCst) {
                continue;
            }
            if let Some(turn) = entry
                .controller
                .live_owned_turn()
                .map_err(|error| failure(error.to_string()))?
            {
                live.push((session_id.clone(), turn.as_str().to_owned()));
            }
        }
        Ok(live)
    }

    pub(crate) fn history_snapshot(
        &self,
        session_id: &str,
    ) -> Result<Option<axocoatl_session::session_history::SessionHistory>> {
        let state = self
            .state
            .lock()
            .map_err(|_| failure("Session dispatch registry failed"))?;
        if let Some(entry) = state.pending.get(session_id) {
            return entry.history_snapshot().map(Some);
        }
        state
            .entries
            .get(session_id)
            .map(|entry| {
                entry
                    .controller
                    .history_snapshot()
                    .map_err(|error| failure(error.to_string()))
            })
            .transpose()
    }

    /// The network record namespace of a retained native Session, for its
    /// single writer. Refused while the Session is closing or closed, so a
    /// released Session's directory lock is never re-taken by its record.
    pub(crate) fn network_record_namespace(
        &self,
        session_id: &str,
    ) -> Result<axocoatl_session::execution_namespace::OwnedExecutionNamespace> {
        let state = self
            .state
            .lock()
            .map_err(|_| failure("Session dispatch registry failed"))?;
        if state.closed || state.closing_sessions.contains(session_id) {
            return Err(failure(
                "the Session is closing; its network record cannot be opened",
            ));
        }
        if let Some(entry) = state.pending.get(session_id) {
            return entry.network_record_namespace();
        }
        let entry = state
            .entries
            .get(session_id)
            .ok_or_else(|| failure("the Session has no retained native history in this daemon"))?;
        entry
            .controller
            .network_record_namespace()
            .map_err(|error| failure(error.to_string()))
    }

    /// Read a retained Session's network record without opening a writer.
    /// `Ok(None)` when the Session has no record or no native history here.
    pub(crate) fn read_network_record(
        &self,
        session_id: &str,
        after: Option<u64>,
        limit: usize,
    ) -> Result<
        Option<(
            Vec<axocoatl_session::network_record::NetworkLine>,
            axocoatl_session::network_record::RecordStats,
        )>,
    > {
        let state = self
            .state
            .lock()
            .map_err(|_| failure("Session dispatch registry failed"))?;
        if let Some(entry) = state.pending.get(session_id) {
            return entry.read_network_record(after, limit);
        }
        match state.entries.get(session_id) {
            Some(entry) => entry
                .controller
                .read_network_record(after, limit)
                .map_err(|error| failure(error.to_string())),
            None => Ok(None),
        }
    }

    /// Read one screenshot kept beside a retained Session's network record.
    pub(crate) fn read_network_screenshot(
        &self,
        session_id: &str,
        sha256: &str,
    ) -> Result<Option<(String, Vec<u8>)>> {
        let state = self
            .state
            .lock()
            .map_err(|_| failure("Session dispatch registry failed"))?;
        if let Some(entry) = state.pending.get(session_id) {
            return entry.read_network_screenshot(sha256);
        }
        match state.entries.get(session_id) {
            Some(entry) => entry
                .controller
                .read_network_screenshot(sha256)
                .map_err(|error| failure(error.to_string())),
            None => Ok(None),
        }
    }

    /// Human requests enter through the authenticated host and share the same
    /// registry fence as Close/Delete/shutdown. No caller-supplied source tag
    /// or cached read capability can grant an operation.
    #[cfg(test)]
    pub(crate) fn submit_human_action(
        &self,
        session_id: &str,
        turn_id: &str,
        request: crate::session_dispatch::HumanControlActionRequest,
        issued_at_ms: u64,
    ) -> Result<axocoatl_session::control_command::CommandReceiptView> {
        self.submit_human_action_with_context(session_id, turn_id, request, issued_at_ms, None)
    }
    pub(crate) fn submit_human_action_with_context(
        &self,
        session_id: &str,
        turn_id: &str,
        request: crate::session_dispatch::HumanControlActionRequest,
        issued_at_ms: u64,
        prepared: Option<crate::session_dispatch::human_context::PreparedHumanControlContext>,
    ) -> Result<axocoatl_session::control_command::CommandReceiptView> {
        if request.session_id.as_str() != session_id || request.turn_id.as_str() != turn_id {
            return Err(failure(
                "control request belongs to another Session or turn",
            ));
        }
        let state = self
            .state
            .lock()
            .map_err(|_| failure("Session dispatch registry failed"))?;
        if state.closed || state.closing_sessions.contains(session_id) {
            return Err(failure("Session lifecycle admission is closed"));
        }
        let entry = state
            .entries
            .get(session_id)
            .ok_or_else(|| failure("This Session has no active command controller."))?;
        if entry.retired.load(Ordering::SeqCst) {
            return Err(failure("Session execution ownership has been retired"));
        }
        let snapshot = entry
            .controller
            .snapshot()
            .map_err(|error| failure(error.to_string()))?;
        if snapshot.turn_id().as_str() != turn_id {
            return Err(failure("control targets an earlier Session turn"));
        }
        entry
            .controller
            .submit_human_action_with_context(request, issued_at_ms, prepared)
            .map_err(|error| failure(error.to_string()))
    }

    /// Compatibility Stop ingress. The registry fence stays held through the
    /// canonical request/cancellation. None means an actually unregistered host.
    pub(crate) fn request_human_turn_stop(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> Result<Option<bool>> {
        let state = self
            .state
            .lock()
            .map_err(|_| failure("Session dispatch registry failed"))?;
        if state.closed || state.closing_sessions.contains(session_id) {
            return Err(failure("Session lifecycle admission is closed"));
        }
        let Some(entry) = state.entries.get(session_id) else {
            if state.pending.contains_key(session_id) {
                return Err(failure("This retained Session has no active turn to stop"));
            }
            return Ok(None);
        };
        if entry.retired.load(Ordering::SeqCst) {
            return Err(failure("Session execution ownership has been retired"));
        }
        let snapshot = entry
            .controller
            .snapshot()
            .map_err(|error| failure(error.to_string()))?;
        if snapshot.owner().session_id.as_str() != session_id
            || snapshot.turn_id().as_str() != turn_id
        {
            return Err(failure("Stop targets another Session or an earlier turn"));
        }
        entry
            .controller
            .request_human_turn_stop(session_id, turn_id)
            .map(|receipt| Some(receipt.first_request()))
            .map_err(|error| failure(error.to_string()))
    }

    /// Reads and controls resolve the same retained controller by exact owner.
    /// A previous turn or a different Session cannot address current work.
    #[cfg(test)]
    pub(crate) fn control_plane(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> Result<Option<crate::session_control_plane::SessionTurnControlPlane>> {
        Ok(match self.lookup_control_plane(session_id, turn_id)? {
            RegisteredControlPlane::Found(view) => Some(*view),
            RegisteredControlPlane::NotRegistered | RegisteredControlPlane::MissingTurn => None,
        })
    }

    pub(crate) fn lookup_control_plane(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> Result<RegisteredControlPlane> {
        let state = self
            .state
            .lock()
            .map_err(|_| failure("Session dispatch registry failed"))?;
        let Some(entry) = state.entries.get(session_id) else {
            return match state.pending.get(session_id) {
                Some(entry) => entry.control_plane(turn_id),
                None => Ok(RegisteredControlPlane::NotRegistered),
            };
        };
        Ok(
            match entry
                .controller
                .control_plane_for_turn(turn_id)
                .map_err(|error| failure(error.to_string()))?
            {
                Some(view) => RegisteredControlPlane::Found(Box::new(view)),
                None => RegisteredControlPlane::MissingTurn,
            },
        )
    }

    /// Serialize the admission fence with the controller's final dispatch gate.
    /// The registry mutex never stays held across an async backend operation.
    fn close_entry(entry: &RegisteredEntry) -> Result<()> {
        let controller = entry
            .controller
            .close_registered_repository_admission()
            .map_err(|error| failure(error.to_string()));
        let owner = entry.owner()?.request_supervised_stop();
        controller.and(owner.map(|_| ()))
    }

    pub(crate) fn close_all_admission(&self) -> Result<()> {
        let (entries, pending) = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| failure("Session dispatch registry failed"))?;
            state.closed = true;
            (
                state.entries.values().cloned().collect::<Vec<_>>(),
                state.pending.values().cloned().collect::<Vec<_>>(),
            )
        };
        let mut failures = Vec::new();
        for entry in pending {
            if let Err(error) = entry.close_admission() {
                failures.push(error.to_string());
            }
        }
        for entry in entries {
            if let Err(error) = Self::close_entry(&entry) {
                failures.push(format!("{}: {error}", entry.owner()?.metadata().session_id));
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(failure(format!(
                "Session dispatch admission cleanup failed: {}",
                failures.join("; ")
            )))
        }
    }

    /// The registered Sessions other than `except` whose live turn holds
    /// `gate` (their Workspace's operation gate), named for a refusal.
    fn workspace_turns_holding(&self, gate: &Arc<AsyncMutex<()>>, except: &str) -> Vec<String> {
        let Ok(state) = self.state.lock() else {
            return Vec::new();
        };
        let mut holders = Vec::new();
        for (session_id, entry) in &state.entries {
            if session_id == except || entry.retired.load(Ordering::SeqCst) {
                continue;
            }
            let shares_gate = entry
                .owner()
                .is_ok_and(|owner| Arc::ptr_eq(&owner.workspace_gate(), gate));
            if !shares_gate {
                continue;
            }
            if let Ok(Some(turn)) = entry.controller.live_owned_turn() {
                holders.push(format!(
                    "Session {session_id}, whose turn {} is running",
                    turn.as_str()
                ));
            }
        }
        holders.sort();
        holders
    }

    /// Called by actual Close/Delete/shutdown before waiting on Workspace.
    /// A previously retired owner reuses its parked Workspace gate. None means
    /// there is no remaining registered owner; the legacy lock path then applies.
    pub(crate) async fn prepare_session_cleanup(
        &self,
        session_id: &str,
        timeout: Duration,
    ) -> Result<SessionDispatchCleanup> {
        self.prepare_session_cleanup_with(session_id, timeout, CleanupWorkspace::Wait)
            .await
    }

    /// [`Self::prepare_session_cleanup`], with how the cleanup has the
    /// Session's Workspace. A lifecycle action a person waits on
    /// ([`CleanupWorkspace::Lifecycle`]) waits only for the Session's own
    /// turn to reach a safe point: another lifecycle action of the Session
    /// in progress is refused as busy at once, and so is, between turns, a
    /// Workspace that another operation holds ([`LIFECYCLE_WORKSPACE_HELD`]);
    /// both refusals change nothing. The Workspace the action already holds
    /// is parked for a retained owner, or returned as the cleanup's
    /// operation when the Session has no registered owner.
    pub(crate) async fn prepare_session_cleanup_with(
        &self,
        session_id: &str,
        timeout: Duration,
        workspace: CleanupWorkspace,
    ) -> Result<SessionDispatchCleanup> {
        let (lifecycle, mut held) = match workspace {
            CleanupWorkspace::Wait => (false, None),
            CleanupWorkspace::Lifecycle(held) => (true, held),
        };
        let (entry, pending) = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| failure("Session dispatch registry failed"))?;
            if !lifecycle {
                state.closing_sessions.insert(session_id.to_owned());
            }
            (
                state.entries.get(session_id).cloned(),
                state.pending.get(session_id).cloned(),
            )
        };
        // A lifecycle action takes what it would otherwise wait for before
        // it changes anything, so a refusal leaves the Session as it was.
        let mut taken_cleanup = None;
        if lifecycle {
            let cleanup_gate = match (&pending, &entry) {
                (Some(pending), _) => Some(pending.cleanup.clone()),
                (None, Some(entry)) => Some(entry.cleanup.clone()),
                (None, None) => None,
            };
            if let Some(gate) = cleanup_gate {
                taken_cleanup = Some(gate.try_lock_owned().map_err(|_| {
                    DaemonError::WorkspaceBusy(format!(
                        "another Close, Delete or environment change of Session {session_id} is \
                         in progress. Try again once it has ended"
                    ))
                })?);
            }
            if let (None, Some(entry), None) = (&pending, &entry, &held) {
                let between_turns = entry
                    .between_turns
                    .lock()
                    .map_err(|_| failure("released repository identity failed"))?
                    .is_some();
                if between_turns && !entry.retired.load(Ordering::SeqCst) {
                    let gate = entry.owner()?.workspace_gate();
                    held = Some(
                        gate.try_lock_owned()
                            .map_err(|_| lifecycle_workspace_held())?,
                    );
                }
            }
        }
        if lifecycle {
            self.state
                .lock()
                .map_err(|_| failure("Session dispatch registry failed"))?
                .closing_sessions
                .insert(session_id.to_owned());
        }
        if let Some(pending) = pending {
            return self
                .prepare_pending_cleanup(session_id, pending, timeout, taken_cleanup, held)
                .await;
        }
        let Some(entry) = entry else {
            return Ok(SessionDispatchCleanup {
                session_id: session_id.to_owned(),
                entry: None,
                pending: None,
                operation: held.map(SessionDispatchOperation::from),
                _cleanup: None,
            });
        };
        Self::close_entry(&entry)?;
        // What the lifecycle action is waiting for, so a timeout says so.
        let waiting = Mutex::new(CleanupWait::Cleanup);
        let wait_for = |stage: CleanupWait| {
            *waiting
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = stage;
        };
        let prepared = tokio::time::timeout(timeout, async {
            let cleanup = Arc::new(match taken_cleanup {
                Some(cleanup) => cleanup,
                None => entry.cleanup.clone().lock_owned().await,
            });
            // Another lifecycle may have completed while this one waited. It
            // must not reuse an old entry to act on a later registration.
            {
                let state = self
                    .state
                    .lock()
                    .map_err(|_| failure("Session dispatch registry failed"))?;
                match state.entries.get(session_id) {
                    Some(current) if !Arc::ptr_eq(current, &entry) => {
                        return Err(failure("Session registration changed while cleanup waited"))
                    }
                    None => return Ok((None, held)),
                    Some(_) => {}
                }
            }
            if !entry.retired.load(Ordering::SeqCst) {
                wait_for(CleanupWait::Executions);
                entry
                    .controller
                    .wait_for_registered_executions(timeout)
                    .await
                    .map_err(|error| failure(error.to_string()))?;
                let owner = entry.owner()?;
                let between_turns = entry
                    .between_turns
                    .lock()
                    .map_err(|_| failure("released repository identity failed"))?
                    .is_some();
                let operation = if between_turns {
                    // No process work remains in this permanently retired owner.
                    // Acquire the actual Workspace mutex, then park it below
                    // without an intervening await. A cancelled Close keeps it.
                    // A lifecycle action took it before anything changed.
                    wait_for(CleanupWait::Workspace);
                    match held.take() {
                        Some(guard) => {
                            let gate = owner.workspace_gate();
                            if !Arc::ptr_eq(OwnedMutexGuard::mutex(&guard), &gate) {
                                return Err(failure("the action holds another Workspace"));
                            }
                            guard
                        }
                        None if lifecycle => owner
                            .workspace_gate()
                            .try_lock_owned()
                            .map_err(|_| lifecycle_workspace_held())?,
                        None => owner.workspace_gate().lock_owned().await,
                    }
                } else {
                    if held.is_some() {
                        // The owner holds the Workspace until its turn ends,
                        // so a lifecycle action cannot hold it too.
                        return Err(failure("the Session's owner lost its Workspace"));
                    }
                    wait_for(CleanupWait::Boundary);
                    owner.wait_for_execution_boundary(timeout).await?;
                    if owner.execution_is_idle()? {
                        owner.retire_idle()?
                    } else {
                        owner.cleanup_for_lifecycle().await?
                    }
                };
                // No await separates receipt of the actual mutex guard from
                // parking it in the retained entry. Cancellation cannot land
                // in a window where only the lifecycle future owns the gate.
                *entry
                    .operation
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::new(operation));
                entry.retired.store(true, Ordering::SeqCst);
            }
            let operation = entry
                .operation
                .lock()
                .map_err(|_| failure("parked Workspace ownership failed"))?
                .clone()
                .ok_or_else(|| failure("retired Session registration lost its Workspace gate"))?;
            Ok::<_, DaemonError>((Some((operation, cleanup)), None))
        })
        .await
        .map_err(|_| {
            let stage = *waiting
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let holders = match (stage, entry.owner()) {
                (CleanupWait::Workspace, Ok(owner)) => {
                    self.workspace_turns_holding(&owner.workspace_gate(), session_id)
                }
                _ => Vec::new(),
            };
            failure(timeout_message(session_id, stage, timeout, &holders))
        })??;
        let (prepared, held) = prepared;
        let Some((operation, cleanup)) = prepared else {
            return Ok(SessionDispatchCleanup {
                session_id: session_id.to_owned(),
                entry: None,
                pending: None,
                operation: held.map(SessionDispatchOperation::from),
                _cleanup: None,
            });
        };
        Ok(SessionDispatchCleanup {
            session_id: session_id.to_owned(),
            entry: Some(entry),
            pending: None,
            operation: Some(SessionDispatchOperation {
                _operation: operation,
                _cleanup: Some(cleanup.clone()),
            }),
            _cleanup: Some(cleanup),
        })
    }

    /// Remove only an explicitly retired entry after the real lifecycle action
    /// succeeds. A failed or cancelled Close keeps the canonical controller for
    /// retry, even if process cleanup itself already finished successfully.
    pub(crate) fn complete_session_cleanup(&self, cleanup: &SessionDispatchCleanup) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| failure("Session dispatch registry failed"))?;
        if let Some(entry) = state.pending.get(&cleanup.session_id) {
            if !cleanup
                .pending
                .as_ref()
                .is_some_and(|expected| Arc::ptr_eq(expected, entry))
                || !entry.retired.load(Ordering::SeqCst)
            {
                return Err(failure(
                    "pending cleanup completion belongs to another or unretired Session",
                ));
            }
            entry.release_stores_after_cleanup()?;
            entry
                .operation
                .lock()
                .map_err(|_| failure("pending Workspace ownership failed"))?
                .take();
            state.pending.remove(&cleanup.session_id);
        }
        if let Some(entry) = state.entries.get(&cleanup.session_id) {
            if !cleanup
                .entry
                .as_ref()
                .is_some_and(|expected| Arc::ptr_eq(expected, entry))
            {
                return Err(failure(
                    "cleanup completion belongs to another Session registration",
                ));
            }
            if !entry.retired.load(Ordering::SeqCst) {
                return Err(failure(
                    "Session dispatch still owns active or unknown repository execution",
                ));
            }
            entry
                .gate
                .lock()
                .map_err(|_| failure("registration gate failed"))?
                .close();
            // Existing lifecycle holders keep the real gate until their scope
            // ends. Removing this anchor is allowed only after explicit success.
            entry
                .operation
                .lock()
                .map_err(|_| failure("parked Workspace ownership failed"))?
                .take();
            state.entries.remove(&cleanup.session_id);
        }
        Ok(())
    }

    /// Reject a retained cleanup before Open waits on the Workspace gate it
    /// deliberately holds. Reopen repeats this check under its actual lock.
    pub(crate) fn require_session_reopenable(&self, session_id: &str) -> Result<()> {
        let state = self
            .state
            .lock()
            .map_err(|_| failure("Session dispatch registry failed"))?;
        if state.closed
            || state.entries.contains_key(session_id)
            || state
                .pending
                .get(session_id)
                .is_some_and(|entry| !entry.closed_history.load(Ordering::SeqCst))
        {
            return Err(failure("Session retains unfinished dispatch cleanup"));
        }
        Ok(())
    }

    /// Explicit Reopen may permit a fresh registration after successful Close.
    pub(crate) fn reopen_session(&self, session_id: &str) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| failure("Session dispatch registry failed"))?;
        if state.closed
            || state.entries.contains_key(session_id)
            || state
                .pending
                .get(session_id)
                .is_some_and(|entry| !entry.closed_history.load(Ordering::SeqCst))
        {
            return Err(failure("Session retains unfinished dispatch cleanup"));
        }
        if let Some(entry) = state.pending.get(session_id) {
            entry.closed_history.store(false, Ordering::SeqCst);
        }
        state.closing_sessions.remove(session_id);
        Ok(())
    }

    pub(crate) fn forget_deleted_session(&self, session_id: &str) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| failure("Session dispatch registry failed"))?;
        if state.entries.contains_key(session_id) || state.pending.contains_key(session_id) {
            return Err(failure(
                "deleted Session still retains repository execution ownership",
            ));
        }
        state.closing_sessions.remove(session_id);
        Ok(())
    }

    /// Complete only the physical portion of a finalized turn. History,
    /// command receipts and canonical ownership remain registered for reads.
    pub(crate) fn release_after_turn(
        &self,
        session_id: &str,
        turn_id: &LogicalTurnId,
    ) -> Result<()> {
        let state = self
            .state
            .lock()
            .map_err(|_| failure("Session dispatch registry failed"))?;
        if state.closed || state.closing_sessions.contains(session_id) {
            return Err(failure("Session lifecycle admission is closed"));
        }
        let entry = state
            .entries
            .get(session_id)
            .ok_or_else(|| failure("Session has no retained dispatch controller"))?;
        if entry.retired.load(Ordering::SeqCst) {
            return Err(failure(
                "Session cleanup has already retired repository ownership",
            ));
        }
        let mut released = entry
            .between_turns
            .lock()
            .map_err(|_| failure("released repository identity failed"))?;
        if let Some(expected) = &*released {
            let snapshot = entry
                .controller
                .snapshot()
                .map_err(|error| failure(error.to_string()))?;
            if snapshot.turn_id() != turn_id
                || entry
                    .controller
                    .finalized_repository_identity()
                    .map_err(|error| failure(error.to_string()))?
                    != *expected
            {
                return Err(failure(
                    "repository release belongs to another finalized turn",
                ));
            }
            return Ok(());
        }
        let owner = entry.owner()?;
        let (finalized, operation) = entry
            .controller
            .release_finalized_repository(turn_id, &owner)
            .map_err(|error| failure(error.to_string()))?;
        *released = Some(finalized);
        // Retirement and recording the exact released state are synchronous.
        // Only now can the real mutex pass to an ordinary participating writer.
        drop(operation);
        Ok(())
    }

    pub(crate) fn prepare_reacquisition(
        &self,
        session_id: &str,
    ) -> Result<RepositoryReacquisition> {
        let state = self
            .state
            .lock()
            .map_err(|_| failure("Session dispatch registry failed"))?;
        if state.closed || state.closing_sessions.contains(session_id) {
            return Err(failure("Session lifecycle admission is closed"));
        }
        let entry = state
            .entries
            .get(session_id)
            .ok_or_else(|| failure("Session has no retained dispatch controller"))?
            .clone();
        if entry.retired.load(Ordering::SeqCst) {
            return Err(failure(
                "Session cleanup has already retired repository ownership",
            ));
        }
        let finalized = entry
            .between_turns
            .lock()
            .map_err(|_| failure("released repository identity failed"))?
            .clone()
            .ok_or_else(|| failure("Session still retains its repository operation"))?;
        if entry
            .controller
            .finalized_repository_identity()
            .map_err(|error| failure(error.to_string()))?
            != finalized
        {
            return Err(failure("released Session closure changed"));
        }
        let prior_owner = entry.owner()?;
        Ok(RepositoryReacquisition {
            session_id: session_id.to_owned(),
            entry,
            finalized,
            prior_owner,
        })
    }

    pub(crate) fn complete_reacquisition(
        &self,
        token: RepositoryReacquisition,
        owner: SessionRepositoryOwner,
    ) -> Result<EvidenceRef> {
        let state = self
            .state
            .lock()
            .map_err(|_| failure("Session dispatch registry failed"))?;
        if state.closed || state.closing_sessions.contains(&token.session_id) {
            return Err(failure(
                "Session lifecycle admission closed during reacquisition",
            ));
        }
        let entry = state
            .entries
            .get(&token.session_id)
            .filter(|entry| Arc::ptr_eq(entry, &token.entry))
            .ok_or_else(|| failure("Session registration changed during reacquisition"))?;
        if entry.retired.load(Ordering::SeqCst)
            || entry
                .between_turns
                .lock()
                .map_err(|_| failure("released repository identity failed"))?
                .as_ref()
                != Some(&token.finalized)
            || !entry.owner()?.same_owner(&token.prior_owner)
            || entry
                .controller
                .finalized_repository_identity()
                .map_err(|error| failure(error.to_string()))?
                != token.finalized
            || !owner.is_fresh_reacquisition_of(&token.prior_owner)
            || !owner.execution_is_idle()?
        {
            return Err(failure(
                "reacquisition differs from the exact released Session resource",
            ));
        }
        let gate = Arc::new(RepositoryRegistrationGate {
            identity: owner.identity().clone(),
            open: AtomicBool::new(true),
        });
        // Retain first. Any subsequent storage failure keeps canonical and
        // actual Workspace ownership together for checked cleanup/recovery.
        {
            // Acquire all fallible bookkeeping guards before changing state.
            // A failure cannot label a newly retained owner as already released.
            let mut held = entry
                .owner
                .lock()
                .map_err(|_| failure("registered repository owner failed"))?;
            let mut registered_gate = entry
                .gate
                .lock()
                .map_err(|_| failure("registration gate failed"))?;
            let mut released = entry
                .between_turns
                .lock()
                .map_err(|_| failure("released repository identity failed"))?;
            let mut reference = entry
                .reference
                .lock()
                .map_err(|_| failure("repository registration reference failed"))?;
            *held = owner.clone();
            *registered_gate = gate.clone();
            *released = None;
            *reference = None;
        }
        entry
            .controller
            .install_repository_registration(&gate)
            .map_err(|error| failure(error.to_string()))?;
        let reference = entry
            .controller
            .retain_repository_resource(owner)
            .map_err(|error| failure(error.to_string()))?;
        *entry
            .reference
            .lock()
            .map_err(|_| failure("repository registration reference failed"))? =
            Some(reference.clone());
        #[cfg(test)]
        if self.fail_reacquisition_ack.swap(false, Ordering::SeqCst) {
            return Err(failure(
                "injected lost repository-registration acknowledgement",
            ));
        }
        entry
            .controller
            .enable_reacquired_repository()
            .map_err(|error| failure(error.to_string()))?;
        Ok(reference)
    }

    #[cfg(test)]
    pub(crate) fn fail_next_reacquisition_ack_for_test(&self) {
        self.fail_reacquisition_ack.store(true, Ordering::SeqCst);
    }
}

impl AxocoatlDaemon {
    /// Called only from the local authenticated command channel. The path and
    /// request identities are checked before any request evidence is retained.
    pub async fn submit_session_control_action(
        &self,
        session_id: &str,
        turn_id: &str,
        request: crate::session_dispatch::HumanControlActionRequest,
    ) -> Result<axocoatl_session::control_command::CommandReceiptView> {
        self.require_runtime_admission()?;
        if self.get_session(session_id).await.is_none() {
            return Err(failure(format!("session '{session_id}' not found")));
        }
        if request.session_id.as_str() != session_id || request.turn_id.as_str() != turn_id {
            return Err(failure(
                "control request belongs to another Session or turn",
            ));
        }
        let token = self
            .session_dispatch_lifecycles
            .session_team_token(session_id)?;
        if let Some(receipt) = self
            .session_dispatch_lifecycles
            .repeated_session_human_action(&token, &request)?
        {
            return Ok(receipt);
        }
        let issued_at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|error| failure(error.to_string()))?
            .as_millis()
            .try_into()
            .map_err(|_| failure("control request timestamp exceeds its bound"))?;
        if let Some(receipt) = self
            .session_dispatch_lifecycles
            .finish_pending_without_result(&request, issued_at_ms)?
        {
            return Ok(receipt);
        }
        let prepared = if let Some(context) = &request.context {
            let session = self
                .get_session(session_id)
                .await
                .ok_or_else(|| failure("Session is missing"))?;
            let (_, attachments, begin) = self
                .prepare_session_turn_context(
                    &session,
                    request.command_id.as_str(),
                    request.instruction.as_deref().unwrap_or_default(),
                    &context.attachment_ids,
                    &context.references,
                    None,
                    None,
                )
                .await?;
            Some(
                crate::session_dispatch::human_context::PreparedHumanControlContext {
                    original: context.clone(),
                    references: begin.context,
                    attachments,
                },
            )
        } else {
            None
        };
        self.ensure_registered_native_session(session_id).await?;
        let receipt = self
            .session_dispatch_lifecycles
            .submit_human_action_with_context(
                session_id,
                turn_id,
                request.clone(),
                issued_at_ms,
                prepared,
            )?;
        self.drive_applied_native_control(&request, &receipt)
            .await?;
        Ok(receipt)
    }

    /// Rejoin the same validated Ready resource after an ordinary writer had
    /// access between finalized turns. Changes to environment/runtime identity
    /// are refused and require their own existing lifecycle integration.
    pub(crate) async fn reacquire_session_dispatch_repository(
        &self,
        session_id: &str,
    ) -> Result<EvidenceRef> {
        self.require_runtime_admission()?;
        let token = self
            .session_dispatch_lifecycles
            .prepare_reacquisition(session_id)?;
        self.validate_repository_daemon_binding(&token.prior_owner)?;
        let owner = token.prior_owner.reacquire_between_turns().await?;
        self.require_runtime_admission()?;
        self.validate_repository_daemon_binding(&owner)?;
        self.session_dispatch_lifecycles
            .complete_reacquisition(token, owner)
    }
}
