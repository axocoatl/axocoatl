//! Host-minted ownership of a Session's actual shared repository runtime.
//!
//! The Workspace operation lease excludes participating Session turns, Ways and
//! lifecycle operations. It is not an exclusive filesystem snapshot: direct
//! Files/Git routes, existing PTYs, background processes and external editors
//! still require their own integration.
//! After dispatch, raw command output cannot release ownership. A dropped lease
//! retains its execution gates in the owner. Only an exact opaque supervisor
//! settlement or explicit checked cleanup can release dispatched execution.

use super::{require_session_environment_ready, AxocoatlDaemon};
use crate::error::DaemonError;
use axocoatl_core::SecureDir;
use axocoatl_isolation::session_sandbox::Sandbox;
use axocoatl_isolation::supervisor_transport::{
    PreparedSupervisedCommand, ProcessSettlement, SupervisorCancellation,
};
use axocoatl_session::execution_store::DurableSessionIdentity;
use axocoatl_session::{
    Session, SessionRuntimeIdentity, SessionStatus, SessionStore, WorkspaceStore,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

#[path = "bootstrap_session_attempt_repository.rs"]
mod attempt;
pub(crate) use attempt::NativeWaysRuntimeFence;

type Result<T> = std::result::Result<T, DaemonError>;

/// Actual resource identity, not a checked tree, approved command or grant.
/// Runtime credentials/ownership tokens are deliberately not serialized here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRepositoryMetadata {
    pub workspace_id: String,
    pub session_id: String,
    pub environment_generation: u64,
    pub backend: String,
    pub runtime_id: String,
    pub execution_identity: String,
    pub runtime_root: PathBuf,
    pub host_workspace_inode: String,
}

struct RetainedExecution {
    id: String,
    _execution: OwnedMutexGuard<()>,
    _start: OwnedMutexGuard<()>,
}

#[derive(Clone)]
struct SupervisedIdentity {
    invocation_id: String,
    request_sha256: String,
    runtime_identity: String,
    program_sha256: String,
    transport_identity: String,
}

impl SupervisedIdentity {
    fn matches(&self, settlement: &ProcessSettlement) -> bool {
        self.invocation_id == settlement.invocation_id()
            && self.request_sha256 == settlement.request_sha256()
            && self.runtime_identity == settlement.runtime_identity()
            && self.program_sha256 == settlement.program_sha256()
            && self.transport_identity == settlement.transport_identity()
    }
}

struct SupervisedBinding {
    identity: SupervisedIdentity,
    cancellation: SupervisorCancellation,
}

#[derive(Default)]
struct ExecutionState {
    active: Option<String>,
    retained: Option<RetainedExecution>,
    supervised: Option<SupervisedBinding>,
    admission_closed: bool,
    cleaning: bool,
    released: bool,
}

struct RepositoryOwnerInner {
    attempt: Option<attempt::AttemptResourceIdentity>,
    identity: DurableSessionIdentity,
    metadata: SessionRepositoryMetadata,
    runtime: SessionRuntimeIdentity,
    sandbox: Arc<dyn Sandbox>,
    data_root: SecureDir,
    workspace_root: SecureDir,
    sessions: Arc<AsyncMutex<SessionStore>>,
    workspaces: Arc<AsyncMutex<WorkspaceStore>>,
    sandboxes: Arc<AsyncMutex<HashMap<String, Arc<dyn Sandbox>>>>,
    start: Arc<AsyncMutex<()>>,
    shutdown: tokio::sync::watch::Receiver<bool>,
    workspace_operation: Mutex<Option<OwnedMutexGuard<()>>>,
    workspace_gate: Arc<AsyncMutex<()>>,
    execution: Arc<AsyncMutex<()>>,
    state: Mutex<ExecutionState>,
    changed: tokio::sync::Notify,
}

/// Retain this owner alongside the canonical controller, including while a
/// result is unknown. Clones share the same Workspace and execution gates.
#[derive(Clone)]
pub struct SessionRepositoryOwner {
    inner: Arc<RepositoryOwnerInner>,
}

pub struct SessionRepositoryExecutionLease {
    owner: SessionRepositoryOwner,
    id: String,
    execution: Option<OwnedMutexGuard<()>>,
    start: Option<OwnedMutexGuard<()>>,
    dispatched: bool,
}

