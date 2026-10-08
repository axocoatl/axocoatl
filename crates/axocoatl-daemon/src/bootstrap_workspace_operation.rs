//! The Workspace operation as a request a person waits on takes it.
//!
//! Every operation that acts on a Workspace's repository or runtime holds
//! that Workspace's operation for its whole length: a Session's turn holds
//! it until the turn ends, and a turn that needs a person holds it until
//! someone continues, stops or closes it. A request that waited on such a
//! holder could wait for as long as that turn lasts, with no answer. So a
//! request a person waits on takes the operation this way instead:
//!
//! - free: taken at once;
//! - held by an open turn of any Session of the Workspace: refused at once
//!   as busy (`DaemonError::WorkspaceBusy`, an HTTP `409`), naming that
//!   Session, its loadout run and its turn; nothing changed, and the same
//!   request succeeds once the turn has ended;
//! - held by anything else (a Session being created and its environment
//!   prepared, a loadout run's admission, a Git change, Ways checks): an
//!   ordinary request (a file or Git change, an attachment, a Ways read)
//!   waits for it up to [`WORKSPACE_OPERATION_WAIT`], then is refused as
//!   busy, naming that operation. A lifecycle action (creating, closing,
//!   deleting or reopening a Session, or changing its environment) never
//!   waits: it is refused at once, naming that operation.
//!
//! Close and Delete wait only for their own Session's work, which they stop
//! themselves: its turn, through the Session's registration, until its
//! running command reaches a safe point (up to
//! `SESSION_LIFECYCLE_CLEANUP_TIMEOUT`, 30 s), and its running Ways or Ways
//! checks, which they interrupt, or its legacy turn, which they have asked
//! to stop (up to [`OWN_WORK_RELEASE_WAIT`], 15 s).
//!
//! Each operation taken here, by a run's admission or by several Ways,
//! names itself while it holds the Workspace, and the Session it acts for
//! when it acts for one, so a refusal can say what holds it.
use super::*;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::OwnedMutexGuard;

/// How long an ordinary request a person waits on waits for an operation of
/// its Workspace that is not a turn before it is refused as busy. Lifecycle
/// actions never wait (see the module documentation).
pub(crate) const WORKSPACE_OPERATION_WAIT: Duration = Duration::from_secs(10);

/// How long Close and Delete wait for their own Session's Ways, which they
/// interrupt, or its legacy turn, which they have asked to stop, to let go
/// of the Workspace.
pub(crate) const OWN_WORK_RELEASE_WAIT: Duration = ATTEMPT_OPERATION_RELEASE_TIMEOUT;

/// What holds one Workspace operation: its label, and the Session it acts
/// for, when it acts for one.
struct HeldOperation {
    id: u64,
    label: String,
    session: Option<String>,
}

/// What holds each Workspace operation now, by operation key, while it is
/// held.
#[derive(Default)]
pub(crate) struct WorkspaceOperationLabels {
    held: StdMutex<HashMap<String, HeldOperation>>,
    next: AtomicU64,
}

impl WorkspaceOperationLabels {
    /// Name the holder of `key`'s operation `label` until the returned value
    /// is dropped.
    pub(crate) fn name(self: &Arc<Self>, key: &str, label: String) -> WorkspaceOperationLabel {
        self.name_for(key, label, None)
    }

    /// [`Self::name`] for an operation that acts for `session`.
    pub(crate) fn name_for(
        self: &Arc<Self>,
        key: &str,
        label: String,
        session: Option<&str>,
    ) -> WorkspaceOperationLabel {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                key.to_string(),
                HeldOperation {
                    id,
                    label,
                    session: session.map(str::to_string),
                },
            );
        WorkspaceOperationLabel {
            labels: self.clone(),
            key: key.to_string(),
            id,
        }
    }

    pub(crate) fn get(&self, key: &str) -> Option<String> {
        self.held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(key)
            .map(|held| held.label.clone())
    }

    /// The Session the operation holding `key` acts for, if it named one.
    pub(crate) fn session(&self, key: &str) -> Option<String> {
        self.held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(key)
            .and_then(|held| held.session.clone())
    }
}

