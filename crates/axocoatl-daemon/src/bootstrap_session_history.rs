//! Version-selected History for existing lifecycle callers.
//!
//! The legacy ledger remains the actual v1 writer. An upgraded Session never
//! falls back to it because a controller is absent or its history is empty.
//! Rewind, Ways adoption, environment replacement and deletion require their
//! own v2 accepted-state/reference protocols before they may mutate v2 state.
//! These guards are compatibility admission, not process-settlement evidence.

use super::*;
use axocoatl_session::execution_ownership::DataRootFormatOwnership;

#[derive(Debug, Clone, Copy)]
pub(super) enum HistoryMutation {
    Rewind,
    ExploreAttempts,
    KeepAttempt,
    DiscardAttempts,
    ReplaceEnvironment,
    DeleteSession,
}

impl HistoryMutation {
    fn description(self) -> &'static str {
        match self {
            Self::Rewind => "Rewind",
            Self::ExploreAttempts => "Explore several ways",
            Self::KeepAttempt => "Keep this attempt",
            Self::DiscardAttempts => "Discard these attempts",
            Self::ReplaceEnvironment => "Replace the Session environment",
            Self::DeleteSession => "Delete this Session",
        }
    }
}

fn require_legacy_history_write(
    ownership: &DataRootFormatOwnership,
    retained_execution: bool,
    operation: HistoryMutation,
) -> Result<(), DaemonError> {
    if retained_execution || matches!(ownership, DataRootFormatOwnership::Upgraded(_)) {
        return Err(DaemonError::SessionConflict(format!(
            "{} requires versioned Session history and accepted-state cleanup that is not enabled in this build; no legacy history mutation was performed",
            operation.description(),
        )));
    }
    Ok(())
}

/// Select from actual retained authority. A missing upgraded source is a
/// visible error even when the still-present legacy ledger contains rows.
fn select_history(
    ownership: &DataRootFormatOwnership,
    canonical: Option<SessionHistory>,
    legacy: &SessionTurnStore,
    session_id: &str,
) -> Result<SessionHistory, DaemonError> {
    if let Some(history) = canonical {
        if history.session_id() != session_id {
            return Err(DaemonError::SessionConflict(
                "canonical History belongs to another Session".into(),
            ));
        }
        return Ok(history);
    }
    if matches!(ownership, DataRootFormatOwnership::Upgraded(_)) {
        return Err(DaemonError::SessionConflict(
            "upgraded Session History is not retained by this host; legacy History cannot substitute for it".into(),
        ));
    }
    SessionHistory::from_legacy(legacy, session_id)
        .map_err(|error| DaemonError::Session(error.to_string()))
}

impl AxocoatlDaemon {
    pub(super) async fn legacy_checkpoint_history_import_required(
        &self,
        session_id: &str,
    ) -> Result<bool, DaemonError> {
        if self
            .session_dispatch_lifecycles
            .retains_session(session_id)?
            || matches!(
                self._data_dir_lease.ownership,
                DataRootFormatOwnership::Upgraded(_)
            )
        {
            // A successful sealed read establishes that the old checkpoint
            // import is already behind this Session's immutable frontier.
            self.versioned_session_history_snapshot(session_id).await?;
            return Ok(false);
        }
        Ok(true)
    }

    pub(super) async fn versioned_session_history_snapshot(
        &self,
        session_id: &str,
    ) -> Result<SessionHistory, DaemonError> {
        self._data_dir_lease
            .ownership
            .verify_root(&self.data_root)
            .map_err(|error| DaemonError::Session(error.to_string()))?;
        let canonical = self
            .session_dispatch_lifecycles
            .history_snapshot(session_id)?;
        let legacy = self.session_turn_store.lock().await;
        select_history(
            &self._data_dir_lease.ownership,
            canonical,
            &legacy,
            session_id,
        )
    }

    /// Call before the first actor, checkpoint, Git, runtime or Session-owner
    /// effect, then repeat at the actual ledger write under the caller's gate.
    pub(super) fn require_legacy_history_mutation(
        &self,
        session_id: &str,
        operation: HistoryMutation,
    ) -> Result<(), DaemonError> {
        self._data_dir_lease
            .ownership
            .verify_root(&self.data_root)
            .map_err(|error| DaemonError::Session(error.to_string()))?;
        require_legacy_history_write(
            &self._data_dir_lease.ownership,
            self.session_dispatch_lifecycles
                .retains_session(session_id)?,
            operation,
        )
    }

    pub(super) async fn legacy_session_history_writer(
        &self,
        session_id: &str,
        operation: HistoryMutation,
    ) -> Result<tokio::sync::MutexGuard<'_, SessionTurnStore>, DaemonError> {
        self.require_legacy_history_mutation(session_id, operation)?;
        let writer = self.session_turn_store.lock().await;
        self.require_legacy_history_mutation(session_id, operation)?;
        Ok(writer)
    }
}

#[cfg(test)]
#[path = "bootstrap_session_history_tests.rs"]
mod tests;

#[path = "bootstrap_session_history_read.rs"]
pub(super) mod read;