fn failure(message: impl Into<String>) -> DaemonError {
    DaemonError::SessionConflict(message.into())
}

fn validate_session_owner(session: &Session, identity: &DurableSessionIdentity) -> Result<()> {
    if session.id != identity.owner().session_id.as_str()
        || session.workspace_id != identity.owner().workspace_id
        || session.status == SessionStatus::Closed
    {
        return Err(failure(
            "repository Session owner is closed or differs from canonical history",
        ));
    }
    require_session_environment_ready(session)?;
    // This owner is used only by native Session execution. A Ready remote
    // environment still serves legacy Sessions, but cannot supply the owned
    // process settlement required by native repository tools and checks.
    if session
        .environment
        .runtime
        .as_ref()
        .is_some_and(|runtime| runtime.backend != "podman")
    {
        return Err(failure(
            "Native Session work requires local Podman process supervision; E2B repository execution is supported only for legacy Sessions",
        ));
    }
    Ok(())
}

impl AxocoatlDaemon {
    /// Physical resources for an exact retained pre-turn entry. The registry
    /// owns canonical stores across these waits and checks the token afterward.
    pub(crate) async fn pending_session_repository_owner(
        &self,
        token: &super::session_dispatch::PendingSessionToken,
    ) -> Result<SessionRepositoryOwner> {
        let identity = self
            .session_dispatch_lifecycles
            .pending_identity(token, &self.data_root)?;
        self.repository_owner_for_identity(identity.clone(), || {
            if self
                .session_dispatch_lifecycles
                .pending_identity(token, &self.data_root)?
                != identity
            {
                return Err(failure(
                    "canonical Session identity changed while preparing its first owner",
                ));
            }
            Ok(())
        })
        .await
    }

