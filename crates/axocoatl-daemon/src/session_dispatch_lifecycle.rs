//! Controller-owned conversation finalization and consuming successor handoff.
//! These operations retain format ownership; they do not settle external effects
//! or authorize replay of an interrupted provider/tool call.
use super::*;
use axocoatl_memory::activation_state::PromotionManifest;
use axocoatl_session::execution_content::ExecutionRequestContent;

/// Acknowledged canonical closure and exact completed conversation promotion.
/// This immutable receipt is not an execution lease or a deserializable claim.
#[derive(Debug)]
pub struct FinalizedTurn {
    snapshot: DurableTurnSnapshot,
    promotion: PromotionManifest,
}
impl FinalizedTurn {
    pub fn snapshot(&self) -> &DurableTurnSnapshot {
        &self.snapshot
    }
    pub fn promotion(&self) -> &PromotionManifest {
        &self.promotion
    }
}

/// The host retains these IDs across an uncertain Begin acknowledgement. A new
/// command or turn ID is new work, never a recovery strategy for the same request.
pub struct SuccessorTurn {
    pub command_id: CommandId,
    pub turn_id: LogicalTurnId,
    pub epoch_id: ExecutionEpochId,
    pub graph: TurnGraphSnapshot,
    pub request: ExecutionRequestContent,
}

impl SessionDispatchController {
    #[cfg(test)]
    pub(crate) fn fail_registered_successor_request_for_test(&self) {
        self.lock().unwrap().fail_at = Some(TestFailure::SuccessorRequest);
    }

    pub fn close_and_promote(&self, envelope: TurnContractEnvelope) -> Result<FinalizedTurn> {
        self.lock()?.close_and_promote(envelope)
    }

    /// Reconcile an already closed canonical turn without inventing a new Close
    /// command. It cannot reopen an epoch or dispatch providers.
    pub fn finalize_closed_turn(&self) -> Result<FinalizedTurn> {
        let mut state = self.lock()?;
        state.ready()?;
        state
            .canonical
            .snapshot(&state.turn_id)
            .map_err(error)?
            .contract()
            .closed_reference()
            .map_err(error)?;
        let result = (|| {
            let revision = state.authority.revision().map_err(error)?;
            state.authority.close_dispatch(revision).map_err(error)?;
            state.reconcile_promotions()?;
            state.finalized_turn()
        })();
        state.fail_closed(result)
    }

    /// Transfer a quiescent owner into a fresh controller. Any remaining actor,
    /// provider, observer, or driver handle prevents consuming the old owner.
    /// On failure, durable state must be inspected before constructing another
    /// request; a persisted successor is recovered, never automatically executed.
    pub fn begin_successor(self, spec: SuccessorTurn) -> Result<Self> {
        {
            let state = self.lock()?;
            Self::require_unoccupied_legacy_turn_id(
                &state.canonical,
                &state.content,
                &spec.turn_id,
            )?;
            if state.repository_registration.is_some() || !state.repository_owners.is_empty() {
                return Err(error("a registered repository controller must advance through its retained daemon registry"));
            }
        }
        let mutex = Arc::try_unwrap(self.state).map_err(|_| {
            error("successor handoff requires all predecessor handles to be released")
        })?;
        let mut state = mutex
            .into_inner()
            .map_err(|_| error("controller lock failed during handoff"))?;
        state.ready()?;
        if state.driver.is_some() {
            return Err(error(
                "successor handoff requires the turn driver to release ownership",
            ));
        }
        state.reconcile_promotions()?;
        let finalized = state.finalized_turn()?;
        if state.canonical.unfinished_turn().map_err(error)?.is_some()
            || spec.turn_id == state.turn_id
            || spec.request.turn_id != spec.turn_id
            || state
                .canonical
                .turn(&spec.turn_id)
                .map_err(error)?
                .is_some()
            || state
                .canonical
                .records()
                .map_err(error)?
                .iter()
                .any(|record| record.command_id == spec.command_id)
        {
            return Err(error(
                "successor identity is occupied, mismatched, or Session work is unfinished",
            ));
        }
        let envelope = TurnContractEnvelope {
            schema_version: TURN_CONTRACT_SCHEMA_VERSION,
            command_id: spec.command_id,
            expected_revision: 0,
            session_id: state.canonical.owner().session_id.clone(),
            turn_id: spec.turn_id.clone(),
            event: TurnContractEvent::Begin {
                epoch_id: spec.epoch_id,
                graph: spec.graph,
                predecessor: Some(finalized.promotion.closure.clone()),
            },
        };
        let mut preview = TurnContract::default();
        preview.apply(&envelope).map_err(error)?;
        let TurnContractEvent::Begin { graph, .. } = &envelope.event else {
            return Err(error("successor admission requires Begin"));
        };
        state
            .memory
            .validate_starting_savepoints(graph)
            .map_err(error)?;
        for node in &graph.nodes {
            let definition = state
                .content
                .resolve_activation_evidence(&node.definition.snapshot)
                .map_err(error)?;
            if !matches!(definition, ActivationEvidenceContent::Definition { definition_id, .. } if definition_id == &node.definition.definition_id)
            {
                return Err(error(
                    "successor definition evidence differs from its graph",
                ));
            }
        }
        let result = (|| {
            let request = state.content.retain_request(spec.request).map_err(error)?;
            #[cfg(test)]
            state.trip(TestFailure::SuccessorRequest)?;
            state
                .canonical
                .begin_with_request(envelope, &request)
                .map_err(error)?;
            #[cfg(test)]
            state.trip(TestFailure::SuccessorBegin)?;
            Ok(())
        })();
        state.fail_closed(result)?;
        // Release child namespaces while preserving the same held canonical
        // journal and format owner. Opening the controller does not reopen the
        // canonical journal and therefore cannot interrupt this fresh epoch.
        let DispatchState {
            canonical,
            content,
            memory,
            audit,
            authority,
            commands,
            ..
        } = state;
        drop((content, memory, audit, authority, commands));
        Self::open(canonical, spec.turn_id)
    }

