//! Host-authenticated durable controls over the existing execution journals.
//!
//! This is not an RPC authentication surface. Accepted means the exact request
//! passed current validation, Applied names a durable canonical event, and
//! Settled names its allocation or safe-boundary result. Neither acknowledgement
//! dispatches an actor. Recovery repairs evidence and never replays execution.

use super::*;
#[path = "session_dispatch_command_resume.rs"]
mod resume;
#[path = "session_dispatch_command_steering.rs"]
mod steering;

use axocoatl_session::control_command::{
    CommandFailure, CommandReceiptView, CommandSourceRecord, ControlCommandEvent,
    ControlCommandRequest, ControlCommandState, ControlParameters, ControlReceiptUpdate,
    ControlTransition, DurableCommandReceipt, FinishMode, TrustedCommandSource,
};

impl SessionDispatchController {
    /// Call only through an authenticated host channel with exact Session access.
    /// Agent source values must come from the live authority, not public JSON.
    pub fn submit_control_command(
        &self,
        request: ControlCommandRequest,
        source: TrustedCommandSource,
    ) -> Result<DurableCommandReceipt> {
        let mut state = self.lock()?;
        state.ready()?;
        state.submit_control_command(request, source)
    }

    pub fn control_command_receipt(
        &self,
        command_id: &CommandId,
    ) -> Result<Option<DurableCommandReceipt>> {
        let state = self.lock()?;
        state.ready()?;
        state.commands.receipt(command_id).map_err(error)
    }

    /// Called after actor settlement and on reconstruction. It may finish an
    /// already accepted close request, but never starts a provider or tool.
    pub fn reconcile_control_commands(&self) -> Result<()> {
        let mut state = self.lock()?;
        state.ready()?;
        let result = state.reconcile_control_commands();
        state.fail_closed(result)
    }

    /// Resolve evidence identifiers emitted by this adapter to actual retained
    /// request/canonical records. Validation bodies use the content store's
    /// ordinary immutable evidence references instead.
    pub fn control_command_evidence(
        &self,
        reference: &EvidenceRef,
    ) -> Result<Option<serde_json::Value>> {
        let state = self.lock()?;
        state.ready()?;
        for record in state.commands.records().map_err(error)? {
            if let ControlCommandEvent::Requested { request, source } = &record.event {
                let value = serde_json::json!({"request": request, "source": source});
                if state.control_evidence("request", &value)? == *reference {
                    return Ok(Some(value));
                }
            }
        }
        // Control evidence belongs to this controller's turn.
        for event in state
            .canonical
            .turn_records(&state.turn_id)
            .map_err(error)?
        {
            if state.canonical_control_evidence(&event)? == *reference {
                return serde_json::to_value(&event).map(Some).map_err(error);
            }
        }
        Ok(None)
    }
}

impl DispatchState {
    pub(super) fn submit_control_command(
        &mut self,
        request: ControlCommandRequest,
        source: TrustedCommandSource,
    ) -> Result<DurableCommandReceipt> {
        self.command_owner(&request, source.record())?;
        // A current permission/revision check must not turn an exact repeat into
        // another operation. Attribution is retained from its original request.
        if let Some(receipt) = self.commands.lookup_request(&request).map_err(error)? {
            return Ok(receipt);
        }
        let result = self
            .commands
            .record_requested(request, source)
            .map_err(error);
        let requested = self.fail_closed(result)?;
        let view = requested.view().clone();
        #[cfg(test)]
        self.crash_agent_command(&view, super::TestFailure::AgentCommandRequested)?;
        if let Err(reason) = self.validate_control_projection(&view, false) {
            return self.command_update(
                &view,
                ControlTransition::Rejected {
                    failure: command_failure("control_rejected", reason, None),
                },
            );
        }
        // Retain the actual validated predecessor and full request attribution.
        // This immutable body is evidence; no reader can turn it into a lease.
        let pending = self.command_request_evidence(&view)?;
        let snapshot = self.canonical.snapshot(&self.turn_id).map_err(error)?;
        let validation = serde_json::json!({
            "kind": "control-validation-v1",
            "request": pending,
            "canonical_journal": snapshot.journal_id(),
            "turn_revision": snapshot.contract().revision(),
            "graph_revision": snapshot.contract().graph().unwrap().revision,
            "source": view.source,
            "parameters": view.request.parameters,
        });
        let result = self
            .content
            .retain_activation_evidence(ActivationEvidenceContent::Guidance {
                text: validation.to_string(),
            })
            .map(|receipt| receipt.reference().clone())
            .map_err(error);
        let validation = self.fail_closed(result)?;
        let accepted = self.command_update(
            &view,
            ControlTransition::Accepted {
                validation,
                pending,
            },
        )?;
        self.changed.notify_waiters();
        #[cfg(test)]
        self.crash_agent_command(&view, super::TestFailure::AgentCommandAccepted)?;
        let result = self.apply_control(accepted.view());
        self.fail_closed(result)?;
        self.commands
            .receipt(&view.request.command_id)
            .map_err(error)?
            .ok_or_else(|| error("accepted control receipt disappeared"))
    }
}