    async fn repository_owner_for_identity(
        &self,
        identity: DurableSessionIdentity,
        verify_canonical: impl Fn() -> Result<()>,
    ) -> Result<SessionRepositoryOwner> {
        verify_canonical()?;
        self.require_runtime_admission()?;
        let session_id = identity.owner().session_id.as_str();
        let operation = self
            .attempt_operation_for_workspace(&identity.owner().workspace_id)
            .await;
        let workspace_gate = operation.clone();
        let operation = operation
            .try_lock_owned()
            .map_err(|_| failure("Workspace already has an owning operation"))?;
        verify_canonical()?;
        self.require_runtime_admission()?;
        self.require_no_unresolved_attempt(session_id).await?;
        let session = self
            .get_session(session_id)
            .await
            .ok_or_else(|| failure("repository Session is missing"))?;
        validate_session_owner(&session, &identity)?;
        let workspace = self
            .workspace_store
            .lock()
            .await
            .get(&session.workspace_id)
            .ok_or_else(|| failure("repository Workspace is missing"))?;
        if workspace.canonical_path != session.working_dir {
            return Err(failure("Session path differs from its durable Workspace"));
        }
        let sandbox = self.ensure_sandbox(&session).await?;
        let start = self
            .sandbox_starts
            .lock()
            .await
            .get(session_id)
            .cloned()
            .ok_or_else(|| failure("repository runtime has no start owner"))?;
        let _start = start.clone().lock_owned().await;
        self.require_runtime_admission()?;
        let session = self
            .get_session(session_id)
            .await
            .ok_or_else(|| failure("repository Session disappeared"))?;
        validate_session_owner(&session, &identity)?;
        let runtime = session
            .environment
            .runtime
            .clone()
            .ok_or_else(|| failure("Ready repository has no runtime identity"))?;
        if runtime.cleanup_confirmed {
            return Err(failure("repository runtime has already been cleaned up"));
        }
        let registered = self
            .session_sandboxes
            .lock()
            .await
            .get(session_id)
            .cloned()
            .ok_or_else(|| failure("Ready repository has no live runtime"))?;
        if !Arc::ptr_eq(&registered, &sandbox) {
            return Err(failure(
                "repository runtime changed during ownership admission",
            ));
        }
        let execution_identity = sandbox
            .execution_identity()
            .filter(|id| !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control))
            .ok_or_else(|| failure("repository requires an immutable execution identity"))?
            .to_owned();
        if !sandbox.execution_boundary_usable() {
            return Err(failure("repository execution boundary is unavailable"));
        }
        if (runtime.backend == "podman"
            && (runtime.id != session.id
                || sandbox.runtime_id().is_some()
                || sandbox.root() != session.working_dir))
            || (runtime.backend == "e2b"
                && (sandbox.runtime_id() != Some(runtime.id.as_str())
                    || runtime.remote_root.as_deref().map(Path::new) != Some(sandbox.root())))
            || !matches!(runtime.backend.as_str(), "podman" | "e2b")
        {
            return Err(failure(
                "actual runtime differs from its durable Ready identity",
            ));
        }
        let workspace_root = SecureDir::open(&workspace.canonical_path)
            .map_err(|error| failure(error.to_string()))?;
        let metadata = SessionRepositoryMetadata {
            workspace_id: workspace.id,
            session_id: session.id,
            environment_generation: session.environment.generation,
            backend: runtime.backend.clone(),
            runtime_id: runtime.id.clone(),
            execution_identity,
            runtime_root: sandbox.root().to_path_buf(),
            host_workspace_inode: workspace_root
                .inode_identity()
                .map_err(|error| failure(error.to_string()))?,
        };
        let owner = SessionRepositoryOwner {
            inner: Arc::new(RepositoryOwnerInner {
                attempt: None,
                identity,
                metadata,
                runtime,
                sandbox,
                data_root: self.data_root.clone(),
                workspace_root,
                sessions: self.session_store.clone(),
                workspaces: self.workspace_store.clone(),
                sandboxes: self.session_sandboxes.clone(),
                start,
                shutdown: self.shutdown_subscriber(),
                workspace_operation: Mutex::new(Some(operation)),
                workspace_gate,
                execution: Arc::new(AsyncMutex::new(())),
                state: Mutex::new(ExecutionState::default()),
                changed: tokio::sync::Notify::new(),
            }),
        };
        owner.validate_current().await?;
        verify_canonical()?;
        Ok(owner)
    }

    pub(crate) fn validate_repository_daemon_binding(
        &self,
        owner: &SessionRepositoryOwner,
    ) -> Result<()> {
        self.data_root
            .verify_ambient_identity()
            .map_err(|error| failure(error.to_string()))?;
        if self
            .data_root
            .inode_identity()
            .map_err(|error| failure(error.to_string()))?
            != owner
                .inner
                .data_root
                .inode_identity()
                .map_err(|error| failure(error.to_string()))?
            || !Arc::ptr_eq(&self.session_store, &owner.inner.sessions)
            || !Arc::ptr_eq(&self.workspace_store, &owner.inner.workspaces)
            || !Arc::ptr_eq(&self.session_sandboxes, &owner.inner.sandboxes)
        {
            return Err(failure(
                "repository owner belongs to another daemon instance or root",
            ));
        }
        Ok(())
    }
}

impl SessionRepositoryOwner {
    /// This is the daemon's coordinating mutex, not process-settlement proof.
    pub(crate) fn workspace_gate(&self) -> Arc<AsyncMutex<()>> {
        self.inner.workspace_gate.clone()
    }

    /// Mint a fresh capability for the same still-current resource. The old
    /// owner's released bit and every old execution lease remain permanently
    /// retired. No command, Stop, or runtime cleanup is performed here.
    pub(crate) async fn reacquire_between_turns(&self) -> Result<Self> {
        if self.inner.attempt.is_some() {
            return Err(failure(
                "A disposed Way cannot become the primary Session runtime",
            ));
        }
        {
            let state = self
                .inner
                .state
                .lock()
                .map_err(|_| failure("repository owner state failed"))?;
            if !state.released
                || state.active.is_some()
                || state.retained.is_some()
                || state.supervised.is_some()
                || state.cleaning
            {
                return Err(failure(
                    "repository reacquisition requires a retired settled owner",
                ));
            }
        }
        let operation = self
            .inner
            .workspace_gate
            .clone()
            .try_lock_owned()
            .map_err(|_| failure("Workspace already has an owning operation"))?;
        let start = self.inner.start.clone().lock_owned().await;
        let owner = Self {
            inner: Arc::new(RepositoryOwnerInner {
                attempt: None,
                identity: self.inner.identity.clone(),
                metadata: self.inner.metadata.clone(),
                runtime: self.inner.runtime.clone(),
                sandbox: self.inner.sandbox.clone(),
                data_root: self.inner.data_root.clone(),
                workspace_root: self.inner.workspace_root.clone(),
                sessions: self.inner.sessions.clone(),
                workspaces: self.inner.workspaces.clone(),
                sandboxes: self.inner.sandboxes.clone(),
                start: self.inner.start.clone(),
                shutdown: self.inner.shutdown.clone(),
                workspace_operation: Mutex::new(Some(operation)),
                workspace_gate: self.inner.workspace_gate.clone(),
                execution: Arc::new(AsyncMutex::new(())),
                state: Mutex::new(ExecutionState::default()),
                changed: tokio::sync::Notify::new(),
            }),
        };
        owner.validate_current().await?;
        if !owner.inner.sandbox.execution_boundary_usable() {
            return Err(failure("repository execution boundary is unavailable"));
        }
        drop(start);
        Ok(owner)
    }

