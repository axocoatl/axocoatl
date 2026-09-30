//! Native ingress resolves the same registry entry as history and controls.
use super::*;
use axocoatl_memory::activation_state::ActivationStateStore;
use axocoatl_session::execution_content::ExecutionContentStore;
use axocoatl_session::execution_store::SessionExecutionStore;
use axocoatl_session::turn_contract::*;

pub(crate) enum NativeFirstTurnExisting {
    Unstarted,
    Registered {
        controller: SessionDispatchController,
        repository: EvidenceRef,
    },
    Retained(Box<crate::session_control_plane::SessionTurnControlPlane>),
}
impl SessionDispatchRegistry {
    pub(crate) fn native_first_turn_existing(
        &self,
        session_id: &str,
        turn_id: &LogicalTurnId,
        source: &str,
    ) -> Result<NativeFirstTurnExisting> {
        let state = self
            .state
            .lock()
            .map_err(|_| failure("Session dispatch registry failed"))?;
        if state.closed || state.closing_sessions.contains(session_id) {
            return Err(failure("Session lifecycle admission is closed"));
        }
        let Some(entry) = state.entries.get(session_id) else {
            if let Some(pending) = state.pending.get(session_id) {
                if pending.native_turn_replay(turn_id, source)? {
                    return match pending.control_plane(turn_id.as_str())? {
                        RegisteredControlPlane::Found(view) => {
                            Ok(NativeFirstTurnExisting::Retained(view))
                        }
                        _ => Err(failure(
                            "retained native replay lost its exact turn projection",
                        )),
                    };
                }
                return Ok(NativeFirstTurnExisting::Unstarted);
            }
            return Err(failure("native turn requires an actually retained Session"));
        };
        if entry.retired.load(Ordering::SeqCst) {
            return Err(failure("Session execution ownership was retired"));
        }
        let existing = entry.controller.with_team_stores(|canonical, content, _| {
            if canonical
                .turn(turn_id)
                .map_err(|error| failure(error.to_string()))?
                .is_none()
            {
                return Ok(false);
            }
            let (_, admission) = content
                .turn_admission(canonical, turn_id)
                .map_err(|error| failure(error.to_string()))?
                .ok_or_else(|| failure("retained native turn lacks complete request"))?;
            if admission.source != source {
                return Err(failure("native turn ID has a different complete request"));
            }
            Ok(true)
        })?;
        let snapshot = entry
            .controller
            .snapshot()
            .map_err(|error| failure(error.to_string()))?;
        if !existing {
            if !snapshot
                .contract()
                .state()
                .is_some_and(LogicalTurnState::is_closed)
            {
                return Err(failure(
                    "resolve the current unfinished turn before sending a new request",
                ));
            }
            return Ok(NativeFirstTurnExisting::Unstarted);
        }
        if snapshot.turn_id() != turn_id
            || snapshot.contract().state() != Some(LogicalTurnState::Running)
        {
            return entry
                .controller
                .control_plane_for_turn(turn_id.as_str())
                .map_err(|error| failure(error.to_string()))?
                .map(|view| NativeFirstTurnExisting::Retained(Box::new(view)))
                .ok_or_else(|| failure("retained native replay lost its exact projection"));
        }
        let mut reference = entry
            .reference
            .lock()
            .map_err(|_| failure("native repository reference failed"))?;
        if reference.is_none() {
            let retained = entry
                .controller
                .retain_repository_resource(entry.owner()?)
                .map_err(|error| failure(error.to_string()))?;
            *reference = Some(retained);
        }
        Ok(NativeFirstTurnExisting::Registered {
            controller: entry.controller.clone(),
            repository: reference.as_ref().unwrap().clone(),
        })
    }

    pub(crate) fn native_pending_token(
        &self,
        session_id: &str,
    ) -> Result<Option<PendingSessionToken>> {
        let pending = {
            let state = self
                .state
                .lock()
                .map_err(|_| failure("Session dispatch registry failed"))?;
            if state.closed || state.closing_sessions.contains(session_id) {
                return Err(failure("Session lifecycle admission is closed"));
            }
            state.pending.contains_key(session_id)
        };
        if pending {
            self.prepare_first_turn(session_id).map(Some)
        } else {
            Ok(None)
        }
    }
    pub(crate) fn native_reacquisition_needed(&self, session_id: &str) -> Result<bool> {
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
            .ok_or_else(|| failure("Session has no retained execution owner"))?;
        let needed = entry
            .between_turns
            .lock()
            .map_err(|_| failure("released repository identity failed"))?
            .is_some();
        Ok(needed)
    }
    pub(crate) fn begin_native_successor_checked(
        &self,
        session_id: &str,
        spec: SuccessorTurn,
        validate: impl FnOnce(
            &SessionExecutionStore,
            &ExecutionContentStore,
            &ActivationStateStore,
        ) -> Result<()>,
    ) -> Result<(SessionDispatchController, EvidenceRef)> {
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
        if entry.retired.load(Ordering::SeqCst) || !entry.owner()?.execution_is_idle()? {
            return Err(failure(
                "successor must wait for actual repository settlement",
            ));
        }
        // Team Apply also holds this exact registry lock, so validation and
        // canonical Begin cannot select different applied revisions.
        entry
            .controller
            .with_team_stores(|canonical, content, memory| validate(canonical, content, memory))?;
        let reference = entry
            .reference
            .lock()
            .map_err(|_| failure("repository registration reference failed"))?
            .clone()
            .ok_or_else(|| failure("successor lacks retained physical repository"))?;
        entry
            .controller
            .begin_registered_successor(spec)
            .map_err(|error| failure(error.to_string()))?;
        Ok((entry.controller.clone(), reference))
    }
}