impl DispatchState {
    fn command_owner(
        &self,
        request: &ControlCommandRequest,
        source: &CommandSourceRecord,
    ) -> Result<()> {
        let owner = self.canonical.owner();
        if request.session_id != owner.session_id || request.turn_id != self.turn_id {
            return Err(error("control belongs to another Session or turn"));
        }
        let (session, turn) = match source {
            CommandSourceRecord::Human {
                session_id,
                turn_id,
                ..
            } => (session_id, turn_id),
            CommandSourceRecord::Agent { activation, .. } => {
                (&activation.session_id, &activation.turn_id)
            }
        };
        if session != &owner.session_id || turn != &self.turn_id {
            return Err(error("control source belongs to another Session or turn"));
        }
        Ok(())
    }

    pub(super) fn control_stage_id(
        &self,
        view: &CommandReceiptView,
        stage: &str,
    ) -> Result<CommandId> {
        let receipt = self
            .commands
            .receipt(&view.request.command_id)
            .map_err(error)?
            .ok_or_else(|| error("control request is not durable"))?;
        let bytes = serde_json::to_vec(&(receipt.journal_id(), &view.request.command_id, stage))
            .map_err(error)?;
        CommandId::new(format!("control-{:x}", Sha256::digest(bytes))).map_err(error)
    }

    fn control_evidence(&self, kind: &str, value: &impl serde::Serialize) -> Result<EvidenceRef> {
        let identity = self.canonical.identity().map_err(error)?;
        let bytes =
            serde_json::to_vec(&("control-evidence-v1", identity.journal_id(), kind, value))
                .map_err(error)?;
        EvidenceRef::new(format!("control-{kind}-{:x}", Sha256::digest(bytes))).map_err(error)
    }

    pub(super) fn command_request_evidence(
        &self,
        view: &CommandReceiptView,
    ) -> Result<EvidenceRef> {
        self.control_evidence(
            "request",
            &serde_json::json!({"request": view.request, "source": view.source}),
        )
    }

    fn canonical_control_evidence(&self, event: &TurnContractEnvelope) -> Result<EvidenceRef> {
        self.control_evidence("canonical", event)
    }

    pub(super) fn command_update(
        &mut self,
        view: &CommandReceiptView,
        transition: ControlTransition,
    ) -> Result<DurableCommandReceipt> {
        let stage = match &transition {
            ControlTransition::Rejected { .. } => "rejected",
            ControlTransition::Accepted { .. } => "accepted",
            ControlTransition::Applied { .. } => "applied",
            ControlTransition::Settled { .. } => "settled",
            ControlTransition::Failed { .. } => "failed",
        };
        let update = ControlReceiptUpdate {
            update_id: self.control_stage_id(view, stage)?,
            command_id: view.request.command_id.clone(),
            session_id: view.request.session_id.clone(),
            turn_id: view.request.turn_id.clone(),
            execution_epoch_id: view.request.execution_epoch_id.clone(),
            expected_receipt_revision: view.revision,
            transition,
        };
        let result = self.commands.advance(update).map_err(error);
        let receipt = self.fail_closed(result)?;
        self.changed.notify_waiters();
        Ok(receipt)
    }