    pub(crate) fn is_fresh_reacquisition_of(&self, prior: &Self) -> bool {
        !self.same_owner(prior)
            && self.identity() == prior.identity()
            && self.metadata() == prior.metadata()
            && Arc::ptr_eq(&self.inner.workspace_gate, &prior.inner.workspace_gate)
            && Arc::ptr_eq(&self.inner.sessions, &prior.inner.sessions)
            && Arc::ptr_eq(&self.inner.workspaces, &prior.inner.workspaces)
            && Arc::ptr_eq(&self.inner.sandboxes, &prior.inner.sandboxes)
            && Arc::ptr_eq(&self.inner.sandbox, &prior.inner.sandbox)
    }

    pub(crate) fn same_owner(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
    pub fn identity(&self) -> &DurableSessionIdentity {
        &self.inner.identity
    }
    pub fn metadata(&self) -> &SessionRepositoryMetadata {
        &self.inner.metadata
    }
    pub fn backend(&self) -> &str {
        &self.inner.metadata.backend
    }
    pub fn root(&self) -> &Path {
        &self.inner.metadata.runtime_root
    }
    pub fn execution_identity(&self) -> &str {
        &self.inner.metadata.execution_identity
    }
    pub(crate) fn sandbox(&self) -> &Arc<dyn Sandbox> {
        &self.inner.sandbox
    }

    pub fn unresolved_execution(&self) -> Result<Option<String>> {
        let state = self
            .inner
            .state
            .lock()
            .map_err(|_| failure("repository owner state failed"))?;
        Ok(state.retained.as_ref().map(|retained| retained.id.clone()))
    }

    /// Lifecycle cancellation closes future admission before observing the
    /// current command, including when resource preparation has not bound yet.
    /// The returned signal remains addressable; it is never settlement proof.
    pub fn request_supervised_stop(&self) -> Result<Option<SupervisorCancellation>> {
        let mut state = self
            .inner
            .state
            .lock()
            .map_err(|_| failure("repository owner state failed"))?;
        state.admission_closed = true;
        let cancellation = state
            .supervised
            .as_ref()
            .map(|binding| binding.cancellation.clone());
        if let Some(signal) = &cancellation {
            signal.cancel();
        }
        Ok(cancellation)
    }

    /// Synchronous recheck beneath a registered controller's dispatch lock.
    /// Full persisted Session/runtime validation is repeated by execution_lease
    /// across its awaits before any supervised command can be dispatched.
    pub(crate) fn validate_dispatch_resource(&self) -> Result<()> {
        if let Some(attempt) = &self.inner.attempt {
            attempt.validate_live()?;
        }
        let state = self
            .inner
            .state
            .lock()
            .map_err(|_| failure("repository owner state failed"))?;
        if state.released
            || state.admission_closed
            || state.retained.is_some()
            || state.cleaning
            || *self.inner.shutdown.borrow()
            || self.inner.sandbox.execution_identity() != Some(self.execution_identity())
            || self.inner.sandbox.root() != self.root()
            || !self.inner.sandbox.execution_boundary_usable()
        {
            return Err(failure(
                "repository resource is retired, uncertain or unavailable",
            ));
        }
        Ok(())
    }

    /// This reads retained live state, never reconstructed result metadata.
    pub(crate) fn execution_is_idle(&self) -> Result<bool> {
        let state = self
            .inner
            .state
            .lock()
            .map_err(|_| failure("repository owner state failed"))?;
        Ok(state.active.is_none()
            && state.retained.is_none()
            && !state.cleaning
            && state.supervised.is_none()
            && !state.released)
    }

    /// Close admission before waiting. Cancellation of this waiter does not
    /// remove the retained owner or turn an unknown process into a stopped one.
    pub(crate) async fn wait_for_execution_boundary(
        &self,
        timeout: std::time::Duration,
    ) -> Result<()> {
        self.request_supervised_stop()?;
        tokio::time::timeout(timeout, async {
            loop {
                let changed = self.inner.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                {
                    let state = self.inner.state.lock().map_err(|_| failure("repository owner state failed"))?;
                    if state.active.is_none() && !state.cleaning {
                        return Ok(());
                    }
                }
                changed.await;
            }
        }).await.map_err(|_| failure("repository command has not reached a confirmed execution boundary; ownership remains held, retry Close after it settles"))?
    }

    /// Transfer the existing Workspace gate to an explicit lifecycle action.
    /// Existing owner clones become permanently unable to admit more commands.
    pub(crate) fn retire_idle(&self) -> Result<OwnedMutexGuard<()>> {
        let mut state = self
            .inner
            .state
            .lock()
            .map_err(|_| failure("repository owner state failed"))?;
        state.admission_closed = true;
        if state.active.is_some()
            || state.retained.is_some()
            || state.cleaning
            || state.supervised.is_some()
            || state.released
        {
            return Err(failure("repository owner cannot retire while execution is active, unknown, or already retired"));
        }
        let operation = self
            .inner
            .workspace_operation
            .lock()
            .map_err(|_| failure("repository Workspace owner failed"))?
            .take()
            .ok_or_else(|| failure("repository Workspace gate is missing"))?;
        state.released = true;
        self.inner.changed.notify_waiters();
        Ok(operation)
    }

    /// Unknown processes require an explicitly authorized full local-runtime
    /// cleanup. Pausing a remote VM cannot prove its old processes will not
    /// resume; preserve the existing remote Close policy instead of deleting it.
    pub(crate) async fn cleanup_for_lifecycle(&self) -> Result<OwnedMutexGuard<()>> {
        if self.backend() != "podman" {
            return Err(failure("unknown repository processes require confirmed cleanup before this runtime can close; remote pause is not process settlement"));
        }
        self.cleanup_with_operation()
            .await?
            .ok_or_else(|| failure("repository cleanup gate was already transferred"))
    }

    pub(crate) async fn validate_current(&self) -> Result<()> {
        if *self.inner.shutdown.borrow() {
            return Err(failure("daemon shutdown closed repository admission"));
        }
        self.validate_resource(false).await.map(|_| ())
    }

    fn validate_session_binding(&self, session: &Session, allow_cleaned: bool) -> Result<bool> {
        if session.id != self.inner.identity.owner().session_id.as_str()
            || session.workspace_id != self.inner.identity.owner().workspace_id
            || session.environment.generation != self.inner.metadata.environment_generation
        {
            return Err(failure(
                "repository Session identity or preparation generation changed",
            ));
        }
        let runtime = session
            .environment
            .runtime
            .as_ref()
            .ok_or_else(|| failure("repository lost its retained runtime identity"))?;
        let mut expected = self.inner.runtime.clone();
        if allow_cleaned {
            expected.cleanup_confirmed = runtime.cleanup_confirmed;
        } else {
            validate_session_owner(session, &self.inner.identity)?;
        }
        if runtime != &expected {
            return Err(failure("repository runtime identity changed"));
        }
        Ok(runtime.cleanup_confirmed)
    }

    /// Cleanup can finish an exact retained runtime after Close or a durable
    /// cleanup tombstone. Neither state authorizes another execution.
    async fn validate_resource(&self, allow_cleaned: bool) -> Result<bool> {
        if let Some(attempt) = &self.inner.attempt {
            return attempt.validate_resource(self, allow_cleaned).await;
        }
        self.inner
            .data_root
            .verify_ambient_identity()
            .map_err(|error| failure(error.to_string()))?;
        self.inner
            .workspace_root
            .verify_ambient_identity()
            .map_err(|error| failure(error.to_string()))?;
        let session = self
            .inner
            .sessions
            .lock()
            .await
            .get(&self.inner.metadata.session_id)
            .ok_or_else(|| failure("repository Session is missing"))?;
        let cleaned = self.validate_session_binding(&session, allow_cleaned)?;
        let workspace = self
            .inner
            .workspaces
            .lock()
            .await
            .get(&self.inner.metadata.workspace_id)
            .ok_or_else(|| failure("repository Workspace is missing"))?;
        if workspace.canonical_path != session.working_dir
            || workspace.canonical_path != self.inner.workspace_root.path()
            || self.inner.sandbox.execution_identity() != Some(self.execution_identity())
            || self.inner.sandbox.root() != self.root()
        {
            return Err(failure("repository runtime or Workspace identity changed"));
        }
        match self.inner.sandboxes.lock().await.get(&session.id) {
            Some(current) if Arc::ptr_eq(current, &self.inner.sandbox) => {}
            None if allow_cleaned && cleaned => {}
            _ => {
                return Err(failure(
                    "repository runtime incarnation changed or disappeared",
                ))
            }
        }
        Ok(cleaned)
    }

    /// Tests drive cleanup without keeping the Workspace operation guard.
    #[cfg(test)]
    async fn cleanup_checked(&self) -> Result<()> {
        drop(self.cleanup_with_operation().await?);
        Ok(())
    }

    async fn cleanup_with_operation(&self) -> Result<Option<OwnedMutexGuard<()>>> {
        let Some(cleanup) = self.begin_cleanup()? else {
            // An exact repeat must not touch a later replacement runtime.
            return Ok(None);
        };
        if self.inner.attempt.is_some() {
            self.inner
                .sandbox
                .stop_checked()
                .await
                .map_err(|error| failure(format!("checked Way cleanup: {error}")))?;
            return cleanup.finish().map(Some);
        }
        // The retained execution owns the Session start lock; do not reacquire.
        let cleaned = self.validate_resource(true).await?;
        if !cleaned {
            self.inner
                .sandbox
                .stop_checked()
                .await
                .map_err(|error| failure(format!("checked repository runtime cleanup: {error}")))?;
            let mut sessions = self.inner.sessions.lock().await;
            let current = sessions
                .get(&self.inner.metadata.session_id)
                .ok_or_else(|| failure("repository Session disappeared during cleanup"))?;
            self.validate_session_binding(&current, true)?;
            sessions
                .confirm_environment_runtime_cleanup(
                    &self.inner.metadata.session_id,
                    &self.inner.runtime.id,
                )
                .map_err(|error| {
                    failure(format!("persisting repository runtime cleanup: {error}"))
                })?;
        }
        let mut sandboxes = self.inner.sandboxes.lock().await;
        match sandboxes.get(&self.inner.metadata.session_id) {
            Some(current) if Arc::ptr_eq(current, &self.inner.sandbox) => {
                sandboxes.remove(&self.inner.metadata.session_id);
            }
            None => {}
            Some(_) => {
                return Err(failure(
                    "repository runtime changed before cleanup eviction",
                ))
            }
        }
        drop(sandboxes);
        cleanup.finish().map(Some)
    }

    pub(crate) async fn execution_lease(&self) -> Result<SessionRepositoryExecutionLease> {
        {
            let state = self
                .inner
                .state
                .lock()
                .map_err(|_| failure("repository owner state failed"))?;
            if state.released
                || state.admission_closed
                || state.retained.is_some()
                || state.cleaning
            {
                return Err(failure(
                    "repository has an unsettled execution or released owner",
                ));
            }
        }
        let execution = self
            .inner
            .execution
            .clone()
            .try_lock_owned()
            .map_err(|_| failure("repository already has an active execution"))?;
        self.execution_lease_after_gate(execution).await
    }

    /// Queue foreground tools behind the same physical execution gate. Waiting
    /// has no process authority; all retained state is checked after acquisition.
    pub(crate) async fn queued_execution_lease(&self) -> Result<SessionRepositoryExecutionLease> {
        self.validate_dispatch_resource()?;
        let execution = self.inner.execution.clone().lock_owned().await;
        self.execution_lease_after_gate(execution).await
    }

    async fn execution_lease_after_gate(
        &self,
        execution: OwnedMutexGuard<()>,
    ) -> Result<SessionRepositoryExecutionLease> {
        let id = uuid::Uuid::new_v4().to_string();
        {
            let mut state = self
                .inner
                .state
                .lock()
                .map_err(|_| failure("repository owner state failed"))?;
            if state.released
                || state.admission_closed
                || state.active.is_some()
                || state.retained.is_some()
            {
                return Err(failure("repository execution ownership changed"));
            }
            state.active = Some(id.clone());
        }
        // This guard exists before the first await, so a cancelled start-lock
        // wait clears active ownership and wakes Close through the same Drop.
        let mut lease = SessionRepositoryExecutionLease {
            owner: self.clone(),
            id,
            execution: Some(execution),
            start: None,
            dispatched: false,
        };
        lease.start = Some(self.inner.start.clone().lock_owned().await);
        self.validate_current().await?;
        if !self.inner.sandbox.execution_boundary_usable() {
            return Err(failure("repository execution boundary is unavailable"));
        }
        {
            let state = self
                .inner
                .state
                .lock()
                .map_err(|_| failure("repository owner state failed"))?;
            if state.admission_closed
                || state.released
                || state.active.as_deref() != Some(lease.id())
            {
                return Err(failure(
                    "repository execution admission closed while preparing",
                ));
            }
        }
        Ok(lease)
    }

    fn begin_cleanup(&self) -> Result<Option<RepositoryCleanup>> {
        let mut state = self
            .inner
            .state
            .lock()
            .map_err(|_| failure("repository owner state failed"))?;
        if state.released {
            return Ok(None);
        }
        if state.cleaning || state.active.is_some() || state.retained.is_none() {
            return Err(failure(
                "repository cleanup requires its retained unsettled execution",
            ));
        }
        state.cleaning = true;
        Ok(Some(RepositoryCleanup {
            owner: self.clone(),
            finished: false,
        }))
    }
}

impl SessionRepositoryExecutionLease {
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn owner(&self) -> &SessionRepositoryOwner {
        &self.owner
    }
    pub(crate) fn sandbox(&self) -> &Arc<dyn Sandbox> {
        self.owner.sandbox()
    }