/// The name of one Workspace operation's holder; dropping it removes the
/// name (unless a later holder has named itself since).
pub(crate) struct WorkspaceOperationLabel {
    labels: Arc<WorkspaceOperationLabels>,
    key: String,
    id: u64,
}

impl Drop for WorkspaceOperationLabel {
    fn drop(&mut self) {
        let mut held = self
            .labels
            .held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if held.get(&self.key).is_some_and(|held| held.id == self.id) {
            held.remove(&self.key);
        }
    }
}

/// A Workspace operation taken by [`AxocoatlDaemon::take_session_workspace_operation`]
/// or [`AxocoatlDaemon::take_lifecycle_workspace_operation`].
pub(crate) struct WorkspaceOperation {
    // Fields drop in order: the name goes before the operation is released.
    _label: Option<WorkspaceOperationLabel>,
    _guard: OwnedMutexGuard<()>,
}

impl From<OwnedMutexGuard<()>> for WorkspaceOperation {
    fn from(guard: OwnedMutexGuard<()>) -> Self {
        Self {
            _label: None,
            _guard: guard,
        }
    }
}

/// How a request that took no Workspace operation was refused, before the
/// Workspace path and holder are added.
pub(crate) struct WorkspaceRequest {
    /// What the operation is called while this request holds it, for other
    /// refusals ("the creation of a Session").
    pub(crate) doing: String,
    /// What did not happen ("No Session was created").
    pub(crate) refused: String,
}

/// How long a request waits for an operation of its Workspace that is not
/// a turn (a turn is never waited for).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkspaceWait {
    /// An ordinary request: up to [`WORKSPACE_OPERATION_WAIT`].
    Brief,
    /// A lifecycle action: not at all.
    Never,
}

/// How a lifecycle action of one Session that stops its own work (Close,
/// Delete) or replaces it (an environment change) has the Workspace.
pub(crate) enum LifecycleWorkspace {
    /// Taken: the action holds the Workspace from here on.
    Taken(OwnedMutexGuard<()>),
    /// The Session's own registration holds it: its open turn, its first
    /// turn being prepared, or the retained cleanup of a lifecycle action
    /// that did not finish. The action's cleanup takes it over from that
    /// registration once the turn's running command reaches a safe point.
    OwnRegistration,
    /// The Session is not known: it has no Workspace.
    UnknownSession,
}

impl LifecycleWorkspace {
    pub(crate) fn into_guard(self) -> Option<OwnedMutexGuard<()>> {
        match self {
            Self::Taken(guard) => Some(guard),
            Self::OwnRegistration | Self::UnknownSession => None,
        }
    }
}

impl AxocoatlDaemon {
    /// The open turn of `session`, in words: `turn … is running` or `turn …
    /// needs attention`. A running or needs-attention turn holds the
    /// Session's Workspace until it ends.
    pub(super) async fn open_turn_of(&self, session_id: &str) -> Option<String> {
        if let Ok(Some(active)) = self.active_session_turn(session_id).await {
            return Some(format!("turn {} is running", active.turn_id));
        }
        self.list_versioned_session_turns(session_id)
            .await
            .ok()
            .and_then(|entries| {
                entries.into_iter().rev().find_map(|entry| match entry {
                    axocoatl_session::session_history::SessionHistoryEntry::ExecutionV2(turn)
                        if !turn.state.is_closed() =>
                    {
                        Some(format!(
                            "turn {} {}",
                            turn.turn_id.as_str(),
                            if turn.state
                                == axocoatl_session::turn_contract::LogicalTurnState::Running
                            {
                                "is running"
                            } else {
                                "needs attention"
                            }
                        ))
                    }
                    _ => None,
                })
            })
    }

    /// Take `workspace_id`'s operation for a lifecycle action, which never
    /// waits for it: a held Workspace refuses `request` at once, naming its
    /// holder.
    pub(crate) async fn take_lifecycle_workspace_operation(
        &self,
        workspace_id: &str,
        request: WorkspaceRequest,
    ) -> Result<WorkspaceOperation, DaemonError> {
        self.take_workspace_operation_as(workspace_id, None, request, WorkspaceWait::Never)
            .await
    }

