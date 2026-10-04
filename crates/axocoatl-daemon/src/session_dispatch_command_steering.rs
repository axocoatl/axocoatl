//! Safe-boundary command queue over the existing durable command journal.
use super::*;
use axocoatl_session::control_command::SteerMode;

impl DispatchState {
    pub(in crate::session_dispatch) fn validate_steer(
        &self,
        view: &CommandReceiptView,
        activation: &ActivationRef,
        instruction: &EvidenceRef,
        mode: &SteerMode,
        instruction_preview: bool,
    ) -> Result<()> {
        if *mode != SteerMode::NextSafeBoundary {
            return Err(error("this host supports only next-safe-boundary guidance"));
        }
        let snapshot = self.current(activation)?;
        self.steer_owner(activation, now_ms()?)?;
        if !instruction_preview {
            let ActivationEvidenceContent::Guidance { text } = &self
                .content
                .resolve_activation_evidence(instruction)
                .map_err(error)?
            else {
                return Err(error("steering instruction is not retained guidance"));
            };
            if text.trim().is_empty() {
                return Err(error("steering instruction is empty"));
            }
        }
        let input = &snapshot
            .contract()
            .activations()
            .iter()
            .find(|item| item.activation == *activation)
            .unwrap()
            .input;
        let applied = snapshot
            .contract()
            .guidance()
            .iter()
            .filter(|item| item.activation == *activation)
            .count();
        let mut queued = 0usize;
        for id in self.control_ids()? {
            if id == view.request.command_id {
                continue;
            }
            if self.commands.receipt(&id).map_err(error)?.is_some_and(|receipt|
                receipt.view().state == ControlCommandState::Accepted && matches!(&receipt.view().request.parameters,
                    ControlParameters::SteerActivation { activation: target, .. } if target == activation)) { queued += 1; }
        }
        // Reuse the existing aggregate input-reference bound. Accepted commands
        // reserve their future reference; unrelated queue entries cannot steal it.
        if input.guidance.len() + input.parents.len() + input.attachments.len() + applied + queued
            >= MAX_INPUT_REFERENCES
        {
            return Err(error("activation guidance reference capacity is exhausted"));
        }
        let mut preview = snapshot.contract().clone();
        preview
            .apply(&TurnContractEnvelope {
                schema_version: TURN_CONTRACT_SCHEMA_VERSION,
                command_id: CommandId::new(format!(
                    "steer-preview-{:x}",
                    Sha256::digest(serde_json::to_vec(&view.request).map_err(error)?)
                ))
                .map_err(error)?,
                expected_revision: preview.revision(),
                session_id: view.request.session_id.clone(),
                turn_id: view.request.turn_id.clone(),
                event: TurnContractEvent::ApplyGuidance {
                    activation: activation.clone(),
                    control_command_id: view.request.command_id.clone(),
                    instruction: instruction.clone(),
                    request: self.command_request_evidence(view)?,
                },
            })
            .map_err(error)?;
        Ok(())
    }

    fn steer_owner(&self, activation: &ActivationRef, boundary_ms: u64) -> Result<()> {
        self.execution_admission()?;
        let bound = self
            .bound
            .get(&activation.activation_id)
            .filter(|bound| {
                bound.activation == *activation
                    && bound.steering_open
                    && !bound.control.is_cancelled()
            })
            .ok_or_else(|| error("activation has no open native safe-boundary steering owner"))?;
        // Human source does not override revoked/expired execution authority.
        // This checks the exact already registered lease without minting a grant.
        self.authority
            .attest_control_source(&bound.lease, boundary_ms)
            .map_err(error)?;
        Ok(())
    }