    /// Bind one actual prepared first-party helper before durable dispatch
    /// admission. A second helper, even for the same request, cannot replace it.
    pub fn bind_supervised_command(&mut self, command: &PreparedSupervisedCommand) -> Result<()> {
        self.owner
            .inner
            .data_root
            .verify_ambient_identity()
            .map_err(|error| failure(error.to_string()))?;
        self.owner
            .inner
            .workspace_root
            .verify_ambient_identity()
            .map_err(|error| failure(error.to_string()))?;
        if command.runtime_identity() != self.owner.execution_identity()
            || self.owner.inner.sandbox.execution_identity()
                != Some(self.owner.execution_identity())
            || self.owner.inner.sandbox.root() != self.owner.root()
            || !self.owner.inner.sandbox.execution_boundary_usable()
        {
            return Err(failure(
                "prepared supervisor belongs to another repository runtime",
            ));
        }
        let request_sha256 = command.request().digest().map_err(failure)?;
        let cancellation = command.cancellation();
        let mut state = self
            .owner
            .inner
            .state
            .lock()
            .map_err(|_| failure("repository owner state failed"))?;
        self.require_bindable(&state)?;
        if cancellation.is_cancelled() {
            return Err(failure("prepared supervisor is already cancelled"));
        }
        state.supervised = Some(SupervisedBinding {
            identity: SupervisedIdentity {
                invocation_id: command.request().invocation_id.clone(),
                request_sha256,
                runtime_identity: command.runtime_identity().to_owned(),
                program_sha256: command.program_sha256().to_owned(),
                transport_identity: command.transport_identity().to_owned(),
            },
            cancellation,
        });
        Ok(())
    }