    async fn take_workspace_operation_as(
        &self,
        workspace_id: &str,
        session_id: Option<&str>,
        request: WorkspaceRequest,
        wait: WorkspaceWait,
    ) -> Result<WorkspaceOperation, DaemonError> {
        let key = workspace_attempt_operation_key(workspace_id);
        let operation = self.attempt_operation_for_key(key.clone()).await;
        let guard = match operation.clone().try_lock_owned() {
            Ok(guard) => guard,
            Err(_) => {
                self.wait_for_workspace_operation(workspace_id, &key, operation, &request, wait)
                    .await?
            }
        };
        Ok(WorkspaceOperation {
            _label: Some(
                self.workspace_operation_labels
                    .name_for(&key, request.doing, session_id),
            ),
            _guard: guard,
        })
    }

    /// Take the operation of `session_id`'s Workspace for an ordinary
    /// `request` (see the module documentation), named as this Session's.
    /// A Session this daemon does not know has no Workspace; its operation
    /// key is its own, and the request goes on to report it missing.
    pub(crate) async fn take_session_workspace_operation(
        &self,
        session_id: &str,
        request: WorkspaceRequest,
    ) -> Result<WorkspaceOperation, DaemonError> {
        self.take_session_workspace_operation_as(session_id, request, WorkspaceWait::Brief)
            .await
    }

    /// [`Self::take_lifecycle_workspace_operation`] for the Workspace of
    /// `session_id`.
    pub(crate) async fn take_session_lifecycle_operation(
        &self,
        session_id: &str,
        request: WorkspaceRequest,
    ) -> Result<WorkspaceOperation, DaemonError> {
        self.take_session_workspace_operation_as(session_id, request, WorkspaceWait::Never)
            .await
    }

    async fn take_session_workspace_operation_as(
        &self,
        session_id: &str,
        request: WorkspaceRequest,
        wait: WorkspaceWait,
    ) -> Result<WorkspaceOperation, DaemonError> {
        match self.get_session(session_id).await {
            Some(session) => {
                self.take_workspace_operation_as(
                    &session.workspace_id,
                    Some(session_id),
                    request,
                    wait,
                )
                .await
            }
            None => {
                let operation = self.attempt_operation(session_id).await;
                Ok(operation.lock_owned().await.into())
            }
        }
    }

    /// The Workspace of Session `id` for a lifecycle action of it that is
    /// `refused` when it cannot have it ("Session … was not closed"):
    ///
    /// - free: taken at once;
    /// - held by another Session's open turn: refused at once, naming it;
    /// - held by this Session's own registration (its turn, first turn or
    ///   retained cleanup): [`LifecycleWorkspace::OwnRegistration`], which
    ///   the action's cleanup takes over;
    /// - with `stops_own_work` (Close, Delete), held by this Session's own
    ///   running Ways or Ways checks, which are interrupted, or by its own
    ///   legacy turn, which the action has asked to stop: waited for up to
    ///   [`OWN_WORK_RELEASE_WAIT`], then refused, naming what still holds
    ///   it;
    /// - held by anything else: refused at once, naming that operation.
    pub(crate) async fn take_session_lifecycle_workspace(
        &self,
        id: &str,
        refused: &str,
        stops_own_work: bool,
    ) -> Result<LifecycleWorkspace, DaemonError> {
        let Some(session) = self.get_session(id).await else {
            return Ok(LifecycleWorkspace::UnknownSession);
        };
        let key = workspace_attempt_operation_key(&session.workspace_id);
        let gate = self.attempt_operation_for_key(key.clone()).await;
        if let Ok(guard) = gate.clone().try_lock_owned() {
            return Ok(LifecycleWorkspace::Taken(guard));
        }
        let holders = self
            .workspace_holders(&session.workspace_id, Some(id))
            .await;
        if !holders.turns.is_empty() {
            return Err(DaemonError::WorkspaceBusy(format!(
                "{refused}: its Workspace {} {}. A lifecycle action of a Session waits for no \
                 other Session's turn: let that turn finish, or stop it, then try again",
                session.working_dir.display(),
                holders.held_by()
            )));
        }
        if self.session_dispatch_lifecycles.holds_workspace(id)? {
            return Ok(LifecycleWorkspace::OwnRegistration);
        }
        if stops_own_work {
            // Who holds it, as it named itself: this Session's running Ways
            // or Ways checks name this Session; a legacy turn names nothing.
            let holder = self.workspace_operation_labels.session(&key);
            let mut own_work = false;
            if holder.as_deref() == Some(id) {
                if let Some(set) = self.peek_current_attempt_set(id).await? {
                    if Self::attempt_state_is_interruptible(set.state) {
                        own_work = true;
                        match self.request_attempt_cancellation(id, &set.id).await {
                            // The Ways may have finished since the state was
                            // read; their operation is then released normally.
                            Ok(()) | Err(DaemonError::AttemptConflict(_)) => {}
                            Err(error) => return Err(error),
                        }
                    }
                }
            } else if holder.is_none() {
                own_work = self.active_session_turns.lock().await.contains_key(id);
            }
            if own_work {
                return match tokio::time::timeout(OWN_WORK_RELEASE_WAIT, gate.lock_owned()).await {
                    Ok(guard) => Ok(LifecycleWorkspace::Taken(guard)),
                    Err(_) => Err(self
                        .workspace_busy(
                            &session.workspace_id,
                            &key,
                            refused,
                            Some(OWN_WORK_RELEASE_WAIT),
                        )
                        .await),
                };
            }
        }
        Err(self
            .workspace_busy(&session.workspace_id, &key, refused, None)
            .await)
    }