    pub(super) fn control_envelope(
        &self,
        view: &CommandReceiptView,
        event: TurnContractEvent,
    ) -> Result<TurnContractEnvelope> {
        let command_id = self.control_stage_id(view, "canonical")?;
        if let Some((_, existing)) = self.canonical.command_record(&command_id).map_err(error)? {
            if existing.event != event
                || existing.turn_id != view.request.turn_id
                || existing.session_id != view.request.session_id
            {
                return Err(error("control canonical identity has conflicting content"));
            }
            return Ok(existing);
        }
        let snapshot = self.canonical.snapshot(&self.turn_id).map_err(error)?;
        Ok(TurnContractEnvelope {
            schema_version: TURN_CONTRACT_SCHEMA_VERSION,
            command_id,
            expected_revision: snapshot.contract().revision(),
            session_id: view.request.session_id.clone(),
            turn_id: view.request.turn_id.clone(),
            event,
        })
    }

    fn control_event(view: &CommandReceiptView) -> Option<TurnContractEvent> {
        match &view.request.parameters {
            ControlParameters::RetryActivation { input, .. } => {
                Some(TurnContractEvent::StartActivation {
                    input: input.clone(),
                })
            }
            ControlParameters::ReviseActivation {
                activation,
                input,
                instruction,
                invalidate,
            } => Some(TurnContractEvent::ReviseAccepted {
                previous: activation.clone(),
                input: input.clone(),
                invalidated_descendants: invalidate.clone(),
                evidence: instruction.clone(),
            }),
            ControlParameters::ContinueTurn { plan, .. } => {
                Some(TurnContractEvent::Continue { plan: plan.clone() })
            }
            ControlParameters::FinishTurn {
                mode:
                    FinishMode::ForcePartial {
                        approval,
                        selected_activations,
                        stop_activations,
                        missing_conditions,
                        missing_condition_ids,
                    },
            } => Some(TurnContractEvent::RequestPartialFinish {
                approval: approval.clone(),
                selected_activations: selected_activations.clone(),
                stop_activations: stop_activations.clone(),
                missing_conditions: missing_conditions.clone(),
                missing_condition_ids: missing_condition_ids.clone(),
            }),
            ControlParameters::FinishTurn {
                mode: FinishMode::Normal,
            } => Some(TurnContractEvent::Close {
                closure: TurnClosure::Completed,
            }),
            _ => None,
        }
    }

    fn validate_control_source(&self, view: &CommandReceiptView) -> Result<()> {
        if let CommandSourceRecord::Agent { activation, .. } = &view.source {
            let bound = self
                .bound
                .get(&activation.activation_id)
                .filter(|bound| bound.activation == *activation)
                .ok_or_else(|| error("control source has no exact live execution owner"))?;
            let fresh = self
                .authority
                .attest_control_source(&bound.lease, now_ms()?)
                .map_err(error)?;
            if fresh.record() != &view.source {
                return Err(error("control source attestation is stale or foreign"));
            }
            let grant = self
                .authority
                .grant_policy(bound.grant.grant_id.as_str())
                .map_err(error)?;
            if activation.node_id != grant.holder {
                return Err(error("only the exact grant holder can delegate control"));
            }
            if grant.delegation.is_some() {
                return self.validate_delegated_control_scope(view, &grant);
            }
            let ControlParameters::StopActivation { activation: target } = &view.request.parameters
            else {
                return Err(error(
                    "this grant does not authorize the requested operation",
                ));
            };
            if activation.node_id != grant.holder
                || !grant.permits_operation(
                    axocoatl_session::turn_contract::DelegatedOperation::StopActivation,
                    &target.node_id,
                )
                || activation.node_id == target.node_id
            {
                return Err(error("source cannot stop this exact target"));
            }
        }
        Ok(())
    }

