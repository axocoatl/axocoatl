//! Exact human whole-turn cancellation through the existing canonical owner.
//! Request acknowledgement is not process/effect settlement or conversation promotion.
use super::*;

#[derive(Debug)]
pub struct TurnStopReceipt {
    first_request: bool,
    accepted: DurableTurnReceipt,
    settled: Option<FinalizedTurn>,
}

impl TurnStopReceipt {
    pub fn first_request(&self) -> bool {
        self.first_request
    }
    pub fn accepted(&self) -> &DurableTurnReceipt {
        &self.accepted
    }
    /// Exact canonical Cancelled closure and completed-generation promotion.
    /// Unknown external effects remain unknown and retain their repository owner.
    pub fn settled(&self) -> Option<&FinalizedTurn> {
        self.settled.as_ref()
    }
}

impl SessionDispatchController {
    /// Cancel setup waits without starting another controller task. The actual
    /// dispatch edge still revalidates authority under the same canonical lock.
    pub(super) async fn wait_for_turn_stop(&self) {
        let changed = match self.lock() {
            Ok(state) => state.changed.clone(),
            Err(_) => return,
        };
        loop {
            let notification = changed.notified();
            tokio::pin!(notification);
            notification.as_mut().enable();
            match self.lock() {
                Ok(state) if state.execution_admission().is_ok() => {}
                _ => return,
            }
            notification.await;
        }
    }
    /// Only the authenticated daemon compatibility ingress may originate this
    /// human-only action. Session/turn strings are selectors, never authority.
    pub(crate) fn request_human_turn_stop(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> Result<TurnStopReceipt> {
        let mut state = self.lock()?;
        state.ready()?;
        if state.execution_admission_closed {
            return Err(error("Session lifecycle has retired Stop admission"));
        }
        if state.canonical.owner().session_id.as_str() != session_id
            || state.turn_id.as_str() != turn_id
        {
            return Err(error("whole-turn Stop targets another Session or turn"));
        }
        let snapshot = state.canonical.snapshot(&state.turn_id).map_err(error)?;
        let existing = match snapshot.contract().stop_requested() {
            Some(intent) if intent.partial_finish.is_some() => {
                return Err(error(
                    "partial Finish already owns this exact closing request",
                ))
            }
            Some(intent) => Some(
                state
                    .canonical
                    .records()
                    .map_err(error)?
                    .iter()
                    .find(|record| record.command_id == intent.command_id)
                    .cloned()
                    .ok_or_else(|| error("Stop intent has no exact canonical request"))?,
            ),
            None => None,
        };
        let first_request = existing.is_none();
        let envelope = match existing {
            Some(envelope) => envelope,
            None => {
                if snapshot
                    .contract()
                    .state()
                    .is_none_or(LogicalTurnState::is_closed)
                {
                    return Err(error("whole-turn Stop requires the exact unfinished turn"));
                }
                // The event itself is the durable human request. Anchor it to
                // the actual retained initial request rather than allocating a
                // second body that could make Stop fail when content is full.
                let evidence = snapshot
                    .request_ref()
                    .cloned()
                    .ok_or_else(|| error("whole-turn Stop has no retained source request"))?;
                let bytes = serde_json::to_vec(&(
                    snapshot.journal_id(),
                    snapshot.turn_id(),
                    "human-whole-turn-stop-v1",
                ))
                .map_err(error)?;
                TurnContractEnvelope {
                    schema_version: TURN_CONTRACT_SCHEMA_VERSION,
                    command_id: CommandId::new(format!("turn-stop-{:x}", Sha256::digest(bytes)))
                        .map_err(error)?,
                    expected_revision: snapshot.contract().revision(),
                    session_id: snapshot.owner().session_id.clone(),
                    turn_id: snapshot.turn_id().clone(),
                    event: TurnContractEvent::RequestTurnStop { evidence },
                }
            }
        };
        let result = (|| {
            let receipt = state.canonical.append(envelope).map_err(error)?;
            // Journal acknowledgement precedes every cancellation signal. This
            // lock also orders Stop against actor admission and tool dispatch.
            state.reconcile_control_commands()?;
            let snapshot = state.canonical.snapshot(&state.turn_id).map_err(error)?;
            let settled = if snapshot.contract().state() == Some(LogicalTurnState::Cancelled) {
                Some(state.finalized_turn()?)
            } else {
                None
            };
            Ok(TurnStopReceipt {
                first_request,
                accepted: receipt,
                settled,
            })
        })();
        state.changed.notify_waiters();
        state.fail_closed(result)
    }
}

impl DispatchState {
    /// Runs on open, command/actor settlement, and final owner-ticket release.
    /// It never starts a provider, reconstructs an actor, or replays a tool.
    pub(super) fn reconcile_turn_stop(&mut self) -> Result<()> {
        self.ready()?;
        let snapshot = self.canonical.snapshot(&self.turn_id).map_err(error)?;
        let Some(intent) = snapshot.contract().stop_requested().cloned() else {
            return Ok(());
        };
        let revision = self.authority.revision().map_err(error)?;
        self.authority.close_dispatch(revision).map_err(error)?;
        for bound in self.bound.values() {
            bound.control.cancel();
        }
        for check in self.repository_checks.values() {
            check.cancel();
        }
        if snapshot
            .contract()
            .state()
            .is_some_and(LogicalTurnState::is_closed)
        {
            self.reconcile_promotions()?;
            return Ok(());
        }
        // Factory waits have no actor or dispatch capability. Record that
        // proof while their original epoch is live; never infer zero usage for
        // a recovered/lost actor from an absent process-local binding.
        if snapshot.contract().state() == Some(LogicalTurnState::Running) {
            let unbound = snapshot
                .contract()
                .activations()
                .iter()
                .filter(|item| {
                    item.state == ActivationState::Running
                        && !self.bound.contains_key(&item.activation.activation_id)
                })
                .cloned()
                .collect::<Vec<_>>();
            for item in unbound {
                super::driver::record_undispatched(self, &snapshot, &item.input)?;
                self.append(
                    &format!(
                        "turn-stop-unbound:{}",
                        item.activation.activation_id.as_str()
                    ),
                    TurnContractEvent::FailActivation {
                        activation: item.activation.clone(),
                        evidence: intent.evidence.clone(),
                    },
                )?;
            }
        }
        let current = self.canonical.snapshot(&self.turn_id).map_err(error)?;
        if current
            .contract()
            .activations()
            .iter()
            .any(|item| item.state == ActivationState::Running)
            || !self.repository_checks.is_empty()
            || !self
                .execution_lifetimes
                .only_driver_remains(self.driver.is_some())
        {
            return Ok(());
        }
        let envelope = TurnContractEnvelope {
            schema_version: TURN_CONTRACT_SCHEMA_VERSION,
            command_id: CommandId::new(format!(
                "turn-stop-close-{:x}",
                Sha256::digest(intent.command_id.as_str().as_bytes())
            ))
            .map_err(error)?,
            expected_revision: current.contract().revision(),
            session_id: current.owner().session_id.clone(),
            turn_id: current.turn_id().clone(),
            event: TurnContractEvent::Close {
                closure: intent.closure,
            },
        };
        self.close_and_promote(envelope)?;
        Ok(())
    }
}
