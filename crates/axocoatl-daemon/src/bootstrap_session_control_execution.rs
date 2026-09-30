//! An applied human command can transfer the existing turn driver once. Runtime
//! resources remain owned by the same controller/registry through cancellation.
use super::*;
use crate::session_dispatch::{
    HumanControlAction, HumanControlActionRequest, SessionDispatchController,
};
use axocoatl_core::{MeasuredTokenUsage, TokenUsageStats};
use axocoatl_session::control_command::{CommandReceiptView, ControlCommandState};
use axocoatl_session::session_history::SessionHistoryEntry;
use axocoatl_session::turn_contract::LogicalTurnId;

fn control_failure(error: impl std::fmt::Display) -> DaemonError {
    DaemonError::SessionConflict(error.to_string())
}
impl AxocoatlDaemon {
    pub(super) async fn drive_applied_native_control(
        &self,
        request: &HumanControlActionRequest,
        receipt: &CommandReceiptView,
    ) -> Result<(), DaemonError> {
        if matches!(
            request.action,
            HumanControlAction::Stop | HumanControlAction::Finish
        ) && matches!(
            receipt.state,
            ControlCommandState::Applied | ControlCommandState::Settled
        ) {
            settle_closed_native_control(
                &self.session_dispatch_lifecycles,
                &self.stream_bus,
                request.session_id.as_str(),
                &request.turn_id,
            )?;
        }
        if !matches!(
            request.action,
            HumanControlAction::Continue
                | HumanControlAction::Retry
                | HumanControlAction::Revise
                | HumanControlAction::Resume
        ) || !matches!(
            receipt.state,
            ControlCommandState::Applied | ControlCommandState::Settled
        ) {
            return Ok(());
        }
        let (controller, repository) = self
            .session_dispatch_lifecycles
            .native_control_runtime(request.session_id.as_str(), &request.turn_id)?;
        // Resume addresses an existing live hook wait. Its driver already owns
        // execution; interrupted/lost waits are rejected by the canonical host.
        if request.action == HumanControlAction::Resume {
            return Ok(());
        }
        let factory = self.native_session_activation_factory(&controller)?;
        let driver = controller
            .prepare_native_control_driver(
                &request.command_id,
                repository,
                self.stream_bus.clone(),
                factory,
            )
            .map_err(control_failure)?;
        let Some(driver) = driver else {
            return Ok(());
        };
        let registry = self.session_dispatch_lifecycles.clone();
        let bus = self.stream_bus.clone();
        let sessions = self.session_store.clone();
        let session_id = request.session_id.as_str().to_owned();
        let turn_id = request.turn_id.clone();
        // The driver and every child hold existing lifecycle execution tickets;
        // Close/Delete/shutdown await those owners rather than this HTTP call.
        tokio::spawn(async move {
            let result = driver.run().await;
            if result
                .as_ref()
                .is_ok_and(|outcome| outcome.finalized.is_some())
            {
                if let Err(error) = registry.release_after_turn(&session_id, &turn_id) {
                    tracing::warn!(session=%session_id,turn=%turn_id.as_str(),%error,"native control repository release failed");
                }
            }
            publish_control_disposition(&controller, &bus, &turn_id);
            if let Err(error) = result {
                tracing::warn!(session=%session_id,turn=%turn_id.as_str(),%error,"native control execution needs attention");
            }
            let mut sessions = sessions.lock().await;
            if sessions
                .get(&session_id)
                .is_some_and(|session| session.status != axocoatl_session::SessionStatus::Closed)
            {
                if let Err(error) = sessions.touch(&session_id) {
                    tracing::warn!(session=%session_id,%error,"could not update Session activity after control execution");
                }
            }
        });
        Ok(())
    }
}

/// A late Stop or Finish can close a turn after its driver already returned
/// NeedsAttention. It must make the same exact resource-release transition as
/// a completed driver, before other Workspace operations can acquire the gate.
pub(super) fn settle_closed_native_control(
    registry: &super::session_dispatch::SessionDispatchRegistry,
    bus: &crate::stream::StreamBus,
    session_id: &str,
    turn_id: &LogicalTurnId,
) -> Result<bool, DaemonError> {
    let (controller, _) = registry.native_control_runtime(session_id, turn_id)?;
    if !controller
        .snapshot()
        .map_err(control_failure)?
        .contract()
        .state()
        .is_some_and(axocoatl_session::turn_contract::LogicalTurnState::is_closed)
        || controller.has_owned_execution().map_err(control_failure)?
    {
        return Ok(false);
    }
    // This checks finalized promotion, exact owner and actual effect settlement.
    // A closed row alone never releases an uncertain execution resource.
    registry.release_after_turn(session_id, turn_id)?;
    publish_control_disposition(&controller, bus, turn_id);
    Ok(true)
}
fn publish_control_disposition(
    controller: &SessionDispatchController,
    bus: &crate::stream::StreamBus,
    turn_id: &LogicalTurnId,
) {
    let history = match controller.history_snapshot() {
        Ok(history) => history,
        Err(error) => {
            tracing::warn!(turn=%turn_id.as_str(),%error,"could not read native control disposition");
            return;
        }
    };
    let Some(SessionHistoryEntry::ExecutionV2(turn)) = history.get(turn_id.as_str()) else {
        return;
    };
    let mut usage = MeasuredTokenUsage::known(TokenUsageStats::default());
    for activation in &turn.activations {
        match controller.activation_provider_usage(&activation.activation.activation) {
            Ok(measured) => {
                usage.usage.merge(&measured.tokens.usage);
                usage.complete &= measured.tokens.complete;
            }
            Err(_) => usage.complete = false,
        }
    }
    super::native_send::publish_native_disposition(bus, turn, &usage);
}
