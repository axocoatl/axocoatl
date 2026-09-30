//! Actual isolated candidate ownership under the existing ActiveAttemptRun.
//! Its durable current-set pointer excludes primary Workspace execution; each
//! candidate retains its own supervisor gate and cannot become a Session owner.
use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Weak;

pub(crate) struct NativeWaysRuntimeFence {
    live: Arc<AtomicBool>,
}
impl NativeWaysRuntimeFence {
    pub(crate) fn new() -> Self {
        Self {
            live: Arc::new(AtomicBool::new(true)),
        }
    }
    pub(crate) fn close(&self) {
        self.live.store(false, Ordering::SeqCst);
    }
}
impl Drop for NativeWaysRuntimeFence {
    fn drop(&mut self) {
        self.close();
    }
}

pub(super) struct AttemptResourceIdentity {
    set: crate::git::AttemptSet,
    lane: usize,
    root: SecureDir,
    lane_root: SecureDir,
    fence: Weak<AtomicBool>,
}
impl AttemptResourceIdentity {
    pub(super) fn validate_live(&self) -> Result<()> {
        if !self
            .fence
            .upgrade()
            .is_some_and(|fence| fence.load(Ordering::SeqCst))
        {
            return Err(failure(
                "The isolated Way no longer has its actual runtime owner",
            ));
        }
        self.root
            .verify_ambient_identity()
            .map_err(|error| failure(error.to_string()))?;
        self.lane_root
            .verify_ambient_identity()
            .map_err(|error| failure(error.to_string()))?;
        Ok(())
    }
    pub(super) async fn validate_resource(
        &self,
        owner: &SessionRepositoryOwner,
        allow_cleaned: bool,
    ) -> Result<bool> {
        if !allow_cleaned {
            self.validate_live()?;
        }
        owner
            .inner
            .data_root
            .verify_ambient_identity()
            .map_err(|error| failure(error.to_string()))?;
        owner
            .inner
            .workspace_root
            .verify_ambient_identity()
            .map_err(|error| failure(error.to_string()))?;
        let session = owner
            .inner
            .sessions
            .lock()
            .await
            .get(&self.set.session_id)
            .ok_or_else(|| failure("Way Session is missing"))?;
        if session.id != owner.identity().owner().session_id.as_str()
            || session.workspace_id != owner.identity().owner().workspace_id
            || session.environment.generation != owner.metadata().environment_generation
            || (!allow_cleaned && session.status == SessionStatus::Closed)
        {
            return Err(failure(
                "Way Session identity, environment generation or lifecycle changed",
            ));
        }
        let workspace = owner
            .inner
            .workspaces
            .lock()
            .await
            .get(&session.workspace_id)
            .ok_or_else(|| failure("Way Workspace is missing"))?;
        if workspace.canonical_path != session.working_dir
            || workspace.canonical_path != owner.inner.workspace_root.path()
            || owner.inner.sandbox.root() != self.lane_root.path()
            || owner.inner.sandbox.execution_identity() != Some(owner.execution_identity())
        {
            return Err(failure(
                "Way sandbox incarnation or Workspace identity changed",
            ));
        }
        let current = AxocoatlDaemon::read_host_json_file::<crate::git::AttemptSet>(
            &self.root,
            Path::new("set.json"),
        )?
        .ok_or_else(|| failure("Way admission manifest is missing"))?;
        if current.id != self.set.id
            || current.session_id != self.set.session_id
            || current.base_sha != self.set.base_sha
            || current.base_tree != self.set.base_tree
            || current.lanes != self.set.lanes
            || current.task != self.set.task
            || current.instruction != self.set.instruction
        {
            return Err(failure("Way immutable source or candidate roster changed"));
        }
        let lane = current
            .lanes
            .iter()
            .find(|lane| lane.index == self.lane)
            .ok_or_else(|| failure("Way candidate is no longer declared"))?;
        if Path::new(&lane.worktree) != self.lane_root.path() {
            return Err(failure("Way candidate root changed"));
        }
        let session_root =
            crate::attempts::session_attempts_root(&session.working_dir, &session.id);
        let relative = session_root
            .strip_prefix(&session.working_dir)
            .map_err(|error| failure(error.to_string()))?;
        let pointer = owner
            .inner
            .workspace_root
            .existing_child(relative)
            .map_err(|error| failure(error.to_string()))?;
        let active = AxocoatlDaemon::read_host_json_file::<crate::git::AttemptSet>(
            &pointer,
            Path::new("current.json"),
        )?
        .ok_or_else(|| failure("Way no longer owns the durable current-set pointer"))?;
        if active.id != self.set.id {
            return Err(failure("Another attempt set owns this Workspace"));
        }
        Ok(false)
    }
}