    /// The cleanup of a lifecycle action of Session `id` that has
    /// `workspace` ([`Self::take_session_lifecycle_workspace`]). It waits
    /// only for the Session's own turn to reach a safe point; another
    /// lifecycle action of the Session in progress, or a Workspace another
    /// operation took meanwhile, is refused as busy at once (`refused: …`),
    /// naming it, and nothing changes.
    ///
    /// Once it has the Workspace, the action is named as `request.doing`,
    /// as this Session's, while the returned label lives.
    pub(super) async fn prepare_lifecycle_cleanup(
        &self,
        id: &str,
        request: &WorkspaceRequest,
        workspace: LifecycleWorkspace,
    ) -> Result<
        (
            session_dispatch::SessionDispatchCleanup,
            Option<WorkspaceOperationLabel>,
        ),
        DaemonError,
    > {
        let cleanup = match self
            .session_dispatch_lifecycles
            .prepare_session_cleanup_with(
                id,
                SESSION_LIFECYCLE_CLEANUP_TIMEOUT,
                session_dispatch::CleanupWorkspace::Lifecycle(workspace.into_guard()),
            )
            .await
        {
            Err(DaemonError::WorkspaceBusy(detail))
                if detail == session_dispatch::LIFECYCLE_WORKSPACE_HELD =>
            {
                return Err(self.session_workspace_busy(id, &request.refused).await)
            }
            Err(DaemonError::WorkspaceBusy(detail)) => {
                return Err(DaemonError::WorkspaceBusy(format!(
                    "{}: {detail}",
                    request.refused
                )))
            }
            other => other?,
        };
        let label = self.get_session(id).await.map(|session| {
            self.workspace_operation_labels.name_for(
                &workspace_attempt_operation_key(&session.workspace_id),
                request.doing.clone(),
                Some(id),
            )
        });
        Ok((cleanup, label))
    }

