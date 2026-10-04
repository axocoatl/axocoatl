//! Resume a genuine native human wait. A durable response is not a recreated
//! hook, an approval for another invocation, or permission for a provider call.
use super::*;
use axocoatl_session::control_command::BlockerResponse;

impl DispatchState {
    pub(in crate::session_dispatch) fn validate_resume(
        &self,
        view: &CommandReceiptView,
        preview: bool,
    ) -> Result<()> {
        let ControlParameters::ResumeBlocked {
            activation,
            blocker_id,
            response,
        } = &view.request.parameters
        else {
            return Err(error("not a Resume request"));
        };
        if !matches!(view.source, CommandSourceRecord::Human { .. }) {
            return Err(error(
                "this blocker requires an authenticated human response",
            ));
        }
        let id = BlockerId::new(blocker_id.as_str()).map_err(error)?;
        let snapshot = self.current(activation)?;
        let wait = self
            .human_waits
            .get(&id)
            .filter(|wait| wait.activation == *activation)
            .ok_or_else(|| {
                error("the exact human wait has no live owner; a lost process cannot Resume")
            })?;
        let item = snapshot
            .contract()
            .blockers()
            .iter()
            .find(|item| item.blocker.blocker_id == id)
            .ok_or_else(|| error("typed human blocker is absent"))?;
        if item.blocker.activation != *activation
            || item.state != TurnBlockerState::Pending
            || item.blocker.parameters != wait.parameters
            || !matches!(item.blocker.kind, TurnBlockerKind::HumanApproval { .. })
        {
            return Err(error(
                "the exact typed blocker is no longer waiting for this response",
            ));
        }
        let bound = self
            .bound
            .get(&activation.activation_id)
            .filter(|bound| bound.activation == *activation && !bound.control.is_cancelled())
            .ok_or_else(|| error("human wait lost its exact actor control"))?;
        let reference = match response {
            BlockerResponse::Approval { approval } => {
                self.authority
                    .attest_control_source(&bound.lease, now_ms()?)
                    .map_err(error)?;
                approval
            }
            BlockerResponse::Decline { reason } => reason,
            BlockerResponse::Evidence { .. } => {
                return Err(error("machine evidence cannot answer a human approval"))
            }
        };
        if !preview {
            self.validate_human_response_evidence(view, reference)?;
        }
        let envelope = if preview {
            TurnContractEnvelope {
                schema_version: TURN_CONTRACT_SCHEMA_VERSION,
                command_id: CommandId::new(format!(
                    "preview-{:x}",
                    Sha256::digest(serde_json::to_vec(&view.request).map_err(error)?)
                ))
                .map_err(error)?,
                expected_revision: snapshot.contract().revision(),
                session_id: activation.session_id.clone(),
                turn_id: activation.turn_id.clone(),
                event: self.resume_event(view)?,
            }
        } else {
            self.resume_envelope(view)?
        };
        let mut preview = snapshot.contract().clone();
        preview.apply(&envelope).map_err(error)?;
        Ok(())
    }

    fn resume_event(&self, view: &CommandReceiptView) -> Result<TurnContractEvent> {
        let ControlParameters::ResumeBlocked {
            activation,
            blocker_id,
            response,
        } = &view.request.parameters
        else {
            return Err(error("not a Resume request"));
        };
        let id = BlockerId::new(blocker_id.as_str()).map_err(error)?;
        let snapshot = self.canonical.snapshot(&self.turn_id).map_err(error)?;
        let item = snapshot
            .contract()
            .blockers()
            .iter()
            .find(|item| item.blocker.blocker_id == id && item.blocker.activation == *activation)
            .ok_or_else(|| error("Resume target disappeared"))?;
        let TurnBlockerKind::HumanApproval { approval_request } = &item.blocker.kind else {
            return Err(error("this host cannot resolve machine blockers"));
        };
        let response = match response {
            BlockerResponse::Approval { approval } => TurnBlockerResponse::HumanApproval {
                approval_request: approval_request.clone(),
                approval_evidence: approval.clone(),
            },
            BlockerResponse::Decline { reason } => TurnBlockerResponse::HumanDecline {
                approval_request: approval_request.clone(),
                reason: reason.clone(),
            },
            BlockerResponse::Evidence { .. } => {
                return Err(error(
                    "human approval cannot be resolved by machine evidence",
                ))
            }
        };
        Ok(TurnContractEvent::ResolveBlocker {
            blocker_id: id,
            activation: activation.clone(),
            response,
        })
    }
    fn resume_envelope(&self, view: &CommandReceiptView) -> Result<TurnContractEnvelope> {
        self.control_envelope(view, self.resume_event(view)?)
    }