    pub(super) fn validate_control_input(
        &self,
        snapshot: &DurableTurnSnapshot,
        input: &ActivationInputManifest,
    ) -> Result<()> {
        let resolved = self
            .content
            .validate_proposed_input(snapshot, input)
            .map_err(error)?;
        let ActivationEvidenceContent::Definition {
            profile,
            configuration,
            ..
        } = &resolved.definition
        else {
            return Err(error("control input definition has the wrong role"));
        };
        let config: axocoatl_core::AgentConfig =
            serde_json::from_str(configuration).map_err(error)?;
        // A helper or the required reviewer runs its Worker template in a
        // conversation of its own.
        let native_child = self.owns_instance_conversation(&input.activation.node_id)?;
        if !(matches!(
            config.role,
            axocoatl_core::AgentRole::Autonomous | axocoatl_core::AgentRole::Coordinator
        ) || (native_child && config.role == axocoatl_core::AgentRole::Worker))
            || (!native_child && config.id.0 != input.conversation_id.as_str())
            || config.provider != profile.provider
            || config.model != profile.model
            || config.tools != profile.tools
            || config.writes != profile.write_scope
            || profile.isolation != "in-process"
        {
            return Err(error(
                "control input is not supported by the autonomous host port",
            ));
        }
        let grant = input
            .grant
            .as_ref()
            .ok_or_else(|| error("control input has no grant"))?;
        let policy = self
            .authority
            .grant_policy(grant.grant_id.as_str())
            .map_err(error)?;
        if !self.captured_grant_is_current(resolved.grant.as_ref(), &resolved.budget, &policy)? {
            return Err(error("control input differs from current grant or budget"));
        }
        self.authority
            .validate_activation_grant(
                &input.activation,
                grant.grant_id.as_str(),
                profile,
                now_ms()?,
            )
            .map_err(error)?;
        if let ConversationSavepoint::Checkpoint { checkpoint } = &input.starting_savepoint {
            self.memory.checkpoint(checkpoint).map_err(error)?;
        }
        for parent in &input.parents {
            self.memory.checkpoint(&parent.checkpoint).map_err(error)?;
        }
        let (_, request) = self
            .content
            .retained_request(&self.turn_id)
            .map_err(error)?
            .ok_or_else(|| error("canonical request body is unavailable"))?;
        let request_ref = snapshot
            .request_ref()
            .ok_or_else(|| error("canonical request is not bound"))?;
        match &input.repository {
            RepositoryInput::Unavailable => {
                super::input::project_text_input(input, request_ref, request, &resolved)?;
            }
            RepositoryInput::Recorded {
                snapshot: repository_ref,
            } => {
                let repository = self.repository_activation_resource(repository_ref)?;
                super::input::project_repository_input(
                    input,
                    request_ref,
                    request,
                    &resolved,
                    &repository,
                )?;
            }
        }
        Ok(())
    }

    pub(super) fn validate_control(&self, view: &CommandReceiptView) -> Result<()> {
        self.validate_control_projection(view, false)
    }

    /// Read-only capability assessment. The future instruction has no retained
    /// body yet; every actual submission still requires the complete validator.
    pub(super) fn preview_human_control(&self, view: &CommandReceiptView) -> Result<()> {
        if !matches!(view.source, CommandSourceRecord::Human { .. }) {
            return Err(error(
                "capability preview requires authenticated human attribution",
            ));
        }
        self.validate_control_projection(view, true)
    }