    /// The start lock of Session `id`'s runtime, for a lifecycle action of
    /// it that is `refused` when it cannot have it: a request that reads the
    /// Session's files can be starting its runtime. Waited for up to
    /// [`OWN_WORK_RELEASE_WAIT`], then refused as busy.
    pub(super) async fn lock_session_start_for_lifecycle(
        &self,
        id: &str,
        refused: &str,
    ) -> Result<OwnedMutexGuard<()>, DaemonError> {
        let start = {
            let mut starts = self.sandbox_starts.lock().await;
            starts
                .entry(id.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        if let Ok(guard) = start.clone().try_lock_owned() {
            return Ok(guard);
        }
        tokio::time::timeout(OWN_WORK_RELEASE_WAIT, start.lock_owned())
            .await
            .map_err(|_| {
                DaemonError::WorkspaceBusy(format!(
                    "{refused}: its runtime was still starting after {} s. Try again once it has \
                     started",
                    OWN_WORK_RELEASE_WAIT.as_secs()
                ))
            })
    }

    /// The busy refusal of a lifecycle action of Session `id` whose cleanup
    /// found its Workspace taken by another operation.
    pub(super) async fn session_workspace_busy(&self, id: &str, refused: &str) -> DaemonError {
        match self.get_session(id).await {
            Some(session) => {
                self.workspace_busy(
                    &session.workspace_id,
                    &workspace_attempt_operation_key(&session.workspace_id),
                    refused,
                    None,
                )
                .await
            }
            None => DaemonError::WorkspaceBusy(format!(
                "{refused}: another operation holds its Workspace. Try again once it has ended"
            )),
        }
    }

    /// The operation is held: refuse at once when a turn holds it or the
    /// request never waits, else wait for it as long as
    /// [`WORKSPACE_OPERATION_WAIT`].
    async fn wait_for_workspace_operation(
        &self,
        workspace_id: &str,
        key: &str,
        operation: Arc<tokio::sync::Mutex<()>>,
        request: &WorkspaceRequest,
        wait: WorkspaceWait,
    ) -> Result<OwnedMutexGuard<()>, DaemonError> {
        let holders = self.workspace_holders(workspace_id, None).await;
        if !holders.turns.is_empty() || wait == WorkspaceWait::Never {
            return Err(self
                .workspace_busy(workspace_id, key, &request.refused, None)
                .await);
        }
        match tokio::time::timeout(WORKSPACE_OPERATION_WAIT, operation.lock_owned()).await {
            Ok(guard) => Ok(guard),
            Err(_) => Err(self
                .workspace_busy(
                    workspace_id,
                    key,
                    &request.refused,
                    Some(WORKSPACE_OPERATION_WAIT),
                )
                .await),
        }
    }

    /// The busy refusal of a request that found `workspace_id`'s operation
    /// held: by an open turn (named; it holds the Workspace until it ends),
    /// or by another operation, named when it named itself, which the
    /// request waited for as long as `waited`, if at all.
    pub(super) async fn workspace_busy(
        &self,
        workspace_id: &str,
        key: &str,
        refused: &str,
        waited: Option<Duration>,
    ) -> DaemonError {
        let holders = self.workspace_holders(workspace_id, None).await;
        let path = self
            .get_workspace(workspace_id)
            .await
            .map(|workspace| workspace.canonical_path.display().to_string())
            .unwrap_or_else(|| workspace_id.to_string());
        let detail = if !holders.turns.is_empty() {
            format!(
                "{} {}. A turn holds its Workspace until it ends: try again once it has ended, \
                 or stop it",
                path,
                holders.held_by()
            )
        } else {
            let holder = match self.workspace_operation_labels.get(key) {
                Some(label) => format!("is held by another operation: {label}"),
                None => holders.held_by(),
            };
            let waited = waited
                .map(|waited| format!(", which did not end within {} s", waited.as_secs()))
                .unwrap_or_default();
            format!("{path} {holder}{waited}. Try again once it has ended")
        };
        DaemonError::WorkspaceBusy(format!("{refused}: its Workspace {detail}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_lasts_while_its_holder_holds_the_operation() {
        let labels = Arc::new(WorkspaceOperationLabels::default());
        let first = labels.name("workspace:a", "the creation of a Session".into());
        assert_eq!(
            labels.get("workspace:a").as_deref(),
            Some("the creation of a Session")
        );
        // A later holder's name is not removed by an earlier one's drop.
        let second = labels.name("workspace:a", "a Git change".into());
        drop(first);
        assert_eq!(labels.get("workspace:a").as_deref(), Some("a Git change"));
        drop(second);
        assert_eq!(labels.get("workspace:a"), None);
    }

    #[tokio::test]
    async fn the_name_goes_before_the_operation_is_released() {
        let labels = Arc::new(WorkspaceOperationLabels::default());
        let gate = Arc::new(tokio::sync::Mutex::new(()));
        let operation = WorkspaceOperation {
            _label: Some(labels.name("workspace:a", "Ways checks".into())),
            _guard: gate.clone().lock_owned().await,
        };
        let waiter = {
            let (labels, gate) = (labels.clone(), gate.clone());
            tokio::spawn(async move {
                let _guard = gate.lock_owned().await;
                labels.get("workspace:a")
            })
        };
        tokio::task::yield_now().await;
        drop(operation);
        assert_eq!(waiter.await.unwrap(), None);
    }
}
