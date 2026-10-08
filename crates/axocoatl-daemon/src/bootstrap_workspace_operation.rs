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
//!   prepared, a loadout run's admission, a Git change, Ways checks): waited
//!   for up to [`WORKSPACE_OPERATION_WAIT`], then refused as busy, naming
//!   that operation.
//!
//! Each operation taken here, by a run's admission or by several Ways,
//! names itself while it holds the Workspace, so a refusal can say what
//! holds it.
use super::*;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::OwnedMutexGuard;

/// How long a request a person waits on waits for an operation of its
/// Workspace that is not a turn before it is refused as busy.
pub(crate) const WORKSPACE_OPERATION_WAIT: Duration = Duration::from_secs(10);

/// What holds each Workspace operation now, by operation key, while it is
/// held.
#[derive(Default)]
pub(crate) struct WorkspaceOperationLabels {
    held: StdMutex<HashMap<String, (u64, String)>>,
    next: AtomicU64,
}

impl WorkspaceOperationLabels {
    /// Name the holder of `key`'s operation `label` until the returned value
    /// is dropped.
    pub(crate) fn name(self: &Arc<Self>, key: &str, label: String) -> WorkspaceOperationLabel {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key.to_string(), (id, label));
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
            .map(|(_, label)| label.clone())
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
        if held.get(&self.key).is_some_and(|(id, _)| *id == self.id) {
            held.remove(&self.key);
        }
    }
}

/// A Workspace operation taken by [`AxocoatlDaemon::take_workspace_operation`].
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

    /// Whether `session_id`'s own work holds its Workspace: its open turn
    /// (registered or in its History), a turn it is still preparing, or its
    /// Ways. Close and Delete stop that work themselves and wait for it.
    pub(super) async fn session_holds_own_workspace_work(
        &self,
        session_id: &str,
    ) -> Result<bool, DaemonError> {
        Ok(self
            .session_dispatch_lifecycles
            .holds_workspace(session_id)?
            || self.open_turn_of(session_id).await.is_some()
            || self.peek_current_attempt_set(session_id).await?.is_some())
    }

    /// Take `workspace_id`'s operation for `request` (see the module
    /// documentation).
    pub(crate) async fn take_workspace_operation(
        &self,
        workspace_id: &str,
        request: WorkspaceRequest,
    ) -> Result<WorkspaceOperation, DaemonError> {
        let key = workspace_attempt_operation_key(workspace_id);
        let operation = self.attempt_operation_for_key(key.clone()).await;
        let guard = match operation.clone().try_lock_owned() {
            Ok(guard) => guard,
            Err(_) => {
                self.wait_for_workspace_operation(workspace_id, &key, operation, &request)
                    .await?
            }
        };
        Ok(WorkspaceOperation {
            _label: Some(self.workspace_operation_labels.name(&key, request.doing)),
            _guard: guard,
        })
    }

    /// [`Self::take_workspace_operation`] for the Workspace of `session_id`.
    /// A Session this daemon does not know has no Workspace; its operation
    /// key is its own, and the request goes on to report it missing.
    pub(crate) async fn take_session_workspace_operation(
        &self,
        session_id: &str,
        request: WorkspaceRequest,
    ) -> Result<WorkspaceOperation, DaemonError> {
        match self.get_session(session_id).await {
            Some(session) => {
                self.take_workspace_operation(&session.workspace_id, request)
                    .await
            }
            None => {
                let operation = self.attempt_operation(session_id).await;
                Ok(operation.lock_owned().await.into())
            }
        }
    }

    /// The operation is held: refuse at once when a turn holds it, else wait
    /// for it as long as [`WORKSPACE_OPERATION_WAIT`].
    async fn wait_for_workspace_operation(
        &self,
        workspace_id: &str,
        key: &str,
        operation: Arc<tokio::sync::Mutex<()>>,
        request: &WorkspaceRequest,
    ) -> Result<OwnedMutexGuard<()>, DaemonError> {
        let holders = self.workspace_holders(workspace_id, None).await;
        if !holders.turns.is_empty() {
            return Err(self
                .workspace_busy(workspace_id, key, &request.refused, false)
                .await);
        }
        match tokio::time::timeout(WORKSPACE_OPERATION_WAIT, operation.lock_owned()).await {
            Ok(guard) => Ok(guard),
            Err(_) => Err(self
                .workspace_busy(workspace_id, key, &request.refused, true)
                .await),
        }
    }

    /// The busy refusal of a request that found `workspace_id`'s operation
    /// held: by an open turn (named; it holds the Workspace until it ends),
    /// or by another operation, named when it named itself, which the
    /// request waited for as long as [`WORKSPACE_OPERATION_WAIT`] when
    /// `waited`.
    pub(super) async fn workspace_busy(
        &self,
        workspace_id: &str,
        key: &str,
        refused: &str,
        waited: bool,
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
            let waited = if waited {
                format!(
                    ", which did not end within {} s",
                    WORKSPACE_OPERATION_WAIT.as_secs()
                )
            } else {
                String::new()
            };
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
