//! One-use actor acknowledgement over an exact retained command/activation.
use super::*;
use axocoatl_actor::{SteeringAcknowledgement, SteeringDelivery};

struct SteeringHandoff {
    controller: SessionDispatchController,
    activation: ActivationRef,
    command_id: CommandId,
}

impl SteeringAcknowledgement for SteeringHandoff {
    fn acknowledge(self: Box<Self>) -> std::result::Result<(), String> {
        let mut state = self.controller.lock().map_err(|error| error.to_string())?;
        let result = state.acknowledge_steering_handoff(&self.activation, &self.command_id);
        state.fail_closed(result).map_err(|error| error.to_string())
    }
}

impl SessionDispatchController {
    pub(super) fn take_activation_guidance(
        &self,
        activation: &ActivationRef,
        final_boundary: bool,
    ) -> Result<Option<SteeringDelivery>> {
        let mut state = self.lock()?;
        let boundary_ms = now_ms()?;
        // Polling the next safe boundary is also an execution-authority
        // boundary. Revocation may win while an already claimed provider call
        // returns. That definitive refusal cancels this actor; it is not an
        // uncertain journal write and must not poison its retained history.
        if let Some(bound) = state.bound.get(&activation.activation_id).filter(|bound| {
            bound.activation == *activation && bound.steering_open && !bound.control.is_cancelled()
        }) {
            match state
                .authority
                .attest_control_source(&bound.lease, boundary_ms)
            {
                Ok(_) => {}
                Err(
                    axocoatl_session::control_authority::AuthorityError::Denied
                    | axocoatl_session::control_authority::AuthorityError::StaleLease,
                ) => {
                    bound.control.cancel();
                    state.changed.notify_waiters();
                    return Ok(None);
                }
                Err(failure) => return state.fail_closed(Err(error(failure))),
            }
        }
        let result = state.next_steering_handoff(activation, final_boundary, boundary_ms);
        let result = state.fail_closed(result)?;
        Ok(
            result.map(|(command_id, text, attachments)| SteeringDelivery {
                text,
                attachments,
                acknowledgement: Box::new(SteeringHandoff {
                    controller: self.clone(),
                    activation: activation.clone(),
                    command_id,
                }),
            }),
        )
    }
}