    fn validate_control_projection(
        &self,
        view: &CommandReceiptView,
        instruction_preview: bool,
    ) -> Result<()> {
        let snapshot = self.canonical.snapshot(&self.turn_id).map_err(error)?;
        let contract = snapshot.contract();
        if contract.stop_requested().is_some()
            || contract.revision() != view.request.expected_turn_revision
            || contract
                .graph()
                .is_none_or(|graph| graph.revision != view.request.expected_graph_revision)
            || contract
                .epochs()
                .last()
                .is_none_or(|epoch| epoch.id != view.request.execution_epoch_id)
            || contract.state().is_none_or(LogicalTurnState::is_closed)
        {
            return Err(error("control targets stale or closed canonical state"));
        }
        self.validate_control_source(view)?;
        match &view.request.parameters {
            ControlParameters::AddAgent { .. } | ControlParameters::ReplaceFutureAgent { .. } => {
                return self.validate_graph_control(view)
            }
            ControlParameters::ResumeBlocked { .. } => {
                return self.validate_resume(view, instruction_preview)
            }
            ControlParameters::StopActivation { activation } => {
                self.current(activation)?;
                match self.bound.get(&activation.activation_id) {
                    Some(bound)
                        if bound.activation != *activation || bound.control.is_cancelled() =>
                    {
                        return Err(error(
                            "Stop targets an obsolete or already stopping activation",
                        ))
                    }
                    _ => {}
                }
                return Ok(());
            }
            ControlParameters::SteerActivation {
                activation,
                instruction,
                mode,
            } => {
                return self.validate_steer(
                    view,
                    activation,
                    instruction,
                    mode,
                    instruction_preview,
                );
            }
            ControlParameters::RetryActivation {
                activation,
                input,
                replay_decisions,
            } => {
                let previous = contract
                    .activations()
                    .iter()
                    .rev()
                    .find(|item| item.activation.node_id == activation.node_id)
                    .ok_or_else(|| error("Retry target has no canonical activation"))?;
                if previous.activation != *activation
                    || !matches!(
                        previous.state,
                        ActivationState::Failed | ActivationState::Interrupted
                    )
                {
                    return Err(error(
                        "Retry requires the exact latest failed or interrupted activation",
                    ));
                }
                if !replay_decisions.is_empty() {
                    return Err(error(
                        "invocation replay decisions require the reconciliation authority",
                    ));
                }
                self.validate_control_input(&snapshot, input)?;
            }
            ControlParameters::ReviseActivation {
                input, instruction, ..
            } => {
                if !input.guidance.contains(instruction) {
                    return Err(error(
                        "revision instruction must be captured in the new input",
                    ));
                }
                self.validate_control_input(&snapshot, input)?;
            }
            ControlParameters::ContinueTurn {
                plan,
                replay_decisions,
            } => {
                if !replay_decisions.is_empty() {
                    return Err(error(
                        "invocation replay decisions require the reconciliation authority",
                    ));
                }
                for selection in &plan.selections {
                    match selection {
                        ContinuationSelection::Retry { input, .. }
                        | ContinuationSelection::Rebase { input, .. }
                        | ContinuationSelection::Revise { input, .. }
                        | ContinuationSelection::PrepareUnmaterialized { input } => {
                            self.validate_control_input(&snapshot, input)?
                        }
                        ContinuationSelection::RetainAccepted { activation } => {
                            let current = contract
                                .activations()
                                .iter()
                                .find(|item| item.activation == *activation)
                                .ok_or_else(|| error("retained activation is absent"))?;
                            self.content
                                .validate_input(&snapshot, &current.input)
                                .map_err(error)?;
                            self.memory
                                .checkpoint(current.checkpoint.as_ref().ok_or_else(|| {
                                    error("retained activation has no accepted checkpoint")
                                })?)
                                .map_err(error)?;
                        }
                        _ => {}
                    }
                }
            }
            ControlParameters::FinishTurn {
                mode: FinishMode::ForcePartial { .. },
            } => {
                self.validate_partial_finish_control(view, instruction_preview)?;
            }
            ControlParameters::FinishTurn {
                mode: FinishMode::Normal,
            } => {
                if self.other_pending_control(&view.request.command_id)? {
                    return Err(error("other pending controls prevent normal Finish"));
                }
                // Human Finish can wait for running actors. The final close is
                // revalidated against their actual canonical settlement later.
                if contract
                    .activations()
                    .iter()
                    .any(|item| item.state == ActivationState::Running)
                {
                    return Ok(());
                }
            }
        }
        let event =
            Self::control_event(view).ok_or_else(|| error("control has no canonical operation"))?;
        let mut preview = contract.clone();
        // A read-only capability check has no command receipt. The preview uses
        // a separate fixed-length identity namespace; actual application still
        // derives its id from the durable command journal in control_envelope.
        let preview_id = CommandId::new(format!(
            "preview-{:x}",
            Sha256::digest(serde_json::to_vec(&view.request).map_err(error)?)
        ))
        .map_err(error)?;
        let envelope = TurnContractEnvelope {
            schema_version: TURN_CONTRACT_SCHEMA_VERSION,
            command_id: preview_id,
            expected_revision: contract.revision(),
            session_id: view.request.session_id.clone(),
            turn_id: view.request.turn_id.clone(),
            event,
        };
        preview.apply(&envelope).map_err(error)?;
        Ok(())
    }