    fn require_bindable(&self, state: &ExecutionState) -> Result<()> {
        if self.dispatched
            || state.active.as_deref() != Some(self.id())
            || state.supervised.is_some()
            || state.admission_closed
            || state.released
            || state.retained.is_some()
            || state.cleaning
            || *self.owner.inner.shutdown.borrow()
        {
            return Err(failure(
                "repository lease cannot bind a prepared supervisor",
            ));
        }
        Ok(())
    }

    /// Consume an exact opaque proof from this specific prepared helper. A
    /// receipt from an earlier helper cannot release a new transport, even when
    /// invocation and executable request bytes happen to be identical.
    pub fn settle_supervised(mut self, settlement: &ProcessSettlement) -> Result<()> {
        {
            let mut state = self
                .owner
                .inner
                .state
                .lock()
                .map_err(|_| failure("repository owner state failed"))?;
            if !self.dispatched
                || state.active.as_deref() != Some(self.id())
                || state.released
                || state.retained.is_some()
                || state.cleaning
            {
                return Err(failure(
                    "repository lease is not the active dispatched execution",
                ));
            }
            let binding = state
                .supervised
                .as_ref()
                .ok_or_else(|| failure("repository execution has no bound supervisor"))?;
            if !binding.identity.matches(settlement) {
                return Err(failure(
                    "process settlement does not match this exact prepared helper",
                ));
            }
            state.supervised = None;
            state.active = None;
            self.dispatched = false;
        }
        self.execution.take();
        self.start.take();
        self.owner.inner.changed.notify_waiters();
        Ok(())
    }

