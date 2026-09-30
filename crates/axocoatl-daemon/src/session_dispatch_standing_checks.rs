use super::*;
use crate::bootstrap::native_turn::{NativeFirstTurnRequest, NativeStandingWork};
use axocoatl_session::execution_content::RepositorySnapshotPhase;

impl DispatchState {
    pub(super) fn standing_work(&self) -> Result<Option<NativeStandingWork>> {
        let Some((_, admission)) = self
            .content
            .turn_admission(&self.canonical, &self.turn_id)
            .map_err(error)?
        else {
            return Ok(None);
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&admission.source) else {
            return Ok(None);
        };
        if value
            .get("standing_work")
            .is_none_or(serde_json::Value::is_null)
        {
            return Ok(None);
        }
        let request: NativeFirstTurnRequest = serde_json::from_value(value).map_err(error)?;
        Ok(request.standing_work)
    }
}

impl SessionDispatchController {
    /// Producer metadata selects a candidate; only the owned observation proves
    /// that this execution is actually using that candidate.
    pub(crate) fn validate_standing_candidate(&self, _activation: &ActivationRef) -> Result<()> {
        let state = self.lock()?;
        let Some(work) = state.standing_work()? else {
            return Ok(());
        };
        let snapshot = state.canonical.snapshot(&state.turn_id).map_err(error)?;
        let mut observations = Vec::new();
        for item in snapshot.contract().activations() {
            observations.extend(
                state
                    .content
                    .repository_snapshots(&snapshot, &item.activation)
                    .map_err(error)?,
            );
        }
        let verified = observations
            .iter()
            .filter(|item| item.content.phase == RepositorySnapshotPhase::Before)
            .any(|item| {
                let before = &item.content;
                if before.tree_sha256.is_none() {
                    return false;
                }
                match work.subject.kind.as_str() {
                    "commit" | "git_commit" => {
                        before.head.as_deref() == Some(work.subject.version.as_str())
                            && before.patch_bytes == 0
                    }
                    "tree" | "tree_sha256" | "build" | "artifact" | "release_candidate" => {
                        before.tree_sha256.as_deref() == Some(work.subject.version.as_str())
                    }
                    // A signal names no producer candidate. The host rechecked
                    // the signaled source bytes immediately before admission;
                    // this execution's own Before capture is its starting tree.
                    "signal_field" => true,
                    _ => false,
                }
            });
        if !verified {
            return Err(error("The declared candidate is not verified in this Session checkout; a commit must match a clean checkout, and build/artifact versions must match the captured tree SHA-256"));
        }
        Ok(())
    }
}