impl AxocoatlDaemon {
    // These separate owned resources and exact identity proofs must all be
    // checked together; bundling them would not remove an independent input.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn native_attempt_repository_owner(
        &self,
        token: &super::super::session_dispatch::PendingSessionToken,
        session: &Session,
        set: &crate::git::AttemptSet,
        index: usize,
        sandbox: Arc<dyn Sandbox>,
        lane_root: SecureDir,
        fence: &NativeWaysRuntimeFence,
    ) -> Result<SessionRepositoryOwner> {
        let identity = self
            .session_dispatch_lifecycles
            .pending_identity(token, &self.data_root)?;
        let expected =
            crate::attempts::worktree_path(&session.working_dir, &session.id, &set.id, index);
        if self.config.sandbox.backend != "podman"
            || identity.owner().session_id.as_str() != session.id
            || identity.owner().workspace_id != session.workspace_id
            || lane_root.path() != expected
            || sandbox.root() != expected
            || !sandbox.execution_boundary_usable()
        {
            return Err(failure(
                "Actual Way sandbox differs from its retained native Session and clone",
            ));
        }
        let execution_identity = sandbox
            .execution_identity()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| failure("Way sandbox has no immutable execution identity"))?
            .to_owned();
        let root = Self::open_attempt_root_host(&session.working_dir, &session.id, &set.id)?;
        let workspace_root =
            SecureDir::open(&session.working_dir).map_err(|error| failure(error.to_string()))?;
        let runtime_id = crate::attempts::container_id(&session.id, &set.id, index);
        // This gate belongs to the existing isolated candidate lifecycle. The
        // durable unresolved-set pointer remains the Workspace exclusion owner.
        let gate = Arc::new(AsyncMutex::new(()));
        let operation = gate.clone().lock_owned().await;
        let owner = SessionRepositoryOwner {
            inner: Arc::new(RepositoryOwnerInner {
                attempt: Some(AttemptResourceIdentity {
                    set: set.clone(),
                    lane: index,
                    root,
                    lane_root,
                    fence: Arc::downgrade(&fence.live),
                }),
                identity,
                metadata: SessionRepositoryMetadata {
                    workspace_id: session.workspace_id.clone(),
                    session_id: session.id.clone(),
                    environment_generation: session.environment.generation,
                    backend: "podman".into(),
                    runtime_id: runtime_id.clone(),
                    execution_identity,
                    runtime_root: expected,
                    host_workspace_inode: workspace_root
                        .inode_identity()
                        .map_err(|error| failure(error.to_string()))?,
                },
                runtime: SessionRuntimeIdentity {
                    backend: "podman".into(),
                    id: runtime_id,
                    remote_root: None,
                    control_plane: None,
                    data_plane_domain: None,
                    authority_fingerprint: None,
                    ownership_token: None,
                    cleanup_confirmed: false,
                },
                sandbox,
                data_root: self.data_root.clone(),
                workspace_root,
                sessions: self.session_store.clone(),
                workspaces: self.workspace_store.clone(),
                sandboxes: self.session_sandboxes.clone(),
                start: Arc::new(AsyncMutex::new(())),
                shutdown: self.shutdown_subscriber(),
                workspace_operation: Mutex::new(Some(operation)),
                workspace_gate: gate,
                execution: Arc::new(AsyncMutex::new(())),
                state: Mutex::new(ExecutionState::default()),
                changed: tokio::sync::Notify::new(),
            }),
        };
        owner.validate_current().await?;
        Ok(owner)
    }
}