    fn validate_human_response_evidence(
        &self,
        view: &CommandReceiptView,
        reference: &EvidenceRef,
    ) -> Result<()> {
        let CommandSourceRecord::Human {
            request_evidence, ..
        } = &view.source
        else {
            return Err(error("human response has no authenticated human source"));
        };
        if reference != request_evidence {
            return Err(error(
                "human response must bind its exact authenticated request body",
            ));
        }
        let ActivationEvidenceContent::Guidance { text } = &self
            .content
            .resolve_activation_evidence(reference)
            .map_err(error)?
        else {
            return Err(error("human response evidence has the wrong role"));
        };
        let request = HumanControlActionRequest::decode(text.as_bytes())?;
        let ControlParameters::ResumeBlocked {
            activation,
            blocker_id,
            response,
        } = &view.request.parameters
        else {
            return Err(error("not a Resume request"));
        };
        if request.action != HumanControlAction::Resume
            || request.command_id != view.request.command_id
            || request.session_id != view.request.session_id
            || request.turn_id != view.request.turn_id
            || request.execution_epoch_id != view.request.execution_epoch_id
            || request.expected_turn_revision != view.request.expected_turn_revision
            || request.expected_graph_revision != view.request.expected_graph_revision
            || request.activation.as_ref() != Some(activation)
            || request.blocker_id.as_ref().map(BlockerId::as_str) != Some(blocker_id.as_str())
            || !matches!(
                (&request.human_response, response),
                (
                    Some(HumanBlockerResponse::Approval),
                    BlockerResponse::Approval { .. }
                ) | (
                    Some(HumanBlockerResponse::Decline { .. }),
                    BlockerResponse::Decline { .. }
                )
            )
        {
            return Err(error(
                "human response content differs from its exact authenticated operation",
            ));
        }
        Ok(())
    }

    pub(in crate::session_dispatch) fn apply_resume(
        &mut self,
        view: &CommandReceiptView,
    ) -> Result<()> {
        let envelope = self.resume_envelope(view)?;
        self.canonical.append(envelope.clone()).map_err(error)?;
        self.command_update(
            view,
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
                pending: self.command_request_evidence(view)?,
            },
        )?;
        if let ControlParameters::ResumeBlocked {
            activation,
            blocker_id,
            ..
        } = &view.request.parameters
        {
            self.publish_human_wait_changed(
                activation,
                &BlockerId::new(blocker_id.as_str()).map_err(error)?,
            )?;
        }
        Ok(())
    }

    pub(in crate::session_dispatch) fn acknowledge_human_response(
        &mut self,
        blocker: &BlockerId,
    ) -> Result<()> {
        let mut matched = None;
        for id in self.control_ids()? {
            let receipt = self
                .commands
                .receipt(&id)
                .map_err(error)?
                .ok_or_else(|| error("human response command receipt disappeared"))?;
            let view = receipt.view();
            if view.state == ControlCommandState::Applied
                && matches!(&view.request.parameters,
                ControlParameters::ResumeBlocked { blocker_id, .. } if blocker_id.as_str() == blocker.as_str())
                && matched.replace(view.clone()).is_some()
            {
                return Err(error(
                    "human response has ambiguous pending command receipts",
                ));
            }
        }
        let view =
            matched.ok_or_else(|| error("human response has no exact pending command receipt"))?;
        let envelope = self.resume_envelope(&view)?;
        if !self
            .canonical
            .command_record(&envelope.command_id)
            .map_err(error)?
            .is_some_and(|(_, record)| record == envelope)
        {
            return Err(error("human response has no durable canonical predecessor"));
        }
        self.settle_control_event(&view, &envelope)
    }

    pub(in crate::session_dispatch) fn reconcile_resume(
        &mut self,
        view: &CommandReceiptView,
    ) -> Result<()> {
        let ControlParameters::ResumeBlocked {
            activation,
            blocker_id,
            ..
        } = &view.request.parameters
        else {
            return Err(error("not a Resume request"));
        };
        let id = BlockerId::new(blocker_id.as_str()).map_err(error)?;
        if self.current(activation).is_ok()
            && self
                .human_waits
                .get(&id)
                .is_some_and(|wait| wait.activation == *activation)
        {
            return Ok(());
        }
        self.command_update(view, ControlTransition::Failed { failure: command_failure("human_response_not_delivered",
            "the exact human wait ended or lost ownership before acknowledging this response; no work was replayed", Some(self.command_request_evidence(view)?)) })?;
        Ok(())
    }

    pub(in crate::session_dispatch) fn human_decline_reason(
        &self,
        evidence: &EvidenceRef,
    ) -> Result<String> {
        let ActivationEvidenceContent::Guidance { text } = &self
            .content
            .resolve_activation_evidence(evidence)
            .map_err(error)?
        else {
            return Err(error("human denial evidence has the wrong role"));
        };
        let request: HumanControlActionRequest = serde_json::from_str(text).map_err(error)?;
        match request.human_response {
            Some(HumanBlockerResponse::Decline { reason }) => Ok(format!(
                "Human declined this exact tool invocation: {reason}"
            )),
            _ => Err(error("denial evidence does not record a human decline")),
        }
    }
}