    pub(crate) fn close_registered_repository_admission(&self) -> Result<()> {
        let mut state = self.lock()?;
        state.execution_admission_closed = true;
        if let Some(gate) = state
            .repository_registration
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
        {
            gate.close();
        }
        for bound in state.bound.values() {
            bound.control.cancel();
        }
        for check in state.repository_checks.values() {
            check.cancel();
        }
        for owner in state.repository_owners.values() {
            owner.request_supervised_stop().map_err(error)?;
        }
        let result = (|| {
            let revision = state.authority.revision().map_err(error)?;
            state.authority.suspend_dispatch(revision).map_err(error)?;
            let snapshot = state.canonical.snapshot(&state.turn_id).map_err(error)?;
            if snapshot.contract().state() == Some(LogicalTurnState::Running) {
                let epoch_id = snapshot
                    .contract()
                    .epochs()
                    .last()
                    .ok_or_else(|| error("running turn has no execution epoch"))?
                    .id
                    .clone();
                state
                    .canonical
                    .append(TurnContractEnvelope {
                        schema_version: TURN_CONTRACT_SCHEMA_VERSION,
                        command_id: CommandId::new(format!(
                            "lifecycle-stop:{}",
                            uuid::Uuid::new_v4()
                        ))
                        .map_err(error)?,
                        expected_revision: snapshot.contract().revision(),
                        session_id: snapshot.owner().session_id.clone(),
                        turn_id: snapshot.turn_id().clone(),
                        event: TurnContractEvent::InterruptEpoch { epoch_id },
                    })
                    .map_err(error)?;
            }
            Ok(())
        })();
        state.changed.notify_waiters();
        state.fail_closed(result)
    }

