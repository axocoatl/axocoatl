#![cfg(unix)]

use super::*;
use axocoatl_isolation::session_sandbox::{BgTask, ExecResult};
use axocoatl_isolation::IsolationError;
use axocoatl_session::execution_ownership::LegacyFormatOwnership;
use axocoatl_session::execution_store::{ExecutionStoreOwner, SessionExecutionStore};
use axocoatl_session::turn_contract::SessionId;
use axocoatl_session::{SessionEnvironmentState, SessionMode};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::Notify;

struct ControlledSandbox {
    root: PathBuf,
    incarnation: String,
    remote_id: Option<String>,
    stop_calls: AtomicUsize,
    fail_stop: AtomicBool,
    block_stop: AtomicBool,
    stop_started: Notify,
    allow_stop: Notify,
}

impl ControlledSandbox {
    fn new(root: &Path, incarnation: &str) -> Self {
        Self {
            root: root.to_path_buf(),
            incarnation: incarnation.into(),
            remote_id: None,
            stop_calls: AtomicUsize::new(0),
            fail_stop: AtomicBool::new(false),
            block_stop: AtomicBool::new(false),
            stop_started: Notify::new(),
            allow_stop: Notify::new(),
        }
    }
}

#[async_trait::async_trait]
impl Sandbox for ControlledSandbox {
    fn root(&self) -> &Path {
        &self.root
    }
    fn execution_identity(&self) -> Option<&str> {
        Some(&self.incarnation)
    }
    fn runtime_id(&self) -> Option<&str> {
        self.remote_id.as_deref()
    }
    async fn exec(
        &self,
        _: &[&str],
        _: Duration,
    ) -> std::result::Result<ExecResult, IsolationError> {
        panic!("owner lifecycle must not dispatch a command")
    }
    async fn exec_stdin(
        &self,
        _: &[&str],
        _: &str,
        _: Duration,
    ) -> std::result::Result<ExecResult, IsolationError> {
        panic!("owner lifecycle must not dispatch a command")
    }
    fn spawn_background(&self, _: &str) -> String {
        panic!("unexpected background command")
    }
    fn spawn_pty(
        &self,
        _: &str,
        _: u16,
        _: u16,
    ) -> std::result::Result<Arc<axocoatl_isolation::pty::PtyTerminal>, String> {
        Err("not used".into())
    }
    fn get_terminal(&self, _: &str) -> Option<Arc<axocoatl_isolation::pty::PtyTerminal>> {
        None
    }
    fn kill_terminal(&self, _: &str) -> bool {
        panic!("owner must preserve unrelated terminals")
    }
    fn list_terminals(&self) -> Vec<(String, String, bool)> {
        vec![("existing".into(), "shell".into(), true)]
    }
    fn list_tasks(&self) -> Vec<BgTask> {
        vec![]
    }
    fn with_root(&self, _: &Path) -> Arc<dyn Sandbox> {
        panic!("owner must retain its exact runtime")
    }
    async fn stop(&self) {
        panic!("best-effort stop is not cleanup evidence")
    }
    async fn stop_checked(&self) -> std::result::Result<(), IsolationError> {
        self.stop_calls.fetch_add(1, Ordering::SeqCst);
        self.stop_started.notify_one();
        if self.block_stop.load(Ordering::SeqCst) {
            self.allow_stop.notified().await;
        }
        if self.fail_stop.load(Ordering::SeqCst) {
            return Err(IsolationError::Io(std::io::Error::other(
                "controlled cleanup failure",
            )));
        }
        Ok(())
    }
}

struct Fixture {
    owner: SessionRepositoryOwner,
    sandbox: Arc<ControlledSandbox>,
    operation: Arc<AsyncMutex<()>>,
    shutdown: tokio::sync::watch::Sender<bool>,
    _canonical: Option<SessionExecutionStore>,
    _data: tempfile::TempDir,
    _workspace: tempfile::TempDir,
}

/// A Session with an actual native creation origin, so its stores can enter
/// the registry and Begin through the checked path.
async fn fixture() -> Fixture {
    fixture_with_origin(None, false, true).await
}

async fn fixture_with_legacy_turn(legacy_turn_id: Option<&str>) -> Fixture {
    fixture_with_legacy_turn_visibility(legacy_turn_id, false).await
}

async fn fixture_with_legacy_turn_visibility(
    legacy_turn_id: Option<&str>,
    hidden: bool,
) -> Fixture {
    fixture_with_origin(legacy_turn_id, hidden, false).await
}

async fn fixture_with_origin(legacy_turn_id: Option<&str>, hidden: bool, native: bool) -> Fixture {
    let data = tempfile::tempdir().unwrap();
    let workspace_dir = tempfile::tempdir().unwrap();
    let data_root = SecureDir::open(data.path()).unwrap();
    let mut workspaces = WorkspaceStore::new_in_secure(&data_root, "workspaces").unwrap();
    let workspace = workspaces
        .register(workspace_dir.path(), Some("Repository"))
        .unwrap();
    let mut sessions = SessionStore::new_in_secure(&data_root, "sessions").unwrap();
    let native_guard = native.then(|| {
        Arc::new(
            LegacyFormatOwnership::acquire(data.path())
                .unwrap()
                .upgrade()
                .unwrap(),
        )
    });
    let (session, creation) = if let Some(guard) = &native_guard {
        let (session, receipt) = sessions
            .create_native_with_environment(
                guard,
                "Owned session",
                &workspace.id,
                &workspace.canonical_path,
                SessionMode::SingleAgent {
                    agent_id: "agent".into(),
                },
                vec![],
                vec![],
                None,
                None,
                false,
                true,
            )
            .unwrap();
        (session, Some(receipt))
    } else {
        let session = sessions
            .create_with_environment(
                "Owned session",
                &workspace.id,
                &workspace.canonical_path,
                SessionMode::SingleAgent {
                    agent_id: "agent".into(),
                },
                vec![],
                vec![],
                None,
                None,
                false,
                true,
            )
            .unwrap();
        (session, None)
    };
    // The creation receipt binds the untouched Session record. Register it
    // before the environment-ready transition, as the real host does.
    let native_canonical = creation.map(|receipt| {
        let mut canonical = SessionExecutionStore::open(
            native_guard.as_ref().unwrap().clone(),
            receipt.owner().clone(),
        )
        .unwrap();
        canonical.record_native_origin(&receipt).unwrap();
        canonical
    });
    let runtime = SessionRuntimeIdentity {
        backend: "podman".into(),
        id: session.id.clone(),
        remote_root: None,
        control_plane: None,
        data_plane_domain: None,
        authority_fingerprint: None,
        ownership_token: None,
        cleanup_confirmed: false,
    };
    let session = sessions
        .set_environment(
            &session.id,
            SessionEnvironmentState::Ready,
            None,
            Some(runtime.clone()),
            vec![],
            None,
        )
        .unwrap();
    if let Some(turn_id) = legacy_turn_id {
        use axocoatl_session::{
            BeginSessionTurn, SessionTurnLifecycle, SessionTurnStore, TransitionSessionTurn,
        };
        let mut legacy = SessionTurnStore::open_in_secure(&data_root, "session-history").unwrap();
        legacy
            .begin(BeginSessionTurn {
                turn_id: Some(turn_id.into()),
                session_id: session.id.clone(),
                user_input: "Original legacy request".into(),
                agent_id: Some("agent".into()),
                model: None,
                context: vec![],
                idempotency_key: None,
                metadata: serde_json::Map::new(),
            })
            .unwrap();
        legacy
            .transition(
                turn_id,
                "legacy-completed",
                TransitionSessionTurn {
                    status: SessionTurnLifecycle::Completed,
                    final_output: Some("Original legacy answer".into()),
                    error: None,
                    metadata: serde_json::Map::new(),
                },
            )
            .unwrap();
        if hidden {
            legacy
                .rewind(&session.id, None, "hide-before-native")
                .unwrap();
        }
        // The real source writer is dropped before acquiring upgraded ownership.
    }
    let guard = native_guard.unwrap_or_else(|| {
        Arc::new(
            LegacyFormatOwnership::acquire(data.path())
                .unwrap()
                .upgrade()
                .unwrap(),
        )
    });
    let mut canonical = native_canonical.unwrap_or_else(|| {
        SessionExecutionStore::open(
            guard,
            ExecutionStoreOwner {
                workspace_id: workspace.id.clone(),
                session_id: SessionId::new(session.id.clone()).unwrap(),
            },
        )
        .unwrap()
    });
    canonical.verify_data_root(&data_root).unwrap();
    if legacy_turn_id.is_some() {
        use axocoatl_session::execution_content::ExecutionContentStore;
        use axocoatl_session::execution_namespace::ExecutionComponent;
        let mut content = ExecutionContentStore::open_owned(
            canonical
                .component_namespace(ExecutionComponent::ExecutionContent)
                .unwrap(),
        )
        .unwrap();
        let retained = content
            .retain_legacy_history(&canonical.legacy_history_snapshot().unwrap())
            .unwrap();
        canonical.seal_legacy_history(&retained).unwrap();
    }
    let identity = canonical.identity().unwrap();
    validate_session_owner(&session, &identity).unwrap();
    let workspace_root = SecureDir::open(&workspace.canonical_path).unwrap();
    let sandbox = Arc::new(ControlledSandbox::new(
        &workspace.canonical_path,
        "container-incarnation-1",
    ));
    let registered: Arc<dyn Sandbox> = sandbox.clone();
    let operation = Arc::new(AsyncMutex::new(()));
    let operation_guard = operation.clone().lock_owned().await;
    let (shutdown, receiver) = tokio::sync::watch::channel(false);
    let owner = SessionRepositoryOwner {
        inner: Arc::new(RepositoryOwnerInner {
            attempt: None,
            identity,
            metadata: SessionRepositoryMetadata {
                workspace_id: workspace.id,
                session_id: session.id.clone(),
                environment_generation: session.environment.generation,
                backend: runtime.backend.clone(),
                runtime_id: runtime.id.clone(),
                execution_identity: sandbox.incarnation.clone(),
                runtime_root: workspace.canonical_path,
                host_workspace_inode: workspace_root.inode_identity().unwrap(),
            },
            runtime,
            sandbox: registered.clone(),
            data_root,
            workspace_root,
            sessions: Arc::new(AsyncMutex::new(sessions)),
            workspaces: Arc::new(AsyncMutex::new(workspaces)),
            sandboxes: Arc::new(AsyncMutex::new(HashMap::from([(session.id, registered)]))),
            start: Arc::new(AsyncMutex::new(())),
            shutdown: receiver,
            workspace_operation: Mutex::new(Some(operation_guard)),
            workspace_gate: operation.clone(),
            execution: Arc::new(AsyncMutex::new(())),
            state: Mutex::new(ExecutionState::default()),
            changed: Notify::new(),
        }),
    };
    owner.validate_current().await.unwrap();
    Fixture {
        owner,
        sandbox,
        operation,
        shutdown,
        _canonical: Some(canonical),
        _data: data,
        _workspace: workspace_dir,
    }
}

#[tokio::test]
async fn idle_retirement_transfers_workspace_guard_without_stopping_the_runtime() {
    let f = fixture().await;
    let operation = f.owner.retire_idle().unwrap();
    assert!(f.operation.try_lock().is_err());
    assert!(f.owner.execution_lease().await.is_err());
    assert_eq!(f.sandbox.stop_calls.load(Ordering::SeqCst), 0);
    assert_eq!(f.owner.sandbox().list_terminals().len(), 1);
    drop(operation);
    assert!(f.operation.try_lock().is_ok());
    assert!(f.owner.retire_idle().is_err());
}

#[tokio::test]
async fn lifecycle_wait_observes_active_predispatch_drop_and_keeps_timeout_ownership() {
    let f = fixture().await;
    let lease = f.owner.execution_lease().await.unwrap();
    assert!(f
        .owner
        .wait_for_execution_boundary(Duration::from_millis(1))
        .await
        .is_err());
    assert!(f.operation.try_lock().is_err());
    assert!(f.owner.retire_idle().is_err());
    let owner = f.owner.clone();
    let waiter = tokio::spawn(async move {
        owner
            .wait_for_execution_boundary(Duration::from_secs(1))
            .await
    });
    drop(lease);
    waiter.await.unwrap().unwrap();
    let operation = f.owner.retire_idle().unwrap();
    assert_eq!(f.sandbox.stop_calls.load(Ordering::SeqCst), 0);
    drop(operation);
    assert!(f.operation.try_lock().is_ok());
}