    /// Arm before the durable execution claim can authorize a backend call.
    /// Failure or cancellation afterwards cannot be relabelled as nondispatch.
    pub fn mark_dispatched(&mut self) -> Result<()> {
        let state = self
            .owner
            .inner
            .state
            .lock()
            .map_err(|_| failure("repository owner state failed"))?;
        if state.active.as_deref() != Some(self.id())
            || state.released
            || state.admission_closed
            || state.retained.is_some()
        {
            return Err(failure("repository lease is no longer current"));
        }
        if *self.owner.inner.shutdown.borrow() {
            return Err(failure("daemon shutdown closed repository admission"));
        }
        if state
            .supervised
            .as_ref()
            .is_some_and(|binding| binding.cancellation.is_cancelled())
        {
            return Err(failure("prepared supervisor was cancelled before dispatch"));
        }
        self.dispatched = true;
        Ok(())
    }
}

impl Drop for SessionRepositoryExecutionLease {
    fn drop(&mut self) {
        let mut state = self
            .owner
            .inner
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if state.active.as_deref() == Some(&self.id) {
            state.active = None;
            if let Some(binding) = &state.supervised {
                binding.cancellation.cancel();
            }
            if self.dispatched {
                if let (Some(execution), Some(start)) = (self.execution.take(), self.start.take()) {
                    state.retained = Some(RetainedExecution {
                        id: self.id.clone(),
                        _execution: execution,
                        _start: start,
                    });
                }
            } else {
                state.supervised = None;
            }
        }
        self.owner.inner.changed.notify_waiters();
    }
}

struct RepositoryCleanup {
    owner: SessionRepositoryOwner,
    finished: bool,
}

impl RepositoryCleanup {
    fn finish(mut self) -> Result<OwnedMutexGuard<()>> {
        let mut state = self
            .owner
            .inner
            .state
            .lock()
            .map_err(|_| failure("repository owner state failed"))?;
        let mut operation = self
            .owner
            .inner
            .workspace_operation
            .lock()
            .map_err(|_| failure("repository Workspace owner failed"))?;
        let operation = operation
            .take()
            .ok_or_else(|| failure("repository cleanup lost its Workspace gate"))?;
        state.released = true;
        state.admission_closed = true;
        state.cleaning = false;
        state.retained.take();
        state.supervised = None;
        self.finished = true;
        self.owner.inner.changed.notify_waiters();
        Ok(operation)
    }
}

impl Drop for RepositoryCleanup {
    fn drop(&mut self) {
        if !self.finished {
            self.owner
                .inner
                .state
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .cleaning = false;
            self.owner.inner.changed.notify_waiters();
        }
    }
}

#[cfg(test)]
#[path = "bootstrap_session_repository_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "bootstrap_session_repository_runtime_tests.rs"]
mod runtime_tests;