    /// Release only an exact finalized turn's physical resource. A closed
    /// journal alone is insufficient: every owned execution ticket and the
    /// actual repository execution lease must also have settled.
    pub(crate) fn release_finalized_repository(
        &self,
        turn_id: &LogicalTurnId,
        owner: &crate::bootstrap::session_repository::SessionRepositoryOwner,
    ) -> Result<(PromotionManifest, tokio::sync::OwnedMutexGuard<()>)> {
        let mut state = self.lock()?;
        state.ready()?;
        if &state.turn_id != turn_id
            || state.driver.is_some()
            || !state.repository_checks.is_empty()
            || !state.execution_lifetimes.is_idle()
            || state.repository_owners.len() != 1
            || !state
                .repository_owners
                .values()
                .any(|held| held.same_owner(owner))
        {
            return Err(error(
                "repository release requires the exact quiescent registered turn",
            ));
        }
        let finalized = state.finalized_turn()?;
        if !finalized
            .snapshot
            .contract()
            .state()
            .is_some_and(LogicalTurnState::is_closed)
            || finalized.snapshot.owner() != owner.identity().owner()
            || finalized.snapshot.journal_id() != owner.identity().journal_id()
            || !owner.execution_is_idle().map_err(error)?
        {
            return Err(error(
                "repository release lacks finalized identity or actual settlement",
            ));
        }
        // A partial closed turn can retain unresolved intents. Reconcile only
        // retained evidence; closed canonical bytes and dispatch gates remain
        // unchanged. Missing outcomes cannot stand in for process settlement.
        let reconciliation = state
            .reconcile()
            .and_then(|()| state.reconcile_conditions());
        state.fail_closed(reconciliation)?;
        state.require_repository_effect_evidence()?;
        let operation = owner.retire_idle().map_err(error)?;
        // No await after retiring the real owner. Old controls cannot acquire
        // another execution ticket while the Workspace passes to peer writers.
        state.execution_admission_closed = true;
        if let Some(gate) = state
            .repository_registration
            .take()
            .and_then(|gate| gate.upgrade())
        {
            gate.close();
        }
        state.repository_owners.clear();
        state.repository_reattachments.clear();
        state.changed.notify_waiters();
        Ok((finalized.promotion, operation))
    }

    pub(crate) fn finalized_repository_identity(&self) -> Result<PromotionManifest> {
        let state = self.lock()?;
        state.ready()?;
        Ok(state.finalized_turn()?.promotion)
    }

    /// Enable only after the registry has retained a fresh owner and the
    /// immutable resource evidence has been acknowledged.
    pub(crate) fn enable_reacquired_repository(&self) -> Result<()> {
        let mut state = self.lock()?;
        state.ready()?;
        let identity = state.canonical.identity().map_err(error)?;
        if state.repository_owners.len() != 1
            || !state.execution_lifetimes.is_idle()
            || !state
                .repository_registration
                .as_ref()
                .and_then(std::sync::Weak::upgrade)
                .is_some_and(|gate| gate.permits(&identity))
        {
            return Err(error(
                "reacquired resource has no complete retained registration",
            ));
        }
        state.execution_admission_closed = false;
        Ok(())
    }