#[tokio::test]
async fn cancelled_start_lock_wait_wakes_lifecycle_and_never_becomes_a_command() {
    let f = fixture().await;
    let start = f.owner.inner.start.clone().lock_owned().await;
    let owner = f.owner.clone();
    let preparation = tokio::spawn(async move { owner.execution_lease().await });
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if f.owner.inner.state.lock().unwrap().active.is_some() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!f.owner.execution_is_idle().unwrap());
    preparation.abort();
    assert!(matches!(preparation.await, Err(error) if error.is_cancelled()));
    f.owner
        .wait_for_execution_boundary(Duration::from_secs(1))
        .await
        .unwrap();
    assert!(f.owner.unresolved_execution().unwrap().is_none());
    drop(start);
    drop(f.owner.retire_idle().unwrap());
    assert!(f.operation.try_lock().is_ok());
    assert_eq!(f.sandbox.stop_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn lifecycle_unknown_cleanup_retains_failure_and_transfers_gate_only_after_checked_stop() {
    let f = fixture().await;
    let id = arm_and_drop(&f.owner).await;
    f.owner
        .wait_for_execution_boundary(Duration::from_secs(1))
        .await
        .unwrap();
    assert!(!f.owner.execution_is_idle().unwrap());
    assert!(f.owner.retire_idle().is_err());
    f.sandbox.fail_stop.store(true, Ordering::SeqCst);
    assert!(f.owner.cleanup_for_lifecycle().await.is_err());
    assert_held(&f, &id);
    f.sandbox.fail_stop.store(false, Ordering::SeqCst);
    let operation = f.owner.cleanup_for_lifecycle().await.unwrap();
    assert!(f.owner.unresolved_execution().unwrap().is_none());
    assert!(f.owner.inner.start.try_lock().is_ok());
    assert!(f.operation.try_lock().is_err());
    assert!(f.owner.execution_lease().await.is_err());
    drop(operation);
    assert!(f.operation.try_lock().is_ok());
}

async fn arm_and_drop(owner: &SessionRepositoryOwner) -> String {
    let mut lease = owner.execution_lease().await.unwrap();
    let id = lease.id().to_owned();
    lease.mark_dispatched().unwrap();
    drop(lease);
    id
}

fn assert_held(fixture: &Fixture, id: &str) {
    assert_eq!(
        fixture.owner.unresolved_execution().unwrap().as_deref(),
        Some(id)
    );
    assert!(fixture.operation.try_lock().is_err());
    assert!(fixture.owner.inner.start.try_lock().is_err());
    assert!(fixture.owner.inner.execution.try_lock().is_err());
}

#[tokio::test]
async fn undispatched_drop_releases_execution_but_keeps_workspace_ownership() {
    let f = fixture().await;
    let first = f.owner.execution_lease().await.unwrap();
    assert!(f.owner.execution_lease().await.is_err());
    assert!(f.owner.cleanup_checked().await.is_err());
    drop(first);
    assert!(f.owner.unresolved_execution().unwrap().is_none());
    assert!(f.owner.inner.start.try_lock().is_ok());
    assert!(f.owner.inner.execution.try_lock().is_ok());
    assert!(f.operation.try_lock().is_err());
    drop(f.owner.execution_lease().await.unwrap());
    assert_eq!(f.sandbox.stop_calls.load(Ordering::SeqCst), 0);
    assert_eq!(f.sandbox.list_terminals().len(), 1);
}

#[tokio::test]
async fn armed_drop_and_failed_cleanup_retain_all_gates_until_exact_success() {
    let f = fixture().await;
    let id = arm_and_drop(&f.owner).await;
    let retained_owner = f.owner.clone();
    assert_held(&f, &id);
    assert!(retained_owner.execution_lease().await.is_err());
    f.sandbox.fail_stop.store(true, Ordering::SeqCst);
    assert!(retained_owner.cleanup_checked().await.is_err());
    assert_held(&f, &id);
    assert!(
        !f.owner
            .inner
            .sessions
            .lock()
            .await
            .get(&f.owner.metadata().session_id)
            .unwrap()
            .environment
            .runtime
            .unwrap()
            .cleanup_confirmed
    );
    f.sandbox.fail_stop.store(false, Ordering::SeqCst);
    retained_owner.cleanup_checked().await.unwrap();
    assert!(f.operation.try_lock().is_ok());
    assert!(f.owner.inner.start.try_lock().is_ok());
    assert!(f.owner.inner.execution.try_lock().is_ok());
    assert!(f.owner.execution_lease().await.is_err());
    assert!(f.owner.inner.sandboxes.lock().await.is_empty());
    let replacement: Arc<dyn Sandbox> =
        Arc::new(ControlledSandbox::new(f.owner.root(), "replacement"));
    f.owner
        .inner
        .sandboxes
        .lock()
        .await
        .insert(f.owner.metadata().session_id.clone(), replacement.clone());
    retained_owner.cleanup_checked().await.unwrap();
    assert_eq!(f.sandbox.stop_calls.load(Ordering::SeqCst), 2);
    assert!(Arc::ptr_eq(
        f.owner
            .inner
            .sandboxes
            .lock()
            .await
            .get(&f.owner.metadata().session_id)
            .unwrap(),
        &replacement
    ));
}

#[tokio::test]
async fn cancelled_checked_stop_preserves_unknown_and_can_be_explicitly_retried() {
    let f = fixture().await;
    let id = arm_and_drop(&f.owner).await;
    f.sandbox.block_stop.store(true, Ordering::SeqCst);
    let owner = f.owner.clone();
    let task = tokio::spawn(async move { owner.cleanup_checked().await });
    tokio::time::timeout(Duration::from_secs(2), f.sandbox.stop_started.notified())
        .await
        .unwrap();
    assert!(f.owner.cleanup_checked().await.is_err());
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_held(&f, &id);
    f.sandbox.block_stop.store(false, Ordering::SeqCst);
    f.owner.cleanup_checked().await.unwrap();
    assert_eq!(f.sandbox.stop_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn cancellation_after_persisted_cleanup_retries_eviction_without_repeating_stop() {
    let f = fixture().await;
    let id = arm_and_drop(&f.owner).await;
    f.sandbox.block_stop.store(true, Ordering::SeqCst);
    let owner = f.owner.clone();
    let task = tokio::spawn(async move { owner.cleanup_checked().await });
    tokio::time::timeout(Duration::from_secs(2), f.sandbox.stop_started.notified())
        .await
        .unwrap();
    // Validation already inspected the map. Hold eviction after the exact stop
    // and durable tombstone, then cancel the production cleanup future.
    let map = f.owner.inner.sandboxes.lock().await;
    f.sandbox.allow_stop.notify_one();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if f.owner
                .inner
                .sessions
                .lock()
                .await
                .get(&f.owner.metadata().session_id)
                .unwrap()
                .environment
                .runtime
                .unwrap()
                .cleanup_confirmed
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    drop(map);
    assert_held(&f, &id);
    f.owner.cleanup_checked().await.unwrap();
    assert_eq!(f.sandbox.stop_calls.load(Ordering::SeqCst), 1);
    assert!(f.operation.try_lock().is_ok());
}

#[tokio::test]
async fn exact_cleanup_tombstone_and_missing_map_finish_the_retained_owner() {
    let f = fixture().await;
    arm_and_drop(&f.owner).await;
    let cleanup = f.owner.begin_cleanup().unwrap().unwrap();
    assert!(!f.owner.validate_resource(true).await.unwrap());
    f.sandbox.stop_checked().await.unwrap();
    f.owner
        .inner
        .sessions
        .lock()
        .await
        .confirm_environment_runtime_cleanup(
            &f.owner.metadata().session_id,
            &f.owner.metadata().runtime_id,
        )
        .unwrap();
    f.owner
        .inner
        .sandboxes
        .lock()
        .await
        .remove(&f.owner.metadata().session_id);
    // Model the saved cleanup prefix immediately before the synchronous release.
    drop(cleanup);
    f.owner.cleanup_checked().await.unwrap();
    f.owner.cleanup_checked().await.unwrap();
    assert_eq!(f.sandbox.stop_calls.load(Ordering::SeqCst), 1);
    assert!(f.operation.try_lock().is_ok());
}

#[tokio::test]
async fn replacement_runtime_is_never_cleaned_or_evicted_by_an_old_owner() {
    for already_cleaned in [false, true] {
        let f = fixture().await;
        let id = arm_and_drop(&f.owner).await;
        if already_cleaned {
            f.owner
                .inner
                .sessions
                .lock()
                .await
                .confirm_environment_runtime_cleanup(
                    &f.owner.metadata().session_id,
                    &f.owner.metadata().runtime_id,
                )
                .unwrap();
        }
        let replacement: Arc<dyn Sandbox> =
            Arc::new(ControlledSandbox::new(f.owner.root(), "replacement"));
        f.owner
            .inner
            .sandboxes
            .lock()
            .await
            .insert(f.owner.metadata().session_id.clone(), replacement.clone());
        assert!(f.owner.cleanup_checked().await.is_err());
        assert_held(&f, &id);
        assert_eq!(f.sandbox.stop_calls.load(Ordering::SeqCst), 0);
        assert!(Arc::ptr_eq(
            f.owner
                .inner
                .sandboxes
                .lock()
                .await
                .get(&f.owner.metadata().session_id)
                .unwrap(),
            &replacement
        ));
    }
}

#[tokio::test]
async fn shutdown_and_closed_session_refuse_dispatch_but_allow_exact_explicit_cleanup() {
    let f = fixture().await;
    let mut lease = f.owner.execution_lease().await.unwrap();
    f.shutdown.send(true).unwrap();
    assert!(lease.mark_dispatched().is_err());
    drop(lease);
    assert!(f.owner.execution_lease().await.is_err());
    f.shutdown.send(false).unwrap();
    let id = arm_and_drop(&f.owner).await;
    f.shutdown.send(true).unwrap();
    f.owner
        .inner
        .sessions
        .lock()
        .await
        .close(&f.owner.metadata().session_id)
        .unwrap();
    assert_held(&f, &id);
    f.owner.cleanup_checked().await.unwrap();
    assert_eq!(f.sandbox.stop_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn canonical_owner_and_preparation_identity_cannot_be_substituted() {
    let f = fixture().await;
    let session = f
        .owner
        .inner
        .sessions
        .lock()
        .await
        .get(&f.owner.metadata().session_id)
        .unwrap();
    let mut wrong = session.clone();
    wrong.workspace_id = "another-workspace".into();
    assert!(f.owner.validate_session_binding(&wrong, false).is_err());
    wrong = session.clone();
    wrong.id = "another-session".into();
    assert!(f.owner.validate_session_binding(&wrong, false).is_err());
    wrong = session.clone();
    wrong.environment.generation += 1;
    assert!(f.owner.validate_session_binding(&wrong, true).is_err());
    wrong = session.clone();
    wrong
        .environment
        .runtime
        .as_mut()
        .unwrap()
        .authority_fingerprint = Some("different-authority".into());
    assert!(f.owner.validate_session_binding(&wrong, true).is_err());
    f.owner
        .inner
        .sessions
        .lock()
        .await
        .set_environment(
            &session.id,
            SessionEnvironmentState::Preparing,
            None,
            session.environment.runtime,
            vec![],
            None,
        )
        .unwrap();
    assert!(f.owner.execution_lease().await.is_err());
    assert_eq!(f.sandbox.stop_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn replaced_workspace_inode_refuses_execution_without_touching_a_runtime() {
    let f = fixture().await;
    let original = f.owner.inner.workspace_root.path().to_path_buf();
    let moved = original.with_extension("retained-test-inode");
    std::fs::rename(&original, &moved).unwrap();
    std::fs::create_dir(&original).unwrap();
    let refused = f.owner.execution_lease().await.is_err();
    std::fs::remove_dir(&original).unwrap();
    std::fs::rename(&moved, &original).unwrap();
    assert!(refused);
    assert_eq!(f.sandbox.stop_calls.load(Ordering::SeqCst), 0);
    drop(f.owner.execution_lease().await.unwrap());
}

#[tokio::test]
async fn lifecycle_stop_before_supervisor_binding_closes_admission_without_claiming_cleanup() {
    let f = fixture().await;
    let mut lease = f.owner.execution_lease().await.unwrap();
    {
        let state = f.owner.inner.state.lock().unwrap();
        lease.require_bindable(&state).unwrap();
    }
    assert!(f.owner.request_supervised_stop().unwrap().is_none());
    {
        let state = f.owner.inner.state.lock().unwrap();
        assert!(lease.require_bindable(&state).is_err());
    }
    assert!(lease.mark_dispatched().is_err());
    drop(lease);
    assert!(f.owner.execution_lease().await.is_err());
    assert!(f.owner.unresolved_execution().unwrap().is_none());
    assert!(f.operation.try_lock().is_err());
    assert_eq!(f.sandbox.stop_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn an_armed_unbound_effect_cannot_acquire_supervisor_attribution_after_dispatch() {
    let f = fixture().await;
    let mut lease = f.owner.execution_lease().await.unwrap();
    lease.mark_dispatched().unwrap();
    {
        let state = f.owner.inner.state.lock().unwrap();
        assert!(lease.require_bindable(&state).is_err());
        assert!(state.supervised.is_none());
    }
    let id = lease.id().to_owned();
    drop(lease);
    assert!(f.owner.request_supervised_stop().unwrap().is_none());
    assert_held(&f, &id);
    // No opaque process proof was fabricated for an unsupervised effect.
    // Only the existing explicit checked cleanup path can release this owner.
    f.owner.cleanup_checked().await.unwrap();
    assert_eq!(f.sandbox.stop_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_dropped_dispatched_lease_stays_held_without_a_settlement() {
    let f = fixture().await;
    let mut lease = f.owner.execution_lease().await.unwrap();
    lease.mark_dispatched().unwrap();
    {
        let state = f.owner.inner.state.lock().unwrap();
        assert_eq!(state.active.as_deref(), Some(lease.id()));
        assert!(state.supervised.is_none());
    }
    let id = lease.id().to_owned();
    drop(lease);
    assert_held(&f, &id);
    assert_eq!(f.sandbox.stop_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn an_abandoned_cleanup_never_clears_a_requested_stop() {
    let f = fixture().await;
    let id = arm_and_drop(&f.owner).await;
    f.owner.request_supervised_stop().unwrap();
    let cleanup = f.owner.begin_cleanup().unwrap().unwrap();
    {
        let state = f.owner.inner.state.lock().unwrap();
        assert!(state.admission_closed);
        assert!(state.cleaning);
    }
    assert_held(&f, &id);
    drop(cleanup);
    {
        let state = f.owner.inner.state.lock().unwrap();
        assert!(state.admission_closed);
    }
    assert!(f.owner.execution_lease().await.is_err());
    assert_held(&f, &id);
    assert_eq!(f.sandbox.stop_calls.load(Ordering::SeqCst), 0);
}

fn controller(fixture: &mut Fixture) -> crate::session_dispatch::SessionDispatchController {
    controller_with_tools(fixture, &[])
}

fn controller_with_tools(
    fixture: &mut Fixture,
    tools: &[&str],
) -> crate::session_dispatch::SessionDispatchController {
    use axocoatl_session::control_authority::ExecutionProfile;
    use axocoatl_session::execution_content::{
        ActivationEvidenceContent, ExecutionContentStore, ExecutionRequestContent,
    };
    use axocoatl_session::execution_namespace::ExecutionComponent;
    use axocoatl_session::turn_contract::*;
    let mut canonical = fixture._canonical.take().unwrap();
    let mut content = ExecutionContentStore::open_owned(
        canonical
            .component_namespace(ExecutionComponent::ExecutionContent)
            .unwrap(),
    )
    .unwrap();
    let definition_id = AgentDefinitionId::new("lifecycle-definition").unwrap();
    let definition = content
        .retain_activation_evidence(ActivationEvidenceContent::Definition {
            definition_id: definition_id.clone(),
            revision: 1,
            profile: ExecutionProfile {
                definition: definition_id.as_str().into(),
                provider: "local".into(),
                model: "model".into(),
                isolation: "in-process".into(),
                tools: tools.iter().map(|tool| (*tool).to_owned()).collect(),
                write_scope: None,
            },
            configuration: "{}".into(),
        })
        .unwrap();
    let turn_id = LogicalTurnId::new("lifecycle-turn").unwrap();
    let request = content
        .retain_request(ExecutionRequestContent {
            turn_id: turn_id.clone(),
            recorded_at_unix_ms: 1,
            display_input: "Retained request".into(),
            effective_input: "Retained request".into(),
            context: vec![],
            target_definition: None,
            model: None,
        })
        .unwrap();
    canonical
        .begin_with_request(
            TurnContractEnvelope {
                schema_version: TURN_CONTRACT_SCHEMA_VERSION,
                command_id: CommandId::new("lifecycle-begin").unwrap(),
                expected_revision: 0,
                session_id: canonical.owner().session_id.clone(),
                turn_id: turn_id.clone(),
                event: TurnContractEvent::Begin {
                    epoch_id: ExecutionEpochId::new("lifecycle-epoch").unwrap(),
                    predecessor: None,
                    graph: TurnGraphSnapshot {
                        snapshot_id: GraphSnapshotId::new("lifecycle-graph").unwrap(),
                        revision: 1,
                        nodes: vec![GraphNode {
                            node_id: TurnNodeId::new("node").unwrap(),
                            slot_id: SessionTeamSlotId::new("slot").unwrap(),
                            definition: DefinitionSnapshotRef {
                                definition_id,
                                snapshot: definition.reference().clone(),
                            },
                            conversation_id: NodeConversationId::new("conversation").unwrap(),
                            starting_savepoint: ConversationSavepoint::Empty,
                            required: true,
                        }],
                        dependencies: vec![],
                        conditions: vec![],
                    },
                },
            },
            &request,
        )
        .unwrap();
    drop(content);
    crate::session_dispatch::SessionDispatchController::open(canonical, turn_id).unwrap()
}

/// Retain the fixture's stores and Begin their first turn through the
/// registry's checked first-Begin path, which registers the controller with
/// the fixture's owner as a native Session's first turn does.
fn begin_registered(
    registry: &crate::bootstrap::session_dispatch::SessionDispatchRegistry,
    fixture: &mut Fixture,
) -> (
    crate::session_dispatch::SessionDispatchController,
    axocoatl_session::turn_contract::EvidenceRef,
) {
    begin_registered_with_tools(registry, fixture, &[])
}

fn begin_registered_with_tools(
    registry: &crate::bootstrap::session_dispatch::SessionDispatchRegistry,
    fixture: &mut Fixture,
    tools: &[&str],
) -> (
    crate::session_dispatch::SessionDispatchController,
    axocoatl_session::turn_contract::EvidenceRef,
) {
    use axocoatl_session::control_authority::ExecutionProfile;
    use axocoatl_session::execution_content::{
        ActivationEvidenceContent, ExecutionContentStore, ExecutionRequestContent,
    };
    use axocoatl_session::turn_contract::*;
    let canonical = fixture._canonical.take().unwrap();
    let session_id = canonical.owner().session_id.as_str().to_owned();
    let token = registry
        .retain_existing_session(&mut Some(held_stores(canonical)))
        .unwrap();
    let team = registry.session_team_token(&session_id).unwrap();
    let definition_id = AgentDefinitionId::new("lifecycle-definition").unwrap();
    let definition = registry
        .with_session_team_stores(&team, |_, content: &mut ExecutionContentStore, _| {
            Ok(content
                .retain_activation_evidence(ActivationEvidenceContent::Definition {
                    definition_id: definition_id.clone(),
                    revision: 1,
                    profile: ExecutionProfile {
                        definition: definition_id.as_str().into(),
                        provider: "local".into(),
                        model: "model".into(),
                        isolation: "in-process".into(),
                        tools: tools.iter().map(|tool| (*tool).to_owned()).collect(),
                        write_scope: None,
                    },
                    configuration: "{}".into(),
                })
                .unwrap())
        })
        .unwrap();
    let turn_id = LogicalTurnId::new("lifecycle-turn").unwrap();
    let spec = crate::session_dispatch::SuccessorTurn {
        command_id: CommandId::new("lifecycle-begin").unwrap(),
        turn_id: turn_id.clone(),
        epoch_id: ExecutionEpochId::new("lifecycle-epoch").unwrap(),
        graph: TurnGraphSnapshot {
            snapshot_id: GraphSnapshotId::new("lifecycle-graph").unwrap(),
            revision: 1,
            nodes: vec![GraphNode {
                node_id: TurnNodeId::new("node").unwrap(),
                slot_id: SessionTeamSlotId::new("slot").unwrap(),
                definition: DefinitionSnapshotRef {
                    definition_id,
                    snapshot: definition.reference().clone(),
                },
                conversation_id: NodeConversationId::new("conversation").unwrap(),
                starting_savepoint: ConversationSavepoint::Empty,
                required: true,
            }],
            dependencies: vec![],
            conditions: vec![],
        },
        request: ExecutionRequestContent {
            turn_id,
            recorded_at_unix_ms: 1,
            display_input: "Retained request".into(),
            effective_input: "Retained request".into(),
            context: vec![],
            target_definition: None,
            model: None,
        },
    };
    registry
        .begin_first_turn_checked(&token, fixture.owner.clone(), spec, |_, _, _| Ok(()))
        .unwrap()
}

/// Canonical, content and activation stores opened from one canonical owner.
fn held_stores(canonical: SessionExecutionStore) -> crate::session_dispatch::RetainedSessionStores {
    use axocoatl_memory::activation_state::ActivationStateStore;
    use axocoatl_session::execution_content::ExecutionContentStore;
    use axocoatl_session::execution_namespace::ExecutionComponent;
    let content = ExecutionContentStore::open_owned(
        canonical
            .component_namespace(ExecutionComponent::ExecutionContent)
            .unwrap(),
    )
    .unwrap();
    let memory = ActivationStateStore::open_owned(
        canonical
            .component_namespace(ExecutionComponent::ActivationState)
            .unwrap(),
    )
    .unwrap();
    crate::session_dispatch::RetainedSessionStores {
        canonical,
        content,
        memory,
    }
}

fn closed_successor(
    controller: &crate::session_dispatch::SessionDispatchController,
) -> crate::session_dispatch::SuccessorTurn {
    use axocoatl_session::execution_content::ExecutionRequestContent;
    use axocoatl_session::turn_contract::*;
    let snapshot = controller.snapshot().unwrap();
    controller
        .close_and_promote(TurnContractEnvelope {
            schema_version: TURN_CONTRACT_SCHEMA_VERSION,
            command_id: CommandId::new("lifecycle-cancel").unwrap(),
            expected_revision: snapshot.contract().revision(),
            session_id: snapshot.owner().session_id.clone(),
            turn_id: snapshot.turn_id().clone(),
            event: TurnContractEvent::Close {
                closure: TurnClosure::Cancelled,
            },
        })
        .unwrap();
    let mut graph = snapshot.contract().graph().unwrap().clone();
    graph.snapshot_id = GraphSnapshotId::new("successor-graph").unwrap();
    crate::session_dispatch::SuccessorTurn {
        command_id: CommandId::new("successor-begin").unwrap(),
        turn_id: LogicalTurnId::new("successor-turn").unwrap(),
        epoch_id: ExecutionEpochId::new("successor-epoch").unwrap(),
        graph,
        request: ExecutionRequestContent {
            turn_id: LogicalTurnId::new("successor-turn").unwrap(),
            recorded_at_unix_ms: 2,
            display_input: "Next request".into(),
            effective_input: "Next request".into(),
            context: vec![],
            target_definition: None,
            model: None,
        },
    }
}

#[tokio::test]
async fn registry_close_reopen_without_entry_cannot_acknowledge_a_later_registration() {
    use crate::bootstrap::session_dispatch::SessionDispatchRegistry;
    let registry = SessionDispatchRegistry::default();
    let mut fixture = fixture().await;
    let session_id = fixture.owner.metadata().session_id.clone();
    let stale = registry
        .prepare_session_cleanup(&session_id, Duration::from_secs(1))
        .await
        .unwrap();
    registry.complete_session_cleanup(&stale).unwrap();
    registry.reopen_session(&session_id).unwrap();
    begin_registered(&registry, &mut fixture);
    assert!(registry.complete_session_cleanup(&stale).is_err());
    assert!(registry.forget_deleted_session(&session_id).is_err());
    let mut current = registry
        .prepare_session_cleanup(&session_id, Duration::from_secs(1))
        .await
        .unwrap();
    assert!(fixture.operation.try_lock().is_err());
    let gate = current.take_operation().unwrap();
    registry.complete_session_cleanup(&current).unwrap();
    registry.forget_deleted_session(&session_id).unwrap();
    drop(gate);
    assert!(fixture.operation.try_lock().is_ok());
    registry.reopen_session(&session_id).unwrap();
    assert_eq!(fixture.sandbox.stop_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn registry_inspection_uses_exact_retained_controller_without_changing_revision() {
    use crate::bootstrap::session_dispatch::SessionDispatchRegistry;
    let registry = SessionDispatchRegistry::default();
    let mut fixture = fixture().await;
    let session_id = fixture.owner.metadata().session_id.clone();
    let (controller, _) = begin_registered(&registry, &mut fixture);
    let before = controller.snapshot().unwrap();
    let turn_id = before.turn_id().as_str().to_owned();
    let view = registry
        .control_plane(&session_id, &turn_id)
        .unwrap()
        .unwrap();
    assert_eq!(view.history_version, "execution_v2");
    assert_eq!(view.session_id, session_id);
    assert_eq!(view.turn_id, turn_id);
    assert!(registry
        .control_plane("foreign-session", &turn_id)
        .unwrap()
        .is_none());
    assert!(registry
        .control_plane(&session_id, "foreign-turn")
        .unwrap()
        .is_none());
    assert_eq!(
        controller.snapshot().unwrap().contract().revision(),
        before.contract().revision()
    );
    // Inspection holds no execution ticket and cannot strand ordinary Close.
    let mut cleanup = registry
        .prepare_session_cleanup(&session_id, Duration::from_secs(1))
        .await
        .unwrap();
    drop(cleanup.take_operation());
    registry.complete_session_cleanup(&cleanup).unwrap();
    assert!(registry
        .control_plane(&session_id, &turn_id)
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn registry_unknown_and_failed_cleanup_keep_canonical_and_actual_owner_until_retry() {
    use crate::bootstrap::session_dispatch::SessionDispatchRegistry;
    let registry = SessionDispatchRegistry::default();
    let mut fixture = fixture().await;
    let session_id = fixture.owner.metadata().session_id.clone();
    begin_registered(&registry, &mut fixture);
    let id = arm_and_drop(&fixture.owner).await;
    fixture.sandbox.fail_stop.store(true, Ordering::SeqCst);
    assert!(registry
        .prepare_session_cleanup(&session_id, Duration::from_secs(1))
        .await
        .is_err());
    assert_held(&fixture, &id);
    assert!(registry.reopen_session(&session_id).is_err());
    assert!(
        axocoatl_session::execution_ownership::UpgradedFormatOwnership::open(fixture._data.path())
            .is_err()
    );
    fixture.sandbox.fail_stop.store(false, Ordering::SeqCst);
    let mut ticket = registry
        .prepare_session_cleanup(&session_id, Duration::from_secs(1))
        .await
        .unwrap();
    assert!(fixture.operation.try_lock().is_err());
    let gate = ticket.take_operation().unwrap();
    // Cancelling the host lifecycle action cannot remove canonical ownership.
    drop(ticket);
    drop(gate);
    // The retired registration also retains the actual Workspace mutex. A
    // peer writer cannot enter between the failed Close and its retry.
    assert!(fixture.operation.try_lock().is_err());
    assert!(
        axocoatl_session::execution_ownership::UpgradedFormatOwnership::open(fixture._data.path())
            .is_err()
    );
    let retry = registry
        .prepare_session_cleanup(&session_id, Duration::from_secs(1))
        .await
        .unwrap();
    registry.complete_session_cleanup(&retry).unwrap();
    drop(retry);
    assert!(
        axocoatl_session::execution_ownership::UpgradedFormatOwnership::open(fixture._data.path())
            .is_ok()
    );
    assert_eq!(fixture.sandbox.stop_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn registry_cancelled_cleanup_keeps_unknown_execution_and_allows_exact_retry() {
    use crate::bootstrap::session_dispatch::SessionDispatchRegistry;
    let registry = Arc::new(SessionDispatchRegistry::default());
    let mut fixture = fixture().await;
    let session_id = fixture.owner.metadata().session_id.clone();
    begin_registered(&registry, &mut fixture);
    let id = arm_and_drop(&fixture.owner).await;
    fixture.sandbox.block_stop.store(true, Ordering::SeqCst);
    let registry_task = registry.clone();
    let task_session = session_id.clone();
    let task = tokio::spawn(async move {
        registry_task
            .prepare_session_cleanup(&task_session, Duration::from_secs(1))
            .await
    });
    fixture.sandbox.stop_started.notified().await;
    task.abort();
    assert!(matches!(task.await, Err(error) if error.is_cancelled()));
    assert_held(&fixture, &id);
    assert!(
        axocoatl_session::execution_ownership::UpgradedFormatOwnership::open(fixture._data.path())
            .is_err()
    );
    fixture.sandbox.block_stop.store(false, Ordering::SeqCst);
    let mut ticket = registry
        .prepare_session_cleanup(&session_id, Duration::from_secs(1))
        .await
        .unwrap();
    drop(ticket.take_operation().unwrap());
    registry.complete_session_cleanup(&ticket).unwrap();
    assert!(fixture.operation.try_lock().is_ok());
}

#[tokio::test]
async fn registry_successor_preserves_idle_capability_and_refuses_consuming_handoff() {
    use crate::bootstrap::session_dispatch::SessionDispatchRegistry;
    let registry = SessionDispatchRegistry::default();
    let mut fixture = fixture().await;
    let session_id = fixture.owner.metadata().session_id.clone();
    let (controller, _) = begin_registered(&registry, &mut fixture);
    let next = closed_successor(&controller);
    let retained_next = crate::session_dispatch::SuccessorTurn {
        command_id: next.command_id.clone(),
        turn_id: next.turn_id.clone(),
        epoch_id: next.epoch_id.clone(),
        graph: next.graph.clone(),
        request: next.request.clone(),
    };
    assert!(controller.begin_successor(next).is_err());
    assert!(
        axocoatl_session::execution_ownership::UpgradedFormatOwnership::open(fixture._data.path())
            .is_err()
    );
    registry
        .begin_native_successor_checked(&session_id, retained_next, |_, _, _| Ok(()))
        .unwrap();
    // Same owner and content capability survive advancement. No runtime restart.
    drop(fixture.owner.execution_lease().await.unwrap());
    assert!(fixture.operation.try_lock().is_err());
    let mut ticket = registry
        .prepare_session_cleanup(&session_id, Duration::from_secs(1))
        .await
        .unwrap();
    drop(ticket.take_operation().unwrap());
    registry.complete_session_cleanup(&ticket).unwrap();
    assert_eq!(fixture.sandbox.stop_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn registry_successor_storage_failure_and_unknown_lease_preserve_ownership() {
    use crate::bootstrap::session_dispatch::SessionDispatchRegistry;
    for storage_failure in [false, true] {
        let registry = SessionDispatchRegistry::default();
        let mut fixture = fixture().await;
        let session_id = fixture.owner.metadata().session_id.clone();
        let (controller, _) = begin_registered(&registry, &mut fixture);
        let next = closed_successor(&controller);
        if storage_failure {
            controller.fail_registered_successor_request_for_test();
        }
        drop(controller);
        let unknown = if storage_failure {
            None
        } else {
            Some(arm_and_drop(&fixture.owner).await)
        };
        assert!(registry
            .begin_native_successor_checked(&session_id, next, |_, _, _| Ok(()))
            .is_err());
        assert!(fixture.operation.try_lock().is_err());
        assert!(
            axocoatl_session::execution_ownership::UpgradedFormatOwnership::open(
                fixture._data.path()
            )
            .is_err()
        );
        if let Some(id) = unknown {
            assert_held(&fixture, &id);
        }
        let mut ticket = registry
            .prepare_session_cleanup(&session_id, Duration::from_secs(1))
            .await
            .unwrap();
        drop(ticket.take_operation().unwrap());
        registry.complete_session_cleanup(&ticket).unwrap();
        drop(ticket);
        assert!(
            axocoatl_session::execution_ownership::UpgradedFormatOwnership::open(
                fixture._data.path()
            )
            .is_ok()
        );
        assert_eq!(
            fixture.sandbox.stop_calls.load(Ordering::SeqCst),
            usize::from(!storage_failure)
        );
    }
}

#[tokio::test]
async fn registry_cancelled_borrowed_lifecycle_retains_parked_workspace_until_retry() {
    use crate::bootstrap::session_dispatch::SessionDispatchRegistry;
    let registry = SessionDispatchRegistry::default();
    let mut fixture = fixture().await;
    let session_id = fixture.owner.metadata().session_id.clone();
    begin_registered(&registry, &mut fixture);

    let (prepared, ready) = tokio::sync::oneshot::channel();
    {
        // This future borrows the retained owner, as daemon shutdown does.
        // Cancelling it after handoff drops both caller-held tokens.
        let lifecycle = async {
            let mut cleanup = registry
                .prepare_session_cleanup(&session_id, Duration::from_secs(1))
                .await
                .unwrap();
            let _operation = cleanup.take_operation().unwrap();
            prepared.send(()).unwrap();
            std::future::pending::<()>().await;
            registry.complete_session_cleanup(&cleanup).unwrap();
        };
        tokio::pin!(lifecycle);
        tokio::select! {
            () = &mut lifecycle => panic!("lifecycle unexpectedly completed"),
            result = ready => result.unwrap(),
        }
    }
    assert!(fixture.operation.try_lock().is_err());
    assert!(registry.require_session_reopenable(&session_id).is_err());
    assert!(registry.reopen_session(&session_id).is_err());
    assert!(
        axocoatl_session::execution_ownership::UpgradedFormatOwnership::open(fixture._data.path())
            .is_err()
    );

    let mut retry = registry
        .prepare_session_cleanup(&session_id, Duration::from_secs(1))
        .await
        .unwrap();
    let operation = retry.take_operation().unwrap();
    registry.complete_session_cleanup(&retry).unwrap();
    // Explicit success removes the registry anchor, but the successful caller
    // still owns the Workspace until its remaining lifecycle scope ends.
    assert!(fixture.operation.try_lock().is_err());
    drop(operation);
    assert!(fixture.operation.try_lock().is_ok());
    registry.require_session_reopenable(&session_id).unwrap();
    drop(retry);
    assert!(
        axocoatl_session::execution_ownership::UpgradedFormatOwnership::open(fixture._data.path())
            .is_ok()
    );
    assert_eq!(fixture.sandbox.stop_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn registry_concurrent_retry_waits_for_both_lifecycle_holders() {
    use crate::bootstrap::session_dispatch::SessionDispatchRegistry;
    let registry = Arc::new(SessionDispatchRegistry::default());
    let mut fixture = fixture().await;
    let session_id = fixture.owner.metadata().session_id.clone();
    begin_registered(&registry, &mut fixture);
    let mut first = registry
        .prepare_session_cleanup(&session_id, Duration::from_secs(1))
        .await
        .unwrap();
    let operation = first.take_operation().unwrap();

    let registry_task = registry.clone();
    let task_session = session_id.clone();
    let mut retry = tokio::spawn(async move {
        registry_task
            .prepare_session_cleanup(&task_session, Duration::from_secs(5))
            .await
    });
    assert!(tokio::time::timeout(Duration::from_millis(20), &mut retry)
        .await
        .is_err());
    drop(first);
    assert!(tokio::time::timeout(Duration::from_millis(20), &mut retry)
        .await
        .is_err());
    assert!(fixture.operation.try_lock().is_err());
    drop(operation);
    let second = tokio::time::timeout(Duration::from_secs(1), retry)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(fixture.operation.try_lock().is_err());
    registry.complete_session_cleanup(&second).unwrap();
    drop(second);
    assert!(fixture.operation.try_lock().is_ok());
    assert_eq!(fixture.sandbox.stop_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn registry_queued_cleanup_cannot_reuse_a_completed_registration() {
    use crate::bootstrap::session_dispatch::SessionDispatchRegistry;
    let registry = Arc::new(SessionDispatchRegistry::default());
    let mut fixture = fixture().await;
    let session_id = fixture.owner.metadata().session_id.clone();
    begin_registered(&registry, &mut fixture);
    let mut first = registry
        .prepare_session_cleanup(&session_id, Duration::from_secs(1))
        .await
        .unwrap();
    let operation = first.take_operation().unwrap();
    let registry_task = registry.clone();
    let task_session = session_id.clone();
    let mut queued = tokio::spawn(async move {
        registry_task
            .prepare_session_cleanup(&task_session, Duration::from_secs(5))
            .await
    });
    assert!(tokio::time::timeout(Duration::from_millis(20), &mut queued)
        .await
        .is_err());
    registry.complete_session_cleanup(&first).unwrap();
    drop(first);
    assert!(fixture.operation.try_lock().is_err());
    drop(operation);
    let mut queued = tokio::time::timeout(Duration::from_secs(1), queued)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(queued.take_operation().is_none());
    assert!(fixture.operation.try_lock().is_ok());
    registry.complete_session_cleanup(&queued).unwrap();
}

#[tokio::test]
async fn finalized_turn_releases_workspace_then_reacquires_a_fresh_owner_for_successor() {
    use crate::bootstrap::session_dispatch::SessionDispatchRegistry;
    let registry = SessionDispatchRegistry::default();
    let mut f = fixture().await;
    let session_id = f.owner.metadata().session_id.clone();
    let (controller, _) = begin_registered(&registry, &mut f);
    let next = closed_successor(&controller);
    let turn_id = controller.snapshot().unwrap().turn_id().clone();
    let revision = controller.snapshot().unwrap().contract().revision();
    drop(controller);
    registry.release_after_turn(&session_id, &turn_id).unwrap();
    registry.release_after_turn(&session_id, &turn_id).unwrap();
    assert!(
        f.owner.execution_lease().await.is_err(),
        "old owner stays retired"
    );
    assert!(registry
        .control_plane(&session_id, turn_id.as_str())
        .unwrap()
        .is_some());
    assert_eq!(f.sandbox.stop_calls.load(Ordering::SeqCst), 0);
    assert_eq!(f.sandbox.list_terminals().len(), 1);
    {
        let _ordinary_writer = f.operation.clone().try_lock_owned().unwrap();
        std::fs::write(
            f._workspace.path().join("between-turns.txt"),
            "ordinary edit",
        )
        .unwrap();
        assert!(f.owner.reacquire_between_turns().await.is_err());
    }
    let token = registry.prepare_reacquisition(&session_id).unwrap();
    let fresh = f.owner.reacquire_between_turns().await.unwrap();
    assert!(!fresh.same_owner(&f.owner));
    registry
        .complete_reacquisition(token, fresh.clone())
        .unwrap();
    assert!(f.operation.try_lock().is_err());
    assert!(f.owner.execution_lease().await.is_err());
    drop(fresh.execution_lease().await.unwrap());
    assert_eq!(
        std::fs::read_to_string(f._workspace.path().join("between-turns.txt")).unwrap(),
        "ordinary edit"
    );
    let view = registry
        .control_plane(&session_id, turn_id.as_str())
        .unwrap()
        .unwrap();
    assert_eq!(
        view.turn_revision,
        crate::session_control_plane::EvidenceValue::Available { value: revision }
    );
    registry
        .begin_native_successor_checked(&session_id, next, |_, _, _| Ok(()))
        .unwrap();
    let mut cleanup = registry
        .prepare_session_cleanup(&session_id, Duration::from_secs(1))
        .await
        .unwrap();
    drop(cleanup.take_operation());
    registry.complete_session_cleanup(&cleanup).unwrap();
    drop(cleanup);
    assert!(f.operation.try_lock().is_ok());
    assert_eq!(f.sandbox.stop_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn running_or_unknown_work_never_releases_repository_ownership() {
    use crate::bootstrap::session_dispatch::SessionDispatchRegistry;
    for unknown in [false, true] {
        let registry = SessionDispatchRegistry::default();
        let mut f = fixture().await;
        let session_id = f.owner.metadata().session_id.clone();
        let (controller, _) = begin_registered(&registry, &mut f);
        if unknown {
            closed_successor(&controller);
        }
        let turn_id = controller.snapshot().unwrap().turn_id().clone();
        drop(controller);
        if unknown {
            arm_and_drop(&f.owner).await;
        }
        assert!(registry.release_after_turn(&session_id, &turn_id).is_err());
        assert!(f.operation.try_lock().is_err());
        assert_eq!(f.sandbox.stop_calls.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn stale_reacquisition_cannot_complete_after_close_and_reopen() {
    use crate::bootstrap::session_dispatch::SessionDispatchRegistry;
    let registry = SessionDispatchRegistry::default();
    let mut f = fixture().await;
    let session_id = f.owner.metadata().session_id.clone();
    let (controller, _) = begin_registered(&registry, &mut f);
    closed_successor(&controller);
    let turn_id = controller.snapshot().unwrap().turn_id().clone();
    drop(controller);
    registry.release_after_turn(&session_id, &turn_id).unwrap();
    let stale = registry.prepare_reacquisition(&session_id).unwrap();
    let mut cleanup = registry
        .prepare_session_cleanup(&session_id, Duration::from_secs(1))
        .await
        .unwrap();
    drop(cleanup.take_operation());
    registry.complete_session_cleanup(&cleanup).unwrap();
    drop(cleanup);
    registry.reopen_session(&session_id).unwrap();
    let fresh = f.owner.reacquire_between_turns().await.unwrap();
    // Close retired the registration the stale token names. The token cannot
    // complete against the reopened Session or retain the fresh owner.
    assert!(registry
        .complete_reacquisition(stale, fresh.clone())
        .is_err());
    assert!(registry
        .control_plane(&session_id, turn_id.as_str())
        .unwrap()
        .is_none());
    assert!(
        f.operation.try_lock().is_err(),
        "the fresh owner still holds the Workspace"
    );
    drop(fresh);
    assert!(
        f.operation.try_lock().is_ok(),
        "the refused reacquisition retained nothing"
    );
    assert_eq!(f.sandbox.stop_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn reacquisition_rechecks_close_after_the_actual_owner_was_acquired() {
    use crate::bootstrap::session_dispatch::SessionDispatchRegistry;
    let registry = Arc::new(SessionDispatchRegistry::default());
    let mut f = fixture().await;
    let session_id = f.owner.metadata().session_id.clone();
    let (controller, _) = begin_registered(&registry, &mut f);
    closed_successor(&controller);
    let turn_id = controller.snapshot().unwrap().turn_id().clone();
    drop(controller);
    registry.release_after_turn(&session_id, &turn_id).unwrap();
    let token = registry.prepare_reacquisition(&session_id).unwrap();
    let fresh = f.owner.reacquire_between_turns().await.unwrap();
    registry.close_all_admission().unwrap();
    assert!(registry.complete_reacquisition(token, fresh).is_err());
    assert!(
        f.operation.try_lock().is_ok(),
        "no command was dispatched by refused acquisition"
    );
    let mut cleanup = registry
        .prepare_session_cleanup(&session_id, Duration::from_secs(1))
        .await
        .unwrap();
    drop(cleanup.take_operation());
    registry.complete_session_cleanup(&cleanup).unwrap();
}

#[tokio::test]
async fn lost_reacquisition_ack_keeps_actual_owner_for_checked_cleanup() {
    use crate::bootstrap::session_dispatch::SessionDispatchRegistry;
    let registry = SessionDispatchRegistry::default();
    let mut f = fixture().await;
    let session_id = f.owner.metadata().session_id.clone();
    let (controller, _) = begin_registered(&registry, &mut f);
    let next = closed_successor(&controller);
    let turn_id = controller.snapshot().unwrap().turn_id().clone();
    drop(controller);
    registry.release_after_turn(&session_id, &turn_id).unwrap();
    let token = registry.prepare_reacquisition(&session_id).unwrap();
    let fresh = f.owner.reacquire_between_turns().await.unwrap();
    registry.fail_next_reacquisition_ack_for_test();
    assert!(registry.complete_reacquisition(token, fresh).is_err());
    assert!(f.operation.try_lock().is_err());
    assert!(registry
        .control_plane(&session_id, turn_id.as_str())
        .unwrap()
        .is_some());
    assert!(registry.prepare_reacquisition(&session_id).is_err());
    assert!(registry
        .begin_native_successor_checked(&session_id, next, |_, _, _| Ok(()))
        .is_err());
    let mut cleanup = registry
        .prepare_session_cleanup(&session_id, Duration::from_secs(1))
        .await
        .unwrap();
    drop(cleanup.take_operation());
    registry.complete_session_cleanup(&cleanup).unwrap();
    drop(cleanup);
    assert!(f.operation.try_lock().is_ok());
    assert_eq!(f.sandbox.stop_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn cancelled_cleanup_after_between_turn_reacquisition_keeps_the_parked_guard() {
    use crate::bootstrap::session_dispatch::SessionDispatchRegistry;
    let registry = Arc::new(SessionDispatchRegistry::default());
    let mut f = fixture().await;
    let session_id = f.owner.metadata().session_id.clone();
    let (controller, _) = begin_registered(&registry, &mut f);
    closed_successor(&controller);
    let turn_id = controller.snapshot().unwrap().turn_id().clone();
    drop(controller);
    registry.release_after_turn(&session_id, &turn_id).unwrap();
    let writer = f.operation.clone().lock_owned().await;
    let (ready, waiting) = tokio::sync::oneshot::channel();
    let cleanup_task = tokio::spawn({
        let registry = registry.clone();
        let session_id = session_id.clone();
        async move {
            let mut cleanup = registry
                .prepare_session_cleanup(&session_id, Duration::from_secs(5))
                .await
                .unwrap();
            let _operation = cleanup.take_operation().unwrap();
            let _ = ready.send(());
            std::future::pending::<()>().await;
            registry.complete_session_cleanup(&cleanup).unwrap();
        }
    });
    drop(writer);
    tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .unwrap()
        .unwrap();
    cleanup_task.abort();
    assert!(cleanup_task.await.unwrap_err().is_cancelled());
    assert!(
        f.operation.try_lock().is_err(),
        "dropped waiter must not unlock a started lifecycle"
    );
    let mut retry = registry
        .prepare_session_cleanup(&session_id, Duration::from_secs(1))
        .await
        .unwrap();
    let operation = retry.take_operation().unwrap();
    registry.complete_session_cleanup(&retry).unwrap();
    drop(retry);
    assert!(f.operation.try_lock().is_err());
    drop(operation);
    assert!(f.operation.try_lock().is_ok());
    assert_eq!(f.sandbox.stop_calls.load(Ordering::SeqCst), 0);
}

struct NeverDispatchedHandoffFactory;

#[async_trait::async_trait]
impl crate::session_dispatch::AutonomousActivationFactory for NeverDispatchedHandoffFactory {
    async fn resources(
        &self,
        _: &axocoatl_session::turn_contract::ActivationInputManifest,
    ) -> std::result::Result<crate::session_dispatch::AutonomousActivationResources, String> {
        panic!("a held, unstarted driver must not resolve or dispatch Agent resources")
    }
}

#[tokio::test]
async fn finalized_closure_cannot_release_a_still_owned_driver_ticket() {
    use crate::bootstrap::session_dispatch::SessionDispatchRegistry;
    use axocoatl_session::control_authority::{AuthorityGrant, ExecutionProfile, GrantLimits};
    use axocoatl_session::execution_content::ActivationEvidenceContent;
    use axocoatl_session::turn_contract::*;
    let registry = SessionDispatchRegistry::default();
    let mut f = fixture().await;
    let session_id = f.owner.metadata().session_id.clone();
    let (controller, _) = begin_registered(&registry, &mut f);
    let snapshot = controller.snapshot().unwrap();
    let node = snapshot.contract().graph().unwrap().nodes[0].clone();
    let limits = GrantLimits {
        activations: 1,
        invocations: 1,
        tokens: 1000,
        cost_microunits: 0,
    };
    let budget = controller
        .retain_activation_evidence(ActivationEvidenceContent::Budget {
            limits: limits.clone(),
        })
        .unwrap();
    let issuer = controller
        .retain_activation_evidence(ActivationEvidenceContent::Guidance {
            text: "Explicit fixture driver grant".into(),
        })
        .unwrap();
    let policy = AuthorityGrant {
        id: "handoff-driver-grant".into(),
        revision: 1,
        issuer_evidence: issuer,
        holder: node.node_id.clone(),
        descendants: vec![],
        allow_stop_descendants: false,
        delegation: None,
        conditions: vec![],
        profiles: vec![ExecutionProfile {
            definition: node.definition.definition_id.as_str().into(),
            provider: "local".into(),
            model: "model".into(),
            isolation: "in-process".into(),
            tools: vec![],
            write_scope: None,
        }],
        limits,
        expires_at_ms: u64::MAX,
    };
    let grant = controller
        .retain_activation_evidence(ActivationEvidenceContent::Grant {
            policy: policy.clone(),
        })
        .unwrap();
    controller.install_grant(policy).unwrap();
    let driver = controller
        .autonomous_turn_driver(
            vec![crate::session_dispatch::AutonomousNodeInput {
                node_id: node.node_id,
                guidance: vec![],
                attachments: vec![],
                repository: RepositoryInput::Unavailable,
                budget,
                grant: Some(GrantSnapshotRef {
                    grant_id: GrantId::new("handoff-driver-grant").unwrap(),
                    revision: 1,
                    evidence: grant,
                }),
            }],
            Arc::new(NeverDispatchedHandoffFactory),
        )
        .unwrap();
    closed_successor(&controller);
    assert!(registry
        .release_after_turn(&session_id, snapshot.turn_id())
        .is_err());
    assert!(f.operation.try_lock().is_err());
    drop(driver);
    registry
        .release_after_turn(&session_id, snapshot.turn_id())
        .unwrap();
    assert!(f.operation.try_lock().is_ok());
    assert_eq!(f.sandbox.stop_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn reacquisition_refuses_changed_runtime_and_releases_only_its_undispatched_guard() {
    use crate::bootstrap::session_dispatch::SessionDispatchRegistry;
    let registry = SessionDispatchRegistry::default();
    let mut f = fixture().await;
    let session_id = f.owner.metadata().session_id.clone();
    let (controller, _) = begin_registered(&registry, &mut f);
    closed_successor(&controller);
    let turn_id = controller.snapshot().unwrap().turn_id().clone();
    drop(controller);
    registry.release_after_turn(&session_id, &turn_id).unwrap();
    let replacement: Arc<dyn Sandbox> = Arc::new(ControlledSandbox::new(
        f.owner.root(),
        "changed-incarnation",
    ));
    f.owner
        .inner
        .sandboxes
        .lock()
        .await
        .insert(session_id, replacement);
    assert!(f.owner.reacquire_between_turns().await.is_err());
    assert!(f.operation.try_lock().is_ok());
    assert!(f.owner.execution_lease().await.is_err());
    assert_eq!(f.sandbox.stop_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn finalized_partial_turn_needs_retained_outcome_even_when_actual_owner_is_idle() {
    use crate::bootstrap::session_dispatch::SessionDispatchRegistry;
    use axocoatl_actor::{
        AgentRunControl, AgentRunId, ToolInvocationOutcome, ToolInvocationRequest,
    };
    use axocoatl_session::control_authority::{
        AuthorityGrant, DispatchReservation, ExecutionProfile, GrantLimits,
    };
    use axocoatl_session::execution_content::ActivationEvidenceContent;
    use axocoatl_session::turn_contract::*;

    for late_outcome in [false, true] {
        let registry = SessionDispatchRegistry::default();
        let mut f = fixture().await;
        let session_id = f.owner.metadata().session_id.clone();
        let (controller, _) = begin_registered_with_tools(&registry, &mut f, &["effect"]);
        let snapshot = controller.snapshot().unwrap();
        let node = snapshot.contract().graph().unwrap().nodes[0].clone();
        let activation = ActivationRef {
            session_id: snapshot.owner().session_id.clone(),
            turn_id: snapshot.turn_id().clone(),
            node_id: node.node_id.clone(),
            activation_id: ActivationId::new("handoff-effect-activation").unwrap(),
            generation: 1,
            execution_epoch_id: snapshot.contract().epochs().last().unwrap().id.clone(),
        };
        let profile = ExecutionProfile {
            definition: node.definition.definition_id.as_str().into(),
            provider: "local".into(),
            model: "model".into(),
            isolation: "in-process".into(),
            tools: vec!["effect".into()],
            write_scope: None,
        };
        let limits = GrantLimits {
            activations: 1,
            invocations: 1,
            tokens: 0,
            cost_microunits: 0,
        };
        let budget = controller
            .retain_activation_evidence(ActivationEvidenceContent::Budget {
                limits: limits.clone(),
            })
            .unwrap();
        let policy = AuthorityGrant {
            id: "handoff-effect-grant".into(),
            revision: 1,
            issuer_evidence: EvidenceRef::new("explicit-test-grant").unwrap(),
            holder: node.node_id,
            descendants: vec![],
            allow_stop_descendants: false,
            delegation: None,
            conditions: vec![],
            profiles: vec![profile.clone()],
            limits,
            expires_at_ms: u64::MAX,
        };
        let grant = controller
            .retain_activation_evidence(ActivationEvidenceContent::Grant {
                policy: policy.clone(),
            })
            .unwrap();
        controller.install_grant(policy).unwrap();
        controller
            .append_host_event(TurnContractEnvelope {
                schema_version: TURN_CONTRACT_SCHEMA_VERSION,
                command_id: CommandId::new("handoff-start-effect").unwrap(),
                expected_revision: snapshot.contract().revision(),
                session_id: activation.session_id.clone(),
                turn_id: activation.turn_id.clone(),
                event: TurnContractEvent::StartActivation {
                    input: Box::new(ActivationInputManifest {
                        manifest_id: InputManifestId::new("handoff-effect-input").unwrap(),
                        activation: activation.clone(),
                        definition: node.definition,
                        conversation_id: node.conversation_id,
                        starting_savepoint: ConversationSavepoint::Empty,
                        parents: vec![],
                        guidance: vec![snapshot.request_ref().unwrap().clone()],
                        attachments: vec![],
                        repository: RepositoryInput::Unavailable,
                        budget,
                        grant: Some(GrantSnapshotRef {
                            grant_id: GrantId::new("handoff-effect-grant").unwrap(),
                            revision: 1,
                            evidence: grant,
                        }),
                        revision_context: None,
                    }),
                },
            })
            .unwrap();
        let control = controller
            .bind_activation(
                activation.clone(),
                profile.clone(),
                "{}".into(),
                AgentRunControl::new(AgentRunId::new("handoff-effect")),
                DispatchReservation {
                    tokens: 0,
                    cost_microunits: 0,
                },
            )
            .unwrap();
        let permit = control
            .execution_boundary()
            .unwrap()
            .admit(&ToolInvocationRequest {
                actor_id: "conversation".into(),
                provider_id: profile.provider,
                model_id: profile.model,
                provider_response_group: 1,
                provider_call_index: 0,
                provider_call_count: 1,
                tool_call: axocoatl_llm::ToolCall {
                    id: "handoff-effect-call".into(),
                    name: "effect".into(),
                    arguments: serde_json::json!({}),
                    provider_metadata: Default::default(),
                },
            })
            .await
            .unwrap();
        let current = controller.snapshot().unwrap();
        controller
            .append_host_event(TurnContractEnvelope {
                schema_version: TURN_CONTRACT_SCHEMA_VERSION,
                command_id: CommandId::new("handoff-interrupt-effect").unwrap(),
                expected_revision: current.contract().revision(),
                session_id: activation.session_id.clone(),
                turn_id: activation.turn_id.clone(),
                event: TurnContractEvent::InterruptEpoch {
                    epoch_id: activation.execution_epoch_id.clone(),
                },
            })
            .unwrap();
        closed_successor(&controller);
        let closed = controller.snapshot().unwrap();
        assert!(closed.contract().has_unknown_effects());
        assert!(f.owner.execution_is_idle().unwrap());
        // A retained real invocation ticket prevents release even before the
        // outcome-evidence gate is considered.
        assert!(registry
            .release_after_turn(&session_id, closed.turn_id())
            .is_err());
        if late_outcome {
            permit
                .record_outcome(&ToolInvocationOutcome::Returned(Ok(
                    serde_json::json!({"observed": true}),
                )))
                .await
                .unwrap();
            // The closed canonical unknown disposition is immutable. Its exact
            // late result is retained in the invocation audit/content instead.
            assert!(controller
                .snapshot()
                .unwrap()
                .contract()
                .has_unknown_effects());
            assert_eq!(
                controller.snapshot().unwrap().contract().revision(),
                closed.contract().revision()
            );
            registry
                .release_after_turn(&session_id, closed.turn_id())
                .unwrap();
            assert!(f.operation.try_lock().is_ok());
            assert!(f.owner.execution_lease().await.is_err());
        } else {
            drop(permit);
            // Zero owned tickets and an idle repository do not turn an unknown
            // durable effect into proof that another writer may be admitted.
            assert!(registry
                .release_after_turn(&session_id, closed.turn_id())
                .is_err());
            assert!(f.operation.try_lock().is_err());
        }
        assert_eq!(f.sandbox.stop_calls.load(Ordering::SeqCst), 0);
        assert_eq!(f.owner.sandbox().list_terminals().len(), 1);
    }
}

#[tokio::test]
async fn cancelled_reacquisition_waiting_for_session_start_releases_only_temporary_ownership() {
    use crate::bootstrap::session_dispatch::SessionDispatchRegistry;
    let registry = Arc::new(SessionDispatchRegistry::default());
    let mut f = fixture().await;
    let session_id = f.owner.metadata().session_id.clone();
    let (controller, _) = begin_registered(&registry, &mut f);
    let next = closed_successor(&controller);
    let turn_id = controller.snapshot().unwrap().turn_id().clone();
    let revision = controller.snapshot().unwrap().contract().revision();
    drop(controller);
    registry.release_after_turn(&session_id, &turn_id).unwrap();

    // The real Session start mutex forces reacquisition to suspend after it
    // owns the real Workspace mutex, before it can construct a fresh owner.
    let start = f.owner.inner.start.clone().lock_owned().await;
    let token = registry.prepare_reacquisition(&session_id).unwrap();
    let task = tokio::spawn({
        let registry = registry.clone();
        let old_owner = f.owner.clone();
        async move {
            let fresh = old_owner.reacquire_between_turns().await?;
            registry.complete_reacquisition(token, fresh)
        }
    });
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if f.operation.try_lock().is_err() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!task.is_finished());
    assert!(registry.retains_session(&session_id).unwrap());
    task.abort();
    assert!(matches!(task.await, Err(error) if error.is_cancelled()));

    // Cancellation dispatches nothing and removes no registration. Its
    // temporary Workspace guard drops; the old capability stays retired.
    assert!(f.operation.try_lock().is_ok());
    assert!(f.owner.execution_lease().await.is_err());
    assert!(registry.retains_session(&session_id).unwrap());
    let view = registry
        .control_plane(&session_id, turn_id.as_str())
        .unwrap()
        .unwrap();
    assert_eq!(
        view.turn_revision,
        crate::session_control_plane::EvidenceValue::Available { value: revision }
    );
    registry.release_after_turn(&session_id, &turn_id).unwrap();
    drop(start);

    let token = registry.prepare_reacquisition(&session_id).unwrap();
    let fresh = f.owner.reacquire_between_turns().await.unwrap();
    registry.complete_reacquisition(token, fresh).unwrap();
    assert!(f.operation.try_lock().is_err());
    assert!(f.owner.execution_lease().await.is_err());
    registry
        .begin_native_successor_checked(&session_id, next, |_, _, _| Ok(()))
        .unwrap();
    let mut cleanup = registry
        .prepare_session_cleanup(&session_id, Duration::from_secs(1))
        .await
        .unwrap();
    drop(cleanup.take_operation());
    registry.complete_session_cleanup(&cleanup).unwrap();
    drop(cleanup);
    assert!(f.operation.try_lock().is_ok());
    assert_eq!(f.sandbox.stop_calls.load(Ordering::SeqCst), 0);
    assert_eq!(f.owner.sandbox().list_terminals().len(), 1);
}

fn rejected_history_action(
    registry: &crate::bootstrap::session_dispatch::SessionDispatchRegistry,
    session_id: &str,
    turn_id: &str,
    command_id: &str,
) -> axocoatl_session::control_command::CommandReceiptView {
    use axocoatl_session::turn_contract::*;
    let view = registry
        .control_plane(session_id, turn_id)
        .unwrap()
        .unwrap();
    let crate::session_control_plane::EvidenceValue::Available { value: revision } =
        view.turn_revision
    else {
        panic!("fixture must have canonical revision")
    };
    let crate::session_control_plane::EvidenceValue::Available { value: epochs } = view.epochs
    else {
        panic!("fixture must have canonical epoch")
    };
    let epoch_id = ExecutionEpochId::new(epochs.last().unwrap()["id"].as_str().unwrap()).unwrap();
    let session_id_typed = SessionId::new(session_id).unwrap();
    let turn_id_typed = LogicalTurnId::new(turn_id).unwrap();
    registry
        .submit_human_action(
            session_id,
            turn_id,
            crate::session_dispatch::HumanControlActionRequest {
                schema_version: 1,
                command_id: CommandId::new(command_id).unwrap(),
                session_id: session_id_typed.clone(),
                turn_id: turn_id_typed.clone(),
                execution_epoch_id: epoch_id.clone(),
                expected_turn_revision: revision,
                expected_graph_revision: 1,
                activation: Some(ActivationRef {
                    session_id: session_id_typed,
                    turn_id: turn_id_typed,
                    execution_epoch_id: epoch_id,
                    node_id: TurnNodeId::new("node").unwrap(),
                    activation_id: ActivationId::new("not-started-fixture-activation").unwrap(),
                    generation: 1,
                }),
                action: crate::session_dispatch::HumanControlAction::Stop,
                instruction: None,
                include_previous_output: false,
                context: None,
                blocker_id: None,
                human_response: None,
                partial_finish: None,
                continuation: None,
            },
            100,
        )
        .unwrap()
}

fn historical_read_tree(
    root: &Path,
) -> std::collections::BTreeMap<PathBuf, (u32, Option<Vec<u8>>)> {
    use std::os::unix::fs::PermissionsExt;
    fn visit(
        root: &Path,
        path: &Path,
        values: &mut std::collections::BTreeMap<PathBuf, (u32, Option<Vec<u8>>)>,
    ) {
        let metadata = std::fs::symlink_metadata(path).unwrap();
        assert!(
            !metadata.file_type().is_symlink(),
            "fixture tree must not contain symlinks"
        );
        values.insert(
            path.strip_prefix(root).unwrap().to_path_buf(),
            (
                metadata.permissions().mode(),
                metadata.is_file().then(|| std::fs::read(path).unwrap()),
            ),
        );
        if metadata.is_dir() {
            for entry in std::fs::read_dir(path).unwrap() {
                visit(root, &entry.unwrap().path(), values);
            }
        }
    }
    let mut values = std::collections::BTreeMap::new();
    visit(root, root, &mut values);
    values
}

#[tokio::test]
async fn historical_registry_reads_preserve_exact_receipts_after_successor_without_writes() {
    use crate::bootstrap::session_dispatch::SessionDispatchRegistry;
    use axocoatl_session::control_command::ControlCommandState;
    let registry = SessionDispatchRegistry::default();
    let mut f = fixture().await;
    let session_id = f.owner.metadata().session_id.clone();
    let (controller, _) = begin_registered(&registry, &mut f);
    let old_turn = controller.snapshot().unwrap().turn_id().clone();
    let receipt = rejected_history_action(
        &registry,
        &session_id,
        old_turn.as_str(),
        "old-exact-command",
    );
    assert_eq!(receipt.state, ControlCommandState::Rejected);
    let next = closed_successor(&controller);
    let next_turn = next.turn_id.clone();
    let closed_current = controller.control_plane().unwrap();
    drop(controller);
    registry
        .begin_native_successor_checked(&session_id, next, |_, _, _| Ok(()))
        .unwrap();
    let before = historical_read_tree(f._data.path());
    for _ in 0..3 {
        let historical = registry
            .control_plane(&session_id, old_turn.as_str())
            .unwrap()
            .unwrap();
        assert_eq!(
            historical.commands,
            crate::session_control_plane::EvidenceValue::Available {
                value: vec![receipt.clone()]
            }
        );
        assert_eq!(historical.turn_revision, closed_current.turn_revision);
        assert_eq!(historical.request, closed_current.request);
        assert_eq!(historical.invocations, closed_current.invocations);
        assert!(historical
            .nodes
            .iter()
            .flat_map(|node| &node.activations)
            .all(|activation| !activation.capabilities.stop.enabled
                && !activation.capabilities.retry.enabled));
        let current = registry
            .control_plane(&session_id, next_turn.as_str())
            .unwrap()
            .unwrap();
        assert_eq!(
            current.commands,
            crate::session_control_plane::EvidenceValue::Available { value: vec![] }
        );
    }
    assert_eq!(historical_read_tree(f._data.path()), before);
    let current_receipt = rejected_history_action(
        &registry,
        &session_id,
        next_turn.as_str(),
        "new-exact-command",
    );
    assert_eq!(current_receipt.state, ControlCommandState::Rejected);
    assert_eq!(
        registry
            .control_plane(&session_id, old_turn.as_str())
            .unwrap()
            .unwrap()
            .commands,
        crate::session_control_plane::EvidenceValue::Available {
            value: vec![receipt]
        }
    );
    assert_eq!(f.sandbox.stop_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn historical_receipt_loss_and_foreign_identity_stay_unavailable_without_recreation() {
    use crate::bootstrap::session_dispatch::SessionDispatchRegistry;
    for remove in [false, true] {
        let registry = SessionDispatchRegistry::default();
        let mut f = fixture().await;
        let session_id = f.owner.metadata().session_id.clone();
        let (controller, _) = begin_registered(&registry, &mut f);
        let old_turn = controller.snapshot().unwrap().turn_id().clone();
        rejected_history_action(
            &registry,
            &session_id,
            old_turn.as_str(),
            "historical-evidence",
        );
        let next = closed_successor(&controller);
        let next_turn = next.turn_id.clone();
        drop(controller);
        registry
            .begin_native_successor_checked(&session_id, next, |_, _, _| Ok(()))
            .unwrap();
        let files = historical_read_tree(f._data.path());
        let relative = files
            .iter()
            .find_map(|(path, (_, bytes))| {
                if path
                    .file_name()
                    .is_some_and(|name| name == "control-command.v1.json")
                {
                    let body: serde_json::Value =
                        serde_json::from_slice(bytes.as_ref().unwrap()).unwrap();
                    if body["owner"]["turn_id"] == old_turn.as_str() {
                        return Some(path.clone());
                    }
                }
                None
            })
            .unwrap();
        let path = f._data.path().join(relative);
        if remove {
            std::fs::remove_file(&path).unwrap();
        } else {
            let mut body: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            body["canonical_journal_id"] = serde_json::json!("foreign-canonical-journal");
            std::fs::write(&path, serde_json::to_vec(&body).unwrap()).unwrap();
        }
        let before = historical_read_tree(f._data.path());
        let view = registry
            .control_plane(&session_id, old_turn.as_str())
            .unwrap()
            .unwrap();
        assert_eq!(view.turn_id, old_turn.as_str());
        assert!(
            matches!(view.commands, crate::session_control_plane::EvidenceValue::Unavailable { reason } if !reason.is_empty())
        );
        assert_eq!(historical_read_tree(f._data.path()), before);
        assert_eq!(
            registry
                .control_plane(&session_id, next_turn.as_str())
                .unwrap()
                .unwrap()
                .commands,
            crate::session_control_plane::EvidenceValue::Available { value: vec![] }
        );
        assert!(f.operation.try_lock().is_err());
        assert_eq!(f.sandbox.stop_calls.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn canonical_missing_turn_and_missing_upgraded_controller_never_read_legacy_fallback() {
    use crate::bootstrap::session_dispatch::SessionDispatchRegistry;
    use axocoatl_session::execution_ownership::DataRootFormatOwnership;
    let registry = SessionDispatchRegistry::default();
    let mut f = fixture().await;
    let session_id = f.owner.metadata().session_id.clone();
    let (controller, _) = begin_registered(&registry, &mut f);
    drop(controller);
    // The canonical registry is decisive even if a host still owns a legacy
    // ledger containing a matching stale row. This is the actual route selector.
    let legacy_root = tempfile::tempdir().unwrap();
    let legacy = DataRootFormatOwnership::Legacy(
        LegacyFormatOwnership::acquire(legacy_root.path()).unwrap(),
    );
    let absent = registry
        .lookup_control_plane(&session_id, "missing-turn")
        .unwrap()
        .resolve_with_legacy(&legacy, || async {
            panic!("canonical miss must not inspect legacy rows")
        })
        .await
        .unwrap();
    assert!(absent.is_none());
    let upgraded_root = tempfile::tempdir().unwrap();
    let upgraded = DataRootFormatOwnership::Upgraded(Arc::new(
        LegacyFormatOwnership::acquire(upgraded_root.path())
            .unwrap()
            .upgrade()
            .unwrap(),
    ));
    assert!(registry
        .lookup_control_plane("unregistered-session", "missing-turn")
        .unwrap()
        .resolve_with_legacy(&upgraded, || async {
            panic!("missing upgraded controller must not inspect legacy rows")
        })
        .await
        .is_err());
    let invoked = AtomicBool::new(false);
    assert!(registry
        .lookup_control_plane("unregistered-session", "missing-turn")
        .unwrap()
        .resolve_with_legacy(&legacy, || async {
            invoked.store(true, Ordering::SeqCst);
            Ok(None)
        })
        .await
        .unwrap()
        .is_none());
    assert!(invoked.load(Ordering::SeqCst));
}

#[tokio::test]
async fn sealed_legacy_ids_outside_v2_syntax_remain_exactly_readable_without_writes() {
    use crate::bootstrap::session_dispatch::SessionDispatchRegistry;
    use axocoatl_session::turn_contract::LogicalTurnId;
    // Both IDs are valid legacy inputs: one exceeds the v2 alphabet, the other
    // exceeds its byte bound. Neither may be normalized or silently omitted.
    for legacy_id in ["turn café".to_owned(), "legacy".repeat(24)] {
        assert!(LogicalTurnId::new(&legacy_id).is_err());
        let mut f = fixture_with_legacy_turn(Some(&legacy_id)).await;
        let registry = SessionDispatchRegistry::default();
        let session_id = f.owner.metadata().session_id.clone();
        let (controller, _) = begin_registered(&registry, &mut f);
        let current_id = controller.snapshot().unwrap().turn_id().clone();
        drop(controller);
        let before = historical_read_tree(f._data.path());
        for _ in 0..3 {
            let view = registry
                .control_plane(&session_id, &legacy_id)
                .unwrap()
                .unwrap();
            assert_eq!(view.session_id, session_id);
            assert_eq!(view.turn_id, legacy_id);
            let encoded = serde_json::to_string(&view).unwrap();
            assert!(encoded.contains("Original legacy request"));
            assert!(encoded.contains("Original legacy answer"));
            assert!(view
                .nodes
                .iter()
                .flat_map(|node| &node.activations)
                .all(|activation| !activation.capabilities.stop.enabled
                    && !activation.capabilities.retry.enabled));
            assert!(registry
                .control_plane(&session_id, &format!("{legacy_id}-absent"))
                .unwrap()
                .is_none());
            let current = registry
                .control_plane(&session_id, current_id.as_str())
                .unwrap()
                .unwrap();
            assert_eq!(current.turn_id, current_id.as_str());
        }
        assert_eq!(historical_read_tree(f._data.path()), before);
        assert_eq!(f.sandbox.stop_calls.load(Ordering::SeqCst), 0);
        assert!(f.operation.try_lock().is_err());
    }
}

#[path = "bootstrap_session_pending_tests.rs"]
mod pending_session_tests;

#[tokio::test]
async fn registered_successor_refuses_visible_and_hidden_sealed_ids_without_poisoning_history() {
    use crate::bootstrap::session_dispatch::SessionDispatchRegistry;
    for hidden in [false, true] {
        let mut fixture = fixture_with_legacy_turn_visibility(Some("successor-turn"), hidden).await;
        let session_id = fixture.owner.metadata().session_id.clone();
        let registry = SessionDispatchRegistry::default();
        let (controller, _) = begin_registered(&registry, &mut fixture);
        let next = closed_successor(&controller);
        drop(controller);
        let before = historical_read_tree(fixture._data.path());
        let error = registry
            .begin_native_successor_checked(&session_id, next, |_, _, _| Ok(()))
            .err()
            .expect("collision must refuse");
        assert!(error
            .to_string()
            .contains("occupied by retained legacy history"));
        assert_eq!(historical_read_tree(fixture._data.path()), before);
        let history = registry.history_snapshot(&session_id).unwrap().unwrap();
        assert_eq!(
            history
                .entries(axocoatl_session::session_history::HistoryVisibility::IncludingSuperseded)
                .len(),
            2
        );
        assert_eq!(history.get("successor-turn").unwrap().is_visible(), !hidden);
        assert!(history.get("lifecycle-turn").is_some());
        drop(fixture.owner.execution_lease().await.unwrap());
        assert_eq!(fixture.sandbox.stop_calls.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn consuming_successor_refuses_sealed_identity_before_durable_changes() {
    let mut fixture = fixture_with_legacy_turn_visibility(Some("successor-turn"), true).await;
    let controller = controller(&mut fixture);
    let next = closed_successor(&controller);
    let before = historical_read_tree(fixture._data.path());
    let error = controller
        .begin_successor(next)
        .err()
        .expect("collision must refuse");
    assert!(error
        .to_string()
        .contains("occupied by retained legacy history"));
    assert_eq!(historical_read_tree(fixture._data.path()), before);
    assert_eq!(fixture.sandbox.stop_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn successor_compares_sealed_legacy_ids_exactly_without_trimming() {
    use crate::bootstrap::session_dispatch::SessionDispatchRegistry;
    let raw_id = "  successor-turn  ";
    let mut fixture = fixture_with_legacy_turn(Some(raw_id)).await;
    let session_id = fixture.owner.metadata().session_id.clone();
    let registry = SessionDispatchRegistry::default();
    let (controller, _) = begin_registered(&registry, &mut fixture);
    let next = closed_successor(&controller);
    drop(controller);
    registry
        .begin_native_successor_checked(&session_id, next, |_, _, _| Ok(()))
        .unwrap();
    let history = registry.history_snapshot(&session_id).unwrap().unwrap();
    assert!(history.get(raw_id).is_some());
    assert!(history.get("successor-turn").is_some());
    assert_eq!(
        history
            .entries(axocoatl_session::session_history::HistoryVisibility::IncludingSuperseded)
            .len(),
        3
    );
}

#[path = "bootstrap_session_repository_activation_tests.rs"]
mod activation_tests;

#[path = "bootstrap_session_turn_stop_tests.rs"]
mod turn_stop_tests;

#[path = "bootstrap_native_turn_tests.rs"]
mod native_turn_tests;

#[path = "bootstrap_session_repository_admission_tests.rs"]
mod admission_tests;

#[tokio::test]
async fn recovered_repository_reattachment_preserves_inputs_and_uses_fresh_physical_owner() {
    use crate::bootstrap::session_dispatch::SessionDispatchRegistry;
    use axocoatl_session::control_authority::{AuthorityGrant, ExecutionProfile, GrantLimits};
    use axocoatl_session::execution_content::ActivationEvidenceContent;
    use axocoatl_session::execution_ownership::UpgradedFormatOwnership;
    use axocoatl_session::turn_contract::*;
    let mut f = fixture_with_origin(None, false, true).await;
    let registry = SessionDispatchRegistry::default();
    let (controller, original) = begin_registered(&registry, &mut f);
    let snapshot = controller.snapshot().unwrap();
    let node = snapshot.contract().graph().unwrap().nodes[0].clone();
    let limits = GrantLimits {
        activations: 2,
        invocations: 4,
        tokens: 0,
        cost_microunits: 0,
    };
    let policy = AuthorityGrant {
        id: "recovered-repository-grant".into(),
        revision: 1,
        issuer_evidence: snapshot.request_ref().unwrap().clone(),
        holder: node.node_id.clone(),
        descendants: vec![],
        allow_stop_descendants: false,
        delegation: None,
        conditions: vec![],
        profiles: vec![ExecutionProfile {
            definition: node.definition.definition_id.as_str().into(),
            provider: "local".into(),
            model: "model".into(),
            isolation: "in-process".into(),
            tools: vec![],
            write_scope: None,
        }],
        limits: limits.clone(),
        expires_at_ms: u64::MAX,
    };
    let budget = controller
        .retain_activation_evidence(ActivationEvidenceContent::Budget { limits })
        .unwrap();
    let grant = controller
        .retain_activation_evidence(ActivationEvidenceContent::Grant {
            policy: policy.clone(),
        })
        .unwrap();
    controller.install_grant(policy).unwrap();
    let input = ActivationInputManifest {
        manifest_id: InputManifestId::new("recovery-input").unwrap(),
        activation: ActivationRef {
            session_id: snapshot.owner().session_id.clone(),
            turn_id: snapshot.turn_id().clone(),
            node_id: node.node_id,
            activation_id: ActivationId::new("recovery-activation").unwrap(),
            generation: 1,
            execution_epoch_id: snapshot.contract().epochs()[0].id.clone(),
        },
        definition: node.definition,
        conversation_id: node.conversation_id,
        starting_savepoint: ConversationSavepoint::Empty,
        parents: vec![],
        guidance: vec![snapshot.request_ref().unwrap().clone()],
        attachments: vec![],
        repository: RepositoryInput::Recorded {
            snapshot: original.clone(),
        },
        budget,
        grant: Some(GrantSnapshotRef {
            grant_id: GrantId::new("recovered-repository-grant").unwrap(),
            revision: 1,
            evidence: grant,
        }),
        revision_context: None,
    };
    controller
        .append_host_event(TurnContractEnvelope {
            schema_version: TURN_CONTRACT_SCHEMA_VERSION,
            command_id: CommandId::new("recovery-start").unwrap(),
            expected_revision: snapshot.contract().revision(),
            session_id: input.activation.session_id.clone(),
            turn_id: input.activation.turn_id.clone(),
            event: TurnContractEvent::StartActivation {
                input: Box::new(input.clone()),
            },
        })
        .unwrap();
    controller
        .repository_activation_resource(&original)
        .unwrap();
    controller.close_registered_repository_admission().unwrap();
    let suspended = controller.snapshot().unwrap();
    let original_bytes = controller
        .with_team_stores(|_, content, _| {
            Ok(content
                .resolve_activation_evidence(&original)
                .unwrap()
                .clone())
        })
        .unwrap();
    drop(controller);
    drop(registry);
    drop(f.owner.retire_idle().unwrap());
    let sandbox = Arc::new(ControlledSandbox::new(
        f.owner.root(),
        "container-incarnation-2",
    ));
    let mut metadata = f.owner.metadata().clone();
    metadata.execution_identity.clone_from(&sandbox.incarnation);
    let registered: Arc<dyn Sandbox> = sandbox.clone();
    f.owner
        .inner
        .sandboxes
        .lock()
        .await
        .insert(metadata.session_id.clone(), registered.clone());
    let fresh = SessionRepositoryOwner {
        inner: Arc::new(RepositoryOwnerInner {
            attempt: None,
            identity: f.owner.identity().clone(),
            metadata,
            runtime: f.owner.inner.runtime.clone(),
            sandbox: registered,
            data_root: f.owner.inner.data_root.clone(),
            workspace_root: f.owner.inner.workspace_root.clone(),
            sessions: f.owner.inner.sessions.clone(),
            workspaces: f.owner.inner.workspaces.clone(),
            sandboxes: f.owner.inner.sandboxes.clone(),
            start: f.owner.inner.start.clone(),
            shutdown: f.owner.inner.shutdown.clone(),
            workspace_operation: Mutex::new(Some(f.operation.clone().lock_owned().await)),
            workspace_gate: f.operation.clone(),
            execution: Arc::new(AsyncMutex::new(())),
            state: Mutex::new(ExecutionState::default()),
            changed: Notify::new(),
        }),
    };
    fresh.validate_current().await.unwrap();
    let session = fresh
        .inner
        .sessions
        .lock()
        .await
        .get(&fresh.metadata().session_id)
        .unwrap()
        .clone();
    let format = Arc::new(UpgradedFormatOwnership::open(f._data.path()).unwrap());
    let stores =
        crate::bootstrap::session_recovery::recover_session_stores(format, &session).unwrap();
    let registry = SessionDispatchRegistry::default();
    let token = registry.retain_existing_session(&mut Some(stores)).unwrap();
    let (recovered, rebound) = registry
        .attach_existing_turn(&token, snapshot.turn_id().clone(), fresh.clone())
        .unwrap();
    assert_eq!(rebound, original);
    assert!(!fresh.same_owner(&f.owner));
    assert!(f.owner.execution_lease().await.is_err());
    drop(fresh.execution_lease().await.unwrap());
    assert_eq!(
        recovered.snapshot().unwrap().contract().revision(),
        suspended.contract().revision()
    );
    assert_eq!(
        recovered.snapshot().unwrap().contract().activations()[0].input,
        input
    );
    let resource = recovered.repository_activation_resource(&original).unwrap();
    assert_eq!(resource.reference(), &original);
    recovered
        .with_team_stores(|canonical, content, _| {
            let view = content
                .project(&canonical.snapshot(snapshot.turn_id()).unwrap())
                .unwrap();
            assert_eq!(view.repository_reattachments.len(), 1);
            let proof = &view.repository_reattachments[0];
            assert_eq!(proof.original_resource, original_bytes);
            assert_ne!(proof.acquired_resource, original_bytes);
            assert_eq!(proof.content.original, original);
            Ok(())
        })
        .unwrap();
    // Historical proof is inspectable but does not outlive actual registration.
    recovered.close_registered_repository_admission().unwrap();
    assert!(recovered.repository_activation_resource(&original).is_err());
    assert_eq!(sandbox.stop_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn recovered_closed_registry_joins_exact_receipts_and_disabled_controls_without_writes() {
    use crate::bootstrap::session_dispatch::SessionDispatchRegistry;
    let registry = SessionDispatchRegistry::default();
    let mut f = fixture_with_origin(None, false, true).await;
    let session_id = f.owner.metadata().session_id.clone();
    let (controller, _) = begin_registered(&registry, &mut f);
    let turn_id = controller.snapshot().unwrap().turn_id().clone();
    let receipt = rejected_history_action(
        &registry,
        &session_id,
        turn_id.as_str(),
        "retained-closed-receipt",
    );
    closed_successor(&controller);
    registry.release_after_turn(&session_id, &turn_id).unwrap();
    let before = registry
        .control_plane(&session_id, turn_id.as_str())
        .unwrap()
        .unwrap();
    assert!(
        matches!(&before.commands, crate::session_control_plane::EvidenceValue::Available {value} if value == &vec![receipt])
    );
    drop(controller);
    drop(registry);
    let session = f
        .owner
        .inner
        .sessions
        .lock()
        .await
        .get(&session_id)
        .unwrap()
        .clone();
    let format = Arc::new(
        axocoatl_session::execution_ownership::UpgradedFormatOwnership::open(f._data.path())
            .unwrap(),
    );
    let stores =
        crate::bootstrap::session_recovery::recover_session_stores(format, &session).unwrap();
    let registry = SessionDispatchRegistry::default();
    let token = registry.retain_existing_session(&mut Some(stores)).unwrap();
    let files = historical_read_tree(f._data.path());
    for _ in 0..3 {
        let after = registry
            .control_plane(&session_id, turn_id.as_str())
            .unwrap()
            .unwrap();
        assert_eq!(after.commands, before.commands);
        assert_eq!(after.invocations, before.invocations);
        assert_eq!(after.turn_revision, before.turn_revision);
        assert_eq!(after.request, before.request);
        let controls = after.turn_controls.unwrap();
        assert!(!controls.finish.enabled && !controls.continue_turn.enabled);
        assert!(
            !controls.finish.requires_revalidation
                && !controls.partial_finish.capability.requires_revalidation
        );
        assert!(after.warnings.is_empty());
    }
    assert_eq!(historical_read_tree(f._data.path()), files);
    assert_eq!(f.sandbox.stop_calls.load(Ordering::SeqCst), 0);
    assert!(registry
        .pending_identity(&token, &f.owner.inner.data_root)
        .is_ok());
    let audit_path = f._data.path().join(
        files
            .keys()
            .find(|path| path.ends_with("invocation-audit.v1.json"))
            .unwrap(),
    );
    let original = std::fs::read(&audit_path).unwrap();
    let mut foreign: serde_json::Value = serde_json::from_slice(&original).unwrap();
    foreign["owner"]["session_id"] = serde_json::json!("foreign-session");
    std::fs::write(&audit_path, serde_json::to_vec(&foreign).unwrap()).unwrap();
    let changed = historical_read_tree(f._data.path());
    let refused = registry
        .control_plane(&session_id, turn_id.as_str())
        .unwrap()
        .unwrap();
    assert!(refused
        .warnings
        .iter()
        .any(|warning| warning.starts_with("Retained invocation audit is unavailable:")));
    assert_eq!(historical_read_tree(f._data.path()), changed);
    std::fs::write(&audit_path, original).unwrap();
    let commands_path = f._data.path().join(
        files
            .keys()
            .find(|path| path.ends_with("control-command.v1.json"))
            .unwrap(),
    );
    std::fs::remove_file(&commands_path).unwrap();
    let missing = historical_read_tree(f._data.path());
    let refused = registry
        .control_plane(&session_id, turn_id.as_str())
        .unwrap()
        .unwrap();
    assert!(matches!(
        refused.commands,
        crate::session_control_plane::EvidenceValue::Unavailable { .. }
    ));
    assert_eq!(historical_read_tree(f._data.path()), missing);
    assert!(!commands_path.exists());
}