    pub(in crate::session_dispatch) fn next_steering_handoff(
        &mut self,
        activation: &ActivationRef,
        final_boundary: bool,
        boundary_ms: u64,
    ) -> Result<Option<(CommandId, String, Vec<axocoatl_core::AgentAttachment>)>> {
        self.ready()?;
        // Stop winning this race prevents delivery. Already incurred provider
        // usage and complete tool groups remain recorded by the actor.
        if self
            .bound
            .get(&activation.activation_id)
            .is_some_and(|bound| bound.activation == *activation && bound.control.is_cancelled())
        {
            return Ok(None);
        }
        self.current(activation)?;
        // Tool-only host bindings deliberately do not advertise steering.
        // Their unchanged DefaultAgentBehavior still polls this default hook.
        if self
            .bound
            .get(&activation.activation_id)
            .is_some_and(|bound| bound.activation == *activation && !bound.steering_open)
        {
            return Ok(None);
        }
        self.steer_owner(activation, boundary_ms)?;
        let mut next = None;
        for id in self.control_ids()? {
            let receipt = self
                .commands
                .receipt(&id)
                .map_err(error)?
                .ok_or_else(|| error("queued command disappeared"))?;
            let view = receipt.view();
            if !matches!(&view.request.parameters, ControlParameters::SteerActivation { activation: target, .. } if target == activation)
            {
                continue;
            }
            if view.state == ControlCommandState::Applied {
                return Err(error(
                    "previous guidance handoff lacks actor acknowledgement",
                ));
            }
            if view.state == ControlCommandState::Accepted && next.is_none() {
                next = Some(view.clone());
            }
        }
        let Some(view) = next else {
            if final_boundary {
                self.bound
                    .get_mut(&activation.activation_id)
                    .ok_or_else(|| error("steering owner disappeared"))?
                    .steering_open = false;
            }
            return Ok(None);
        };
        let ControlParameters::SteerActivation {
            instruction,
            mode: SteerMode::NextSafeBoundary,
            ..
        } = &view.request.parameters
        else {
            return Err(error(
                "unsupported guidance entered the safe-boundary queue",
            ));
        };
        if view.request.execution_epoch_id != activation.execution_epoch_id {
            return Err(error("guidance belongs to a lost epoch"));
        }
        let ActivationEvidenceContent::Guidance { text } = &self
            .content
            .resolve_activation_evidence(instruction)
            .map_err(error)?
        else {
            return Err(error("queued guidance body is unavailable"));
        };
        let _ = text;
        let (text, attachments) =
            crate::session_dispatch::human_context::delivery(&self.content, &view, instruction)?;
        let envelope = self.control_envelope(
            &view,
            TurnContractEvent::ApplyGuidance {
                activation: activation.clone(),
                control_command_id: view.request.command_id.clone(),
                instruction: instruction.clone(),
                request: self.command_request_evidence(&view)?,
            },
        )?;
        self.canonical.append(envelope.clone()).map_err(error)?;
        self.command_update(
            &view,
            ControlTransition::Applied {
                state_transition: self.canonical_control_evidence(&envelope)?,
                turn_revision: envelope.expected_revision + 1,
                graph_revision: self
                    .canonical
                    .snapshot(&self.turn_id)
                    .map_err(error)?
                    .contract()
                    .graph()
                    .unwrap()
                    .revision,
                pending: self.command_request_evidence(&view)?,
            },
        )?;
        Ok(Some((view.request.command_id, text, attachments)))
    }

    pub(in crate::session_dispatch) fn acknowledge_steering_handoff(
        &mut self,
        activation: &ActivationRef,
        id: &CommandId,
    ) -> Result<()> {
        self.ready()?;
        let receipt = self
            .commands
            .receipt(id)
            .map_err(error)?
            .ok_or_else(|| error("guidance receipt disappeared"))?;
        let view = receipt.view();
        let ControlParameters::SteerActivation {
            activation: target,
            instruction,
            mode: SteerMode::NextSafeBoundary,
        } = &view.request.parameters
        else {
            return Err(error("actor acknowledgement has another command kind"));
        };
        if target != activation || view.state != ControlCommandState::Applied {
            return Err(error(
                "actor acknowledgement has no exact pending guidance handoff",
            ));
        }
        let envelope = self.control_envelope(
            view,
            TurnContractEvent::ApplyGuidance {
                activation: activation.clone(),
                control_command_id: id.clone(),
                instruction: instruction.clone(),
                request: self.command_request_evidence(view)?,
            },
        )?;
        if !self
            .canonical
            .command_record(&envelope.command_id)
            .map_err(error)?
            .is_some_and(|(_, record)| record == envelope)
        {
            return Err(error(
                "guidance acknowledgement has no exact durable canonical handoff",
            ));
        }
        // The opaque acknowledgement is called only after the synchronous actor
        // append. It may record that input after Stop; it grants no next call.
        self.settle_control_event(view, &envelope)
    }

    pub(in crate::session_dispatch) fn reconcile_steer(
        &mut self,
        view: &CommandReceiptView,
    ) -> Result<()> {
        let ControlParameters::SteerActivation { activation, .. } = &view.request.parameters else {
            return Err(error("not a steering command"));
        };
        let live = self.current(activation).is_ok()
            && self
                .bound
                .get(&activation.activation_id)
                .is_some_and(|bound| {
                    bound.activation == *activation
                        && bound.steering_open
                        && (!bound.control.is_cancelled()
                            || view.state == ControlCommandState::Applied)
                });
        // An exact handoff already held by a live actor may acknowledge its
        // synchronous append after Stop. Termination/lost epoch still fails an
        // unacknowledged handoff; cancellation never grants another model call.
        if live {
            return Ok(());
        }
        let applied = self
            .canonical
            .snapshot(&self.turn_id)
            .map_err(error)?
            .contract()
            .guidance()
            .iter()
            .any(|item| item.control_command_id == view.request.command_id);
        self.command_update(view, ControlTransition::Failed { failure: command_failure(
            if applied { "steer_delivery_unknown" } else { "steer_not_delivered" },
            if applied { "guidance handoff lost its actor acknowledgement; delivery is unknown and was not replayed" }
            else { "activation ended or lost ownership before the guidance boundary; instruction was not delivered" },
            Some(self.command_request_evidence(view)?),
        ) })?;
        Ok(())
    }
}