    /// Registry-owned advancement preserves the same canonical store on every
    /// failure. Child stores with Session lifetime remain open, and idle actual
    /// repository owners move forward with their existing content references.
    pub(crate) fn begin_registered_successor(&self, spec: SuccessorTurn) -> Result<()> {
        if Arc::strong_count(&self.state) != 1 {
            return Err(error("registered successor requires external driver, provider and observer handles to finish"));
        }
        let mut state = self.lock()?;
        state.ready()?;
        // Whole-turn Stop closes the predecessor permanently. A separately
        // admitted successor uses a fresh turn/authority after finalized proof;
        // the independent lifecycle fence must still remain open.
        if state.execution_admission_closed {
            return Err(error("Session lifecycle has closed successor admission"));
        }
        let identity = state.canonical.identity().map_err(error)?;
        if !state
            .repository_registration
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
            .is_some_and(|gate| gate.permits(&identity))
        {
            return Err(error(
                "registered successor has no live daemon ownership gate",
            ));
        }
        if state.driver.is_some()
            || !state.repository_checks.is_empty()
            || !state.execution_lifetimes.is_idle()
        {
            return Err(error(
                "registered successor must wait for all owned execution tasks",
            ));
        }
        state.require_repository_effect_evidence()?;
        for owner in state.repository_owners.values() {
            if !owner.execution_is_idle().map_err(error)? {
                return Err(error(
                    "registered successor cannot release active or unknown repository execution",
                ));
            }
        }
        Self::require_unoccupied_legacy_turn_id(&state.canonical, &state.content, &spec.turn_id)?;
        let result = (|| {
            state.reconcile_promotions()?;
            let finalized = state.finalized_turn()?;
            if state.canonical.unfinished_turn().map_err(error)?.is_some()
                || spec.turn_id == state.turn_id
                || spec.request.turn_id != spec.turn_id
                || state
                    .canonical
                    .turn(&spec.turn_id)
                    .map_err(error)?
                    .is_some()
                || state
                    .canonical
                    .records()
                    .map_err(error)?
                    .iter()
                    .any(|record| record.command_id == spec.command_id)
            {
                return Err(error(
                    "successor identity is occupied, mismatched, or Session work is unfinished",
                ));
            }
            let envelope = TurnContractEnvelope {
                schema_version: TURN_CONTRACT_SCHEMA_VERSION,
                command_id: spec.command_id,
                expected_revision: 0,
                session_id: state.canonical.owner().session_id.clone(),
                turn_id: spec.turn_id.clone(),
                event: TurnContractEvent::Begin {
                    epoch_id: spec.epoch_id,
                    graph: spec.graph,
                    predecessor: Some(finalized.promotion.closure.clone()),
                },
            };
            let mut preview = TurnContract::default();
            preview.apply(&envelope).map_err(error)?;
            let TurnContractEvent::Begin { graph, .. } = &envelope.event else {
                return Err(error("successor requires Begin"));
            };
            state
                .memory
                .validate_starting_savepoints(graph)
                .map_err(error)?;
            for node in &graph.nodes {
                let definition = state
                    .content
                    .resolve_activation_evidence(&node.definition.snapshot)
                    .map_err(error)?;
                if !matches!(definition, ActivationEvidenceContent::Definition { definition_id, .. } if definition_id == &node.definition.definition_id)
                {
                    return Err(error(
                        "successor definition evidence differs from its graph",
                    ));
                }
            }
            let request = state.content.retain_request(spec.request).map_err(error)?;
            #[cfg(test)]
            state.trip(TestFailure::SuccessorRequest)?;
            state
                .canonical
                .begin_with_request(envelope, &request)
                .map_err(error)?;
            #[cfg(test)]
            state.trip(TestFailure::SuccessorBegin)?;
            // These namespaces have new turn identities. Open them before
            // replacing old fields so failed storage leaves a retained owner.
            let authority = ControlAuthority::open_owned(
                state
                    .canonical
                    .component_namespace(ExecutionComponent::ControlAuthority {
                        turn_id: spec.turn_id.clone(),
                    })
                    .map_err(error)?,
            )
            .map_err(error)?;
            let commands = ControlCommandStore::open_owned(
                state
                    .canonical
                    .component_namespace(ExecutionComponent::ControlCommands {
                        turn_id: spec.turn_id.clone(),
                    })
                    .map_err(error)?,
            )
            .map_err(error)?;
            state.turn_id = spec.turn_id;
            state.authority = authority;
            state.commands = commands;
            state.bound.clear();
            state.follow_ups.clear();
            state.reconcile()?;
            state.reconcile_conditions()?;
            state.reconcile_promotions()?;
            state.reconcile_control_commands()?;
            state.reconcile_delegate_returns()?;
            Ok(())
        })();
        state.changed.notify_waiters();
        state.fail_closed(result)
    }
}

impl DispatchState {
    pub(super) fn close_and_promote(
        &mut self,
        envelope: TurnContractEnvelope,
    ) -> Result<FinalizedTurn> {
        self.ready()?;
        let TurnContractEvent::Close { closure } = &envelope.event else {
            return Err(error("finalization requires an exact Close envelope"));
        };
        if envelope.session_id != self.canonical.owner().session_id
            || envelope.turn_id != self.turn_id
        {
            return Err(error("closure belongs to another Session or turn"));
        }
        let snapshot = self.canonical.snapshot(&self.turn_id).map_err(error)?;
        let mut preview = snapshot.contract().clone();
        preview.apply(&envelope).map_err(error)?;
        // Validate candidate/base bytes and reserve even zero-activation closure
        // before closing either dispatch or the canonical logical turn.
        let prepared = self
            .memory
            .prepare_close(&snapshot, *closure)
            .map_err(error);
        self.fail_closed(prepared)?;
        let result = (|| {
            let revision = self.authority.revision().map_err(error)?;
            self.authority.close_dispatch(revision).map_err(error)?;
            for bound in self.bound.values() {
                bound.control.cancel();
            }
            for check in self.repository_checks.values() {
                check.cancel();
            }
            self.canonical.append(envelope).map_err(error)?;
            #[cfg(test)]
            self.trip(TestFailure::CanonicalClose)?;
            let snapshot = self.canonical.snapshot(&self.turn_id).map_err(error)?;
            self.memory.promote(&snapshot).map_err(error)?;
            self.reconcile_knowledge()?;
            #[cfg(test)]
            self.trip(TestFailure::Promotion)?;
            self.finalized_turn()
        })();
        let finalized = self.fail_closed(result)?;
        self.changed.notify_waiters();
        Ok(finalized)
    }