impl SessionDispatchRegistry {
    pub(crate) fn native_control_runtime(
        &self,
        session_id: &str,
        turn_id: &LogicalTurnId,
    ) -> Result<(SessionDispatchController, EvidenceRef)> {
        let state = self
            .state
            .lock()
            .map_err(|_| failure("Session registry failed"))?;
        if state.closed || state.closing_sessions.contains(session_id) {
            return Err(failure("Session is closing"));
        }
        let entry = state
            .entries
            .get(session_id)
            .ok_or_else(|| failure("Session has no retained native controller"))?;
        if entry.retired.load(Ordering::SeqCst) {
            return Err(failure("Session execution owner was retired"));
        }
        let snapshot = entry
            .controller
            .snapshot()
            .map_err(|error| failure(error.to_string()))?;
        if snapshot.turn_id() != turn_id {
            return Err(failure("control driver targets another current turn"));
        }
        let reference = entry
            .reference
            .lock()
            .map_err(|_| failure("repository registration reference failed"))?
            .clone()
            .ok_or_else(|| failure("control driver lacks actual repository owner"))?;
        Ok((entry.controller.clone(), reference))
    }
}

impl SessionDispatchRegistry {
    /// Whether the unchanged approved runtime may be prepared again at its
    /// current generation although a turn is unfinished. That turn must be
    /// paused with nothing running, no stop in progress and no unknown
    /// effect, and no controller or repository owner may hold a runtime for
    /// it. Its Continue/Finish then reattach to the same generation and
    /// revalidate the repository; a changed plan still requires closure.
    pub(crate) fn require_native_environment_retry_ready(&self, session_id: &str) -> Result<()> {
        let state = self
            .state
            .lock()
            .map_err(|_| failure("Session registry failed"))?;
        if state.closed || state.closing_sessions.contains(session_id) {
            return Err(failure("Session lifecycle admission is closed"));
        }
        if state.entries.contains_key(session_id) {
            return Err(failure(
                "The paused turn still holds its runtime; Stop or Finish it before changing the runtime",
            ));
        }
        let entry = state.pending.get(session_id).ok_or_else(|| {
            failure("Session canonical history is unavailable for runtime replacement")
        })?;
        if entry.retired.load(Ordering::SeqCst) || entry.closed_history.load(Ordering::SeqCst) {
            return Err(failure("Session lifecycle admission is closed"));
        }
        if entry.holds_repository_owner()? {
            return Err(failure(
                "The paused turn still holds its runtime; Stop or Finish it before changing the runtime",
            ));
        }
        entry.with_team_stores(|canonical, _, _| {
            let Some((_, turn)) = canonical
                .unfinished_turn()
                .map_err(|error| failure(error.to_string()))?
            else {
                return Ok(());
            };
            if turn.state() != Some(LogicalTurnState::NeedsAttention)
                || !turn.epochs().last().is_some_and(|epoch| {
                    matches!(epoch.state, EpochState::Paused | EpochState::Interrupted)
                })
                || turn.stop_requested().is_some()
                || turn.has_unknown_effects()
                || turn
                    .activations()
                    .iter()
                    .any(|activation| activation.state == ActivationState::Running)
            {
                return Err(failure(
                    "Only a paused turn with settled effects can keep its runtime plan; Stop or Finish the unfinished Session turn first",
                ));
            }
            Ok(())
        })
    }

    pub(crate) fn require_native_environment_change_ready(&self, session_id: &str) -> Result<()> {
        let state = self
            .state
            .lock()
            .map_err(|_| failure("Session registry failed"))?;
        if state.closed {
            return Err(failure("Daemon is shutting down"));
        }
        let inspect = |canonical: &SessionExecutionStore,
                       _: &mut ExecutionContentStore,
                       _: &mut ActivationStateStore| {
            if canonical
                .unfinished_turn()
                .map_err(|error| failure(error.to_string()))?
                .is_some()
            {
                return Err(failure(
                    "Stop or Finish the unfinished Session turn before changing its runtime",
                ));
            }
            Ok(())
        };
        if let Some(entry) = state.entries.get(session_id) {
            entry.controller.with_team_stores(inspect)
        } else if let Some(entry) = state.pending.get(session_id) {
            entry.with_team_stores(inspect)
        } else {
            Err(failure(
                "Session canonical history is unavailable for runtime replacement",
            ))
        }
    }
}