    pub(super) fn other_pending_control(&self, except: &CommandId) -> Result<bool> {
        for id in self.control_ids()? {
            if &id != except
                && self
                    .commands
                    .receipt(&id)
                    .map_err(error)?
                    .is_some_and(|receipt| !receipt.view().state.is_terminal())
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn control_ids(&self) -> Result<Vec<CommandId>> {
        Ok(self
            .commands
            .records()
            .map_err(error)?
            .iter()
            .filter_map(|record| match &record.event {
                ControlCommandEvent::Requested { request, .. } => Some(request.command_id.clone()),
                _ => None,
            })
            .collect())
    }

    pub(super) fn applied_control(
        &self,
        view: &CommandReceiptView,
    ) -> Result<Option<TurnContractEnvelope>> {
        let event = match &view.request.parameters {
            ControlParameters::StopActivation { activation } => TurnContractEvent::FailActivation {
                activation: activation.clone(),
                evidence: self.command_request_evidence(view)?,
            },
            _ if self.graph_control_input(view).is_some() => self
                .graph_control_event(view)?
                .ok_or_else(|| error("missing graph control operation"))?,
            _ => match Self::control_event(view) {
                Some(event) => event,
                None => return Ok(None),
            },
        };
        let id = self.control_stage_id(view, "canonical")?;
        let found = self
            .canonical
            .command_record(&id)
            .map_err(error)?
            .map(|(_, record)| record);
        match found {
            Some(record)
                if record.event == event
                    && record.session_id == view.request.session_id
                    && record.turn_id == view.request.turn_id =>
            {
                Ok(Some(record))
            }
            Some(_) => Err(error(
                "retained control operation conflicts with its request",
            )),
            None => Ok(None),
        }
    }

    pub(super) fn settle_control_event(
        &mut self,
        view: &CommandReceiptView,
        event: &TurnContractEnvelope,
    ) -> Result<()> {
        let evidence = self.canonical_control_evidence(event)?;
        let applied = if view.state == ControlCommandState::Accepted {
            self.command_update(
                view,
                ControlTransition::Applied {
                    state_transition: evidence.clone(),
                    turn_revision: event.expected_revision + 1,
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
            )?
        } else {
            self.commands
                .receipt(&view.request.command_id)
                .map_err(error)?
                .ok_or_else(|| error("applied control disappeared"))?
        };
        if applied.view().state == ControlCommandState::Applied {
            if !matches!(applied.view().last_transition.as_ref(), Some(ControlTransition::Applied { state_transition, .. }) if state_transition == &evidence)
            {
                return Err(error(
                    "applied control does not select its exact canonical evidence",
                ));
            }
            self.command_update(
                applied.view(),
                ControlTransition::Settled { result: evidence },
            )?;
        }
        Ok(())
    }

    fn apply_control(&mut self, view: &CommandReceiptView) -> Result<()> {
        if matches!(
            view.request.parameters,
            ControlParameters::ResumeBlocked { .. }
        ) {
            return self.apply_resume(view);
        }
        if matches!(
            view.request.parameters,
            ControlParameters::SteerActivation { .. }
        ) {
            // Accepted is the durable bounded queue. Only an actual actor
            // safe-boundary callback may apply and acknowledge its input.
            return Ok(());
        }
        if let ControlParameters::StopActivation { activation } = &view.request.parameters {
            let bound = self
                .bound
                .get(&activation.activation_id)
                .filter(|bound| bound.activation == *activation)
                .cloned();
            let Some(bound) = bound else {
                let snapshot = self.current(activation)?;
                let input = &snapshot
                    .contract()
                    .activations()
                    .iter()
                    .find(|item| item.activation == *activation)
                    .ok_or_else(|| error("unbound Stop has no canonical activation"))?
                    .input;
                let ActivationEvidenceContent::Definition { profile, .. } = self
                    .content
                    .resolve_activation_evidence(&input.definition.snapshot)
                    .map_err(error)?
                else {
                    return Err(error("unbound Stop has no retained execution profile"));
                };
                // This stopped accounting record mints no lease and consumes no
                // activation grant. It proves zero dispatch across restart and
                // lets a later retry retain honest conversation accounting.
                self.authority
                    .record_undispatched_activation(
                        &snapshot,
                        activation,
                        profile.clone(),
                        self.authority.revision().map_err(error)?,
                    )
                    .map_err(error)?;
                let envelope = self.control_envelope(
                    view,
                    TurnContractEvent::FailActivation {
                        activation: activation.clone(),
                        evidence: self.command_request_evidence(view)?,
                    },
                )?;
                self.canonical.append(envelope.clone()).map_err(error)?;
                self.changed.notify_waiters();
                return self.settle_control_event(view, &envelope);
            };
            let revision = self.authority.revision().map_err(error)?;
            match &view.source {
                CommandSourceRecord::Human { .. } => self
                    .authority
                    .stop_activation(activation, revision)
                    .map_err(error)?,
                CommandSourceRecord::Agent {
                    activation: source, ..
                } => {
                    let lease = &self
                        .bound
                        .get(&source.activation_id)
                        .ok_or_else(|| error("Stop source lost its live owner"))?
                        .lease;
                    if self
                        .authority
                        .grant_policy(
                            self.bound
                                .get(&source.activation_id)
                                .unwrap()
                                .grant
                                .grant_id
                                .as_str(),
                        )
                        .map_err(error)?
                        .delegation
                        .is_some()
                    {
                        self.validate_control_source(view)?;
                        self.authority
                            .stop_activation(activation, revision)
                            .map_err(error)?;
                    } else {
                        self.authority
                            .stop_descendant(lease, activation, revision, now_ms()?)
                            .map_err(error)?;
                    }
                }
            }
            // Return Accepted immediately; the receipt settles once the
            // stopped actor returns and its terminal record is appended.
            bound.control.cancel();
            self.changed.notify_waiters();
            return Ok(());
        }
        if self.graph_control_input(view).is_some() {
            return self.apply_graph_control(view);
        }
        if matches!(
            view.request.parameters,
            ControlParameters::FinishTurn {
                mode: FinishMode::Normal
            }
        ) {
            return self.finish_control(view);
        }
        let event =
            Self::control_event(view).ok_or_else(|| error("unsupported accepted control"))?;
        let envelope = self.control_envelope(view, event)?;
        self.canonical.append(envelope.clone()).map_err(error)?;
        self.changed.notify_waiters();
        if matches!(
            view.request.parameters,
            ControlParameters::FinishTurn {
                mode: FinishMode::ForcePartial { .. }
            }
        ) {
            self.reconcile_turn_stop()?;
            return self.settle_partial_finish(view);
        }
        self.settle_control_event(view, &envelope)
    }

    fn settle_partial_finish(&mut self, view: &CommandReceiptView) -> Result<()> {
        let Some(request) = self.applied_control(view)? else {
            return Ok(());
        };
        let snapshot = self.canonical.snapshot(&self.turn_id).map_err(error)?;
        let intent = snapshot
            .contract()
            .stop_requested()
            .ok_or_else(|| error("partial Finish lacks its canonical closing intent"))?;
        if intent.command_id != request.command_id || intent.partial_finish.is_none() {
            return Err(error("partial Finish does not own this closing request"));
        }
        let applied_evidence = self.canonical_control_evidence(&request)?;
        let applied = if view.state == ControlCommandState::Accepted {
            self.command_update(
                view,
                ControlTransition::Applied {
                    state_transition: applied_evidence.clone(),
                    turn_revision: request.expected_revision + 1,
                    graph_revision: snapshot.contract().graph().unwrap().revision,
                    pending: self.command_request_evidence(view)?,
                },
            )?
        } else {
            self.commands
                .receipt(&view.request.command_id)
                .map_err(error)?
                .ok_or_else(|| error("partial Finish receipt disappeared"))?
        };
        if !matches!(applied.view().last_transition.as_ref(), Some(ControlTransition::Applied { state_transition, .. }) if state_transition == &applied_evidence)
        {
            return Err(error(
                "partial Finish Applied receipt differs from its canonical request",
            ));
        }
        if snapshot.contract().state() == Some(LogicalTurnState::Finished) {
            let closure = self
                .canonical
                .turn_records(&self.turn_id)
                .map_err(error)?
                .into_iter()
                .find(|record| {
                    matches!(
                        record.event,
                        TurnContractEvent::Close {
                            closure: TurnClosure::Finished
                        }
                    )
                })
                .ok_or_else(|| error("partial Finish lacks its safe closure"))?;
            self.close_and_promote(closure.clone())?;
            self.command_update(
                applied.view(),
                ControlTransition::Settled {
                    result: self.canonical_control_evidence(&closure)?,
                },
            )?;
        }
        Ok(())
    }

    fn finish_control(&mut self, view: &CommandReceiptView) -> Result<()> {
        let snapshot = self.canonical.snapshot(&self.turn_id).map_err(error)?;
        if snapshot
            .contract()
            .activations()
            .iter()
            .any(|item| item.state == ActivationState::Running)
            || self.other_pending_control(&view.request.command_id)?
        {
            return Ok(());
        }
        if self.driver.is_some()
            && snapshot.contract().state() == Some(LogicalTurnState::Running)
            && !snapshot.contract().completion_satisfied()
        {
            // A parent may have settled before its child is allocated. Only
            // the driver's quiescence decision establishes that no work remains.
            return Ok(());
        }
        let envelope = self.control_envelope(
            view,
            TurnContractEvent::Close {
                closure: TurnClosure::Completed,
            },
        )?;
        let mut preview = snapshot.contract().clone();
        if let Err(reason) = preview.apply(&envelope) {
            self.command_update(
                view,
                ControlTransition::Failed {
                    failure: command_failure("finish_conditions_unmet", reason, None),
                },
            )?;
            return Ok(());
        }
        self.close_and_promote(envelope.clone())?;
        self.settle_control_event(view, &envelope)
    }

    pub(super) fn reconcile_control_commands(&mut self) -> Result<()> {
        self.ready()?;
        self.reconcile_turn_stop()?;
        for id in self.control_ids()? {
            let receipt = self
                .commands
                .receipt(&id)
                .map_err(error)?
                .ok_or_else(|| error("control receipt disappeared"))?;
            let view = receipt.view();
            if view.state.is_terminal() {
                continue;
            }
            if view.state == ControlCommandState::Requested {
                self.command_update(
                    view,
                    ControlTransition::Rejected {
                        failure: command_failure(
                            "request_not_accepted",
                            "request had no durable acceptance; submit a new exact operation",
                            None,
                        ),
                    },
                )?;
                continue;
            }
            if matches!(
                view.request.parameters,
                ControlParameters::SteerActivation { .. }
            ) {
                self.reconcile_steer(view)?;
                continue;
            }
            if matches!(
                view.request.parameters,
                ControlParameters::ResumeBlocked { .. }
            ) {
                self.reconcile_resume(view)?;
                continue;
            }
            if matches!(
                view.request.parameters,
                ControlParameters::FinishTurn {
                    mode: FinishMode::ForcePartial { .. }
                }
            ) && self.applied_control(view)?.is_some()
            {
                self.settle_partial_finish(view)?;
                continue;
            }
            if let Some(envelope) = self.applied_control(view)? {
                if matches!(envelope.event, TurnContractEvent::Close { .. }) {
                    self.close_and_promote(envelope.clone())?;
                }
                self.settle_control_event(view, &envelope)?;
                continue;
            }
            let snapshot = self.canonical.snapshot(&self.turn_id).map_err(error)?;
            if let ControlParameters::StopActivation { activation } = &view.request.parameters {
                let terminal = self.canonical.turn_records(&self.turn_id).map_err(error)?.into_iter().find(|record| {
                    matches!(&record.event, TurnContractEvent::FailActivation { activation: target, .. } if target == activation)
                });
                if let Some(terminal) = terminal {
                    self.settle_control_event(view, &terminal)?;
                    continue;
                }
            }
            let interrupted = snapshot.contract().epochs().last().is_none_or(|epoch| {
                epoch.id != view.request.execution_epoch_id
                    || epoch.state == EpochState::Interrupted
            });
            if interrupted
                || snapshot
                    .contract()
                    .state()
                    .is_some_and(LogicalTurnState::is_closed)
            {
                self.command_update(view, ControlTransition::Failed {
                    failure: command_failure("execution_ownership_lost", "unfinished control requires explicit recovery; no actor or effect was replayed", None),
                })?;
            } else if matches!(
                view.request.parameters,
                ControlParameters::FinishTurn {
                    mode: FinishMode::Normal
                }
            ) {
                self.finish_control(view)?;
            } else if !matches!(
                view.request.parameters,
                ControlParameters::StopActivation { .. }
            ) {
                // A durable Accepted receipt without a canonical operation is
                // not permission to create one during reconstruction.
                self.command_update(view, ControlTransition::Failed {
                    failure: command_failure("control_not_applied", "accepted operation has no canonical application; explicit recovery is required", None),
                })?;
            }
        }
        Ok(())
    }
}

fn command_failure(
    code: &str,
    reason: impl std::fmt::Display,
    evidence: Option<EvidenceRef>,
) -> CommandFailure {
    CommandFailure {
        code: code.into(),
        message: reason
            .to_string()
            .chars()
            .filter(|c| *c != '\0')
            .take(255)
            .collect(),
        evidence,
        blocker: None,
    }
}