    /// Closed canonical history intentionally keeps its original unknown
    /// disposition. Late audit/content evidence can establish the exact outcome
    /// without rewriting that history. It supplements the actual owner's opaque
    /// settlement proof; serialized observations alone never release a writer.
    fn require_repository_effect_evidence(&self) -> Result<()> {
        use axocoatl_session::execution_content::ConditionProcessStatus;
        let snapshot = self.canonical.snapshot(&self.turn_id).map_err(error)?;
        for invocation in snapshot.contract().invocations() {
            if self
                .audit
                .invocation(&invocation.invocation_id)
                .map_err(error)?
                .is_none_or(|record| record.disposition() == EffectDisposition::OutcomeUnknown)
            {
                return Err(error(
                    "repository release requires exact retained invocation outcomes",
                ));
            }
        }
        for run in snapshot.contract().condition_runs() {
            let arguments = self
                .content
                .condition_arguments(&snapshot, &run.run.run_id)
                .map_err(error)?
                .ok_or_else(|| error("repository release lacks condition arguments"))?;
            let result = self
                .content
                .condition_result(&arguments)
                .map_err(error)?
                .ok_or_else(|| error("repository release lacks a retained condition outcome"))?;
            if *result.status() != ConditionProcessStatus::NotDispatched
                && (matches!(result.status(), ConditionProcessStatus::Uncertain { .. })
                    || !result.supervision().is_some_and(|proof| proof.quiescent))
            {
                return Err(error(
                    "repository release requires a settled owned condition outcome",
                ));
            }
        }
        // Provider accounting uncertainty is preserved by the authority ledger.
        // It is not a claim that a repository process is alive or has settled;
        // provider and tool futures must already have dropped their real tickets.
        Ok(())
    }

    pub(super) fn finalized_turn(&self) -> Result<FinalizedTurn> {
        let snapshot = self.canonical.snapshot(&self.turn_id).map_err(error)?;
        let promotion = self
            .memory
            .promotion(&snapshot)
            .map_err(error)?
            .ok_or_else(|| error("canonical closure has no completed conversation promotion"))?;
        Ok(FinalizedTurn {
            snapshot,
            promotion,
        })
    }

    /// Recover exact closed decisions in canonical Begin order, never filename
    /// or checkpoint-version order. A newer frozen graph is not silently rebased.
    pub(super) fn reconcile_promotions(&mut self) -> Result<()> {
        let mut seen = HashSet::new();
        let mut turns = Vec::new();
        for record in self.canonical.records().map_err(error)? {
            if seen.insert(record.turn_id.clone()) {
                turns.push(self.canonical.snapshot(&record.turn_id).map_err(error)?);
            }
        }
        for (index, snapshot) in turns.iter().enumerate() {
            if !snapshot
                .contract()
                .state()
                .is_some_and(LogicalTurnState::is_closed)
                || self.memory.promotion(snapshot).map_err(error)?.is_some()
            {
                continue;
            }
            let promotion = self.memory.preview_promotion(snapshot).map_err(error)?;
            for newer in &turns[index + 1..] {
                if let Some(graph) = newer.contract().graph() {
                    for node in &graph.nodes {
                        if let Some(entry) = promotion
                            .selected
                            .iter()
                            .find(|entry| entry.committed.conversation_id == node.conversation_id)
                        {
                            let expected = ConversationSavepoint::Checkpoint {
                                checkpoint: Box::new(entry.committed.clone()),
                            };
                            if node.starting_savepoint != expected {
                                return Err(error("missing historical promotion conflicts with a newer immutable starting savepoint"));
                            }
                        }
                    }
                }
            }
            self.memory
                .prepare_close(snapshot, promotion.closure.closure())
                .map_err(error)?;
            self.memory.promote(snapshot).map_err(error)?;
        }
        self.reconcile_knowledge()?;
        // A pending promotion may have been completed by Memory::open before
        // this reconciliation. It must not leave a newer unfinished graph using
        // a silently changed baseline, even when the promotion now exists.
        if let Some((_, unfinished)) = self.canonical.unfinished_turn().map_err(error)? {
            if let Some(graph) = unfinished.graph() {
                self.memory
                    .validate_starting_savepoints(graph)
                    .map_err(error)?;
            }
        }
        Ok(())
    }
}
