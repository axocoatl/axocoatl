//! Exact host driver setup; all execution stays in the existing autonomous driver.
use super::*;

impl SessionDispatchController {
    pub(crate) fn with_team_stores<T>(
        &self,
        use_stores: impl FnOnce(
            &SessionExecutionStore,
            &mut ExecutionContentStore,
            &mut ActivationStateStore,
        ) -> std::result::Result<T, crate::error::DaemonError>,
    ) -> std::result::Result<T, crate::error::DaemonError> {
        let failure = |error: SessionDispatchError| {
            crate::error::DaemonError::SessionConflict(error.to_string())
        };
        let mut state = self.lock().map_err(failure)?;
        state.ready().map_err(failure)?;
        state
            .content
            .verify_canonical_owner(&state.canonical)
            .map_err(|error| crate::error::DaemonError::SessionConflict(error.to_string()))?;
        state
            .memory
            .verify_canonical_owner(&state.canonical)
            .map_err(|error| crate::error::DaemonError::SessionConflict(error.to_string()))?;
        let DispatchState {
            canonical,
            content,
            memory,
            ..
        } = &mut *state;
        use_stores(canonical, content, memory)
    }

    pub(crate) fn prepare_native_host_driver(
        &self,
        source: &str,
        repository: EvidenceRef,
        bus: crate::stream::StreamBus,
        factory: Arc<dyn AutonomousActivationFactory>,
    ) -> Result<Option<AutonomousTurnDriver>> {
        self.prepare_native_driver(Some(source), None, repository, bus, factory)
    }
    pub(crate) fn prepare_native_control_driver(
        &self,
        command: &CommandId,
        repository: EvidenceRef,
        bus: crate::stream::StreamBus,
        factory: Arc<dyn AutonomousActivationFactory>,
    ) -> Result<Option<AutonomousTurnDriver>> {
        self.prepare_native_driver(None, Some(command), repository, bus, factory)
    }
    fn prepare_native_driver(
        &self,
        source: Option<&str>,
        command: Option<&CommandId>,
        repository: EvidenceRef,
        bus: crate::stream::StreamBus,
        factory: Arc<dyn AutonomousActivationFactory>,
    ) -> Result<Option<AutonomousTurnDriver>> {
        let (admission, grants, canonical_command) = {
            let mut state = self.lock()?;
            state.ready()?;
            let (_, admission) = state
                .content
                .turn_admission(&state.canonical, &state.turn_id)
                .map_err(error)?
                .ok_or_else(|| error("native Begin has no retained host input"))?;
            if source.is_some_and(|source| admission.source != source) {
                return Err(error("native turn ID was reused with different host input"));
            }
            let admission = admission.clone();
            if serde_json::from_str::<crate::bootstrap::native_ways::NativeWaysAdmission>(
                &admission.source,
            )
            .is_ok()
            {
                return Err(error("This turn belongs to isolated Ways; use its existing comparison and cleanup controls"));
            }
            let snapshot = state.canonical.snapshot(&state.turn_id).map_err(error)?;
            if state.driver.is_some()
                || snapshot.contract().state() != Some(LogicalTurnState::Running)
            {
                return Ok(None);
            }
            let canonical_command = if let Some(command) = command {
                use axocoatl_session::control_command::{ControlCommandState, ControlParameters};
                let receipt = state
                    .commands
                    .receipt(command)
                    .map_err(error)?
                    .ok_or_else(|| error("control driver has no actual command receipt"))?;
                let view = receipt.view();
                if !matches!(
                    view.state,
                    ControlCommandState::Applied | ControlCommandState::Settled
                ) || !matches!(
                    view.request.parameters,
                    ControlParameters::ContinueTurn { .. }
                        | ControlParameters::RetryActivation { .. }
                        | ControlParameters::ReviseActivation { .. }
                ) {
                    return Ok(None);
                }
                if state
                    .content
                    .control_driver_handed_off(&state.canonical, &state.turn_id, command)
                    .map_err(error)?
                {
                    return Ok(None);
                }
                let event = state
                    .applied_control(view)?
                    .ok_or_else(|| error("control receipt lacks its canonical transition"))?;
                let current_epoch = &snapshot
                    .contract()
                    .epochs()
                    .last()
                    .ok_or_else(|| error("native turn has no epoch"))?
                    .id;
                let matches_current = match &event.event {
                    TurnContractEvent::Continue { plan } => &plan.epoch_id == current_epoch,
                    TurnContractEvent::StartActivation { input }
                    | TurnContractEvent::ReviseAccepted { input, .. } => {
                        &input.activation.execution_epoch_id == current_epoch
                            && snapshot
                                .contract()
                                .activations()
                                .iter()
                                .rev()
                                .find(|item| item.activation.node_id == input.activation.node_id)
                                .is_some_and(|item| {
                                    item.activation == input.activation
                                        && matches!(
                                            item.state,
                                            ActivationState::Running | ActivationState::Unstarted
                                        )
                                })
                    }
                    _ => false,
                };
                if !matches_current {
                    return Ok(None);
                }
                Some(event.command_id)
            } else {
                if state
                    .content
                    .turn_driver_handed_off(&state.canonical, &state.turn_id)
                    .map_err(error)?
                {
                    return Ok(None);
                }
                None
            };
            state.execution_admission()?;
            match &state.stream_bus {
                Some(existing) if !existing.same_bus(&bus) => {
                    return Err(error("native setup belongs to another daemon stream bus"))
                }
                Some(_) => {}
                None => state.stream_bus = Some(bus.clone()),
            }
            let owner = state
                .repository_owners
                .get(&repository)
                .ok_or_else(|| error("native driver lacks its exact retained repository owner"))?;
            repository::validate_retained_repository(&state, owner, &repository)?;
            let mut grants = Vec::new();
            for node in &admission.nodes {
                let ActivationEvidenceContent::Grant { policy } = &state
                    .content
                    .resolve_activation_evidence(&node.grant.evidence)
                    .map_err(error)?
                else {
                    return Err(error("retained node grant has the wrong role"));
                };
                grants.push(policy.clone());
            }
            (admission, grants, canonical_command)
        };
        // Control resumes retain the existing authority journal and its actual
        // charges/revocations; they never reinstall an original grant revision.
        if command.is_none() {
            for grant in grants {
                self.install_grant(grant.clone())?;
                if grant.delegation.is_some() {
                    let state = self.lock()?;
                    let snapshot = state.canonical.snapshot(&state.turn_id).map_err(error)?;
                    state
                        .authority
                        .acknowledge_native_delegation(
                            &snapshot,
                            &grant.id,
                            state.authority.revision().map_err(error)?,
                        )
                        .map_err(error)?;
                }
            }
        }
        let inputs = {
            let state = self.lock()?;
            let snapshot = state.canonical.snapshot(&state.turn_id).map_err(error)?;
            let graph = snapshot
                .contract()
                .graph()
                .ok_or_else(|| error("native driver has no canonical graph"))?;
            graph
                .nodes
                .iter()
                .map(|selected| {
                    if let Some(node) = admission
                        .nodes
                        .iter()
                        .find(|node| node.node_id == selected.node_id)
                    {
                        Ok(AutonomousNodeInput {
                            node_id: node.node_id.clone(),
                            guidance: node.guidance.clone(),
                            attachments: node.attachments.clone(),
                            repository: RepositoryInput::Recorded {
                                snapshot: repository.clone(),
                            },
                            budget: node.budget.clone(),
                            grant: Some(node.grant.clone()),
                        })
                    } else {
                        state.dynamic_node_input(&selected.node_id)?.ok_or_else(|| {
                            error("native dynamic node has no retained applied admission")
                        })
                    }
                })
                .collect::<Result<Vec<_>>>()?
        };
        let driver = match self.autonomous_turn_driver(inputs, factory) {
            Ok(driver) => driver,
            Err(failure) => {
                let state = self.lock()?;
                if state.driver.is_some()
                    || state
                        .canonical
                        .snapshot(&state.turn_id)
                        .map_err(error)?
                        .contract()
                        .state()
                        != Some(LogicalTurnState::Running)
                {
                    return Ok(None);
                }
                return Err(failure);
            }
        };
        {
            let mut state = self.lock()?;
            state.execution_admission()?;
            let turn_id = state.turn_id.clone();
            let DispatchState {
                canonical, content, ..
            } = &mut *state;
            let result =
                if let (Some(command), Some(canonical_command)) = (command, canonical_command) {
                    content
                        .retain_control_driver_handoff(
                            canonical,
                            &turn_id,
                            command.clone(),
                            canonical_command,
                        )
                        .map_err(error)
                } else {
                    content
                        .retain_turn_driver_handoff(canonical, &turn_id)
                        .map_err(error)
                };
            state.fail_closed(result)?;
        }
        let snapshot = self.snapshot()?;
        let session = snapshot.owner().session_id.as_str().to_owned();
        let turn_id = snapshot.turn_id().as_str().to_owned();
        if command.is_none() {
            let _ = bus.send(crate::stream::StreamFrame::SessionAccepted {
                session: session.clone(),
                turn_id: turn_id.clone(),
            });
        }
        let _ = bus.send(crate::stream::StreamFrame::SessionStart {
            session,
            turn_id: Some(turn_id),
        });
        Ok(Some(driver))
    }
}
