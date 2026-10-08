//! Canonical ownership before the first v2 Begin. No runtime, background task,
//! execution authority or legacy fallback is manufactured by retaining history.
use super::*;
use crate::session_dispatch::RetainedSessionStores;
use axocoatl_core::SecureDir;
use axocoatl_memory::activation_state::ActivationStateStore;
use axocoatl_session::execution_content::ExecutionContentStore;
use axocoatl_session::execution_namespace::ExecutionComponent;
use axocoatl_session::execution_ownership::UpgradedFormatOwnership;
use axocoatl_session::execution_store::{DurableLegacySeal, SessionExecutionStore};
use axocoatl_session::native_history::NativeSessionCreationReceipt;
use axocoatl_session::turn_contract::*;

struct PendingStores {
    canonical: Option<SessionExecutionStore>,
    content: Option<ExecutionContentStore>,
    memory: Option<ActivationStateStore>,
    legacy_seal: Option<DurableLegacySeal>,
}

impl PendingStores {
    fn canonical(&self) -> Result<&SessionExecutionStore> {
        self.canonical
            .as_ref()
            .ok_or_else(|| failure("canonical store is transitioning"))
    }
    fn verify(&self) -> Result<()> {
        let canonical = self.canonical()?;
        let content = self
            .content
            .as_ref()
            .ok_or_else(|| failure("content initialization is incomplete"))?;
        let memory = self
            .memory
            .as_ref()
            .ok_or_else(|| failure("activation memory initialization is incomplete"))?;
        content
            .verify_canonical_owner(canonical)
            .map_err(|error| failure(error.to_string()))?;
        memory
            .verify_canonical_owner(canonical)
            .map_err(|error| failure(error.to_string()))?;
        if canonical
            .legacy_seal()
            .map_err(|error| failure(error.to_string()))?
            != self.legacy_seal
            || (self.legacy_seal.is_none()
                && canonical
                    .native_origin()
                    .map_err(|error| failure(error.to_string()))?
                    .is_none())
        {
            return Err(failure(
                "Session has no exact retained migration seal or actual native creation origin",
            ));
        }
        Ok(())
    }
    fn restore(&mut self, stores: RetainedSessionStores) {
        self.canonical = Some(stores.canonical);
        self.content = Some(stores.content);
        self.memory = Some(stores.memory);
    }
}

pub(super) struct PendingSessionEntry {
    stores: Mutex<PendingStores>,
    owner: Mutex<Option<SessionRepositoryOwner>>,
    pub(super) cleanup: Arc<AsyncMutex<()>>,
    pub(super) operation: Mutex<Option<Arc<OwnedMutexGuard<()>>>>,
    pub(super) retired: AtomicBool,
    pub(super) closed_history: AtomicBool,
}

impl PendingSessionEntry {
    /// Whether a repository owner is attached, and so holds a live runtime.
    pub(super) fn holds_repository_owner(&self) -> Result<bool> {
        Ok(self
            .owner
            .lock()
            .map_err(|_| failure("pending repository owner failed"))?
            .is_some())
    }

    /// Whether its repository owner holds the Workspace operation.
    pub(super) fn holds_workspace_operation(&self) -> Result<bool> {
        Ok(self
            .owner
            .lock()
            .map_err(|_| failure("pending repository owner failed"))?
            .as_ref()
            .is_some_and(SessionRepositoryOwner::holds_workspace_operation))
    }

    pub(super) fn with_team_stores<T>(
        &self,
        use_stores: impl FnOnce(
            &SessionExecutionStore,
            &mut ExecutionContentStore,
            &mut ActivationStateStore,
        ) -> Result<T>,
    ) -> Result<T> {
        let mut stores = self
            .stores
            .lock()
            .map_err(|_| failure("retained team ownership failed"))?;
        stores.verify()?;
        let PendingStores {
            canonical,
            content,
            memory,
            ..
        } = &mut *stores;
        use_stores(
            canonical.as_ref().expect("verified canonical"),
            content.as_mut().expect("verified content"),
            memory.as_mut().expect("verified memory"),
        )
    }

    fn new(stores: PendingStores) -> Self {
        Self {
            stores: Mutex::new(stores),
            owner: Mutex::new(None),
            cleanup: Arc::new(AsyncMutex::new(())),
            operation: Mutex::new(None),
            retired: AtomicBool::new(false),
            closed_history: AtomicBool::new(false),
        }
    }
    pub(super) fn release_stores_after_cleanup(&self) -> Result<()> {
        let mut stores = self
            .stores
            .lock()
            .map_err(|_| failure("retained history ownership failed"))?;
        // Old opaque tokens may outlive successful Close; they must not keep
        // child inode locks alive or make reopening the actual Session fail.
        stores.memory.take();
        stores.content.take();
        stores.canonical.take();
        self.owner
            .lock()
            .map_err(|_| failure("pending repository owner failed"))?
            .take();
        Ok(())
    }
    pub(super) fn close_admission(&self) -> Result<()> {
        if let Some(owner) = self
            .owner
            .lock()
            .map_err(|_| failure("pending repository owner failed"))?
            .as_ref()
        {
            owner.request_supervised_stop()?;
        }
        Ok(())
    }
    pub(super) fn network_record_namespace(
        &self,
    ) -> Result<axocoatl_session::execution_namespace::OwnedExecutionNamespace> {
        let stores = self
            .stores
            .lock()
            .map_err(|_| failure("retained network record ownership failed"))?;
        crate::session_network::writer_namespace(stores.canonical()?).map_err(failure)
    }

    pub(super) fn read_network_record(
        &self,
        after: Option<u64>,
        limit: usize,
    ) -> Result<
        Option<(
            Vec<axocoatl_session::network_record::NetworkLine>,
            axocoatl_session::network_record::RecordStats,
        )>,
    > {
        let stores = self
            .stores
            .lock()
            .map_err(|_| failure("retained network record ownership failed"))?;
        // A Session between Close and Reopen has released its stores.
        let Ok(canonical) = stores.canonical() else {
            return Ok(None);
        };
        crate::session_network::read_existing(canonical, after, limit).map_err(failure)
    }

    pub(super) fn read_network_screenshot(
        &self,
        sha256: &str,
    ) -> Result<Option<(String, Vec<u8>)>> {
        let stores = self
            .stores
            .lock()
            .map_err(|_| failure("retained network record ownership failed"))?;
        let Ok(canonical) = stores.canonical() else {
            return Ok(None);
        };
        crate::session_network::read_screenshot(canonical, sha256).map_err(failure)
    }

    pub(super) fn history_snapshot(
        &self,
    ) -> Result<axocoatl_session::session_history::SessionHistory> {
        let stores = self
            .stores
            .lock()
            .map_err(|_| failure("retained history ownership failed"))?;
        stores.verify()?;
        let mut history = axocoatl_session::session_history::SessionHistory::from_upgraded(
            stores.canonical()?,
            stores.content.as_ref().expect("verified content"),
        )
        .map_err(|error| failure(error.to_string()))?;
        history
            .apply_superseded(
                &stores
                    .memory
                    .as_ref()
                    .expect("verified memory")
                    .superseded_turn_ids()
                    .map_err(|error| failure(error.to_string()))?,
            )
            .map_err(|error| failure(error.to_string()))?;
        Ok(history)
    }
    pub(super) fn native_turn_replay(&self, turn_id: &LogicalTurnId, source: &str) -> Result<bool> {
        let stores = self
            .stores
            .lock()
            .map_err(|_| failure("retained history ownership failed"))?;
        stores.verify()?;
        let canonical = stores.canonical()?;
        let Some(turn) = canonical
            .turn(turn_id)
            .map_err(|error| failure(error.to_string()))?
        else {
            return Ok(false);
        };
        let content = stores.content.as_ref().expect("verified content");
        let (_, admission) = content
            .turn_admission(canonical, turn_id)
            .map_err(|error| failure(error.to_string()))?
            .ok_or_else(|| failure("retained native turn has no complete host admission source"))?;
        if admission.source != source {
            return Err(failure("native turn ID has a different complete request"));
        }
        Ok(turn.state() != Some(LogicalTurnState::Running)
            || content
                .turn_driver_handed_off(canonical, turn_id)
                .map_err(|error| failure(error.to_string()))?)
    }
    pub(super) fn control_plane(&self, turn_id: &str) -> Result<RegisteredControlPlane> {
        let stores = self
            .stores
            .lock()
            .map_err(|_| failure("retained history ownership failed"))?;
        stores.verify()?;
        let canonical = stores.canonical()?;
        let content = stores.content.as_ref().expect("verified content");
        let superseded = stores
            .memory
            .as_ref()
            .expect("verified memory")
            .superseded_turn_ids()
            .map_err(|error| failure(error.to_string()))?
            .iter()
            .any(|id| id == turn_id);
        if let Ok(id) = LogicalTurnId::new(turn_id) {
            if canonical
                .turn(&id)
                .map_err(|error| failure(error.to_string()))?
                .is_some()
            {
                let snapshot = canonical
                    .snapshot(&id)
                    .map_err(|error| failure(error.to_string()))?;
                let mut view =
                    crate::session_dispatch::SessionDispatchController::project_retained_control_plane(
                        canonical, content, &snapshot,
                    )
                    .map_err(|error| failure(error.to_string()))?;
                view.mark_conversation_superseded(superseded);
                // Only retained cooperative native admission supports these
                // recovery requests. Ways keep their separate existing owner.
                if let Some((_, admission)) = content
                    .turn_admission(canonical, &id)
                    .map_err(|error| failure(error.to_string()))?
                {
                    if serde_json::from_str::<crate::bootstrap::native_turn::NativeFirstTurnRequest>(
                        &admission.source,
                    )
                    .is_ok()
                    {
                        view.expose_closed_turn_controls(&snapshot)
                            .map_err(|error| failure(error.to_string()))?;
                        view.expose_recovery_requests(&snapshot)
                            .map_err(|error| failure(error.to_string()))?;
                    }
                }
                return Ok(RegisteredControlPlane::Found(Box::new(view)));
            }
        }
        if let Some(seal) = &stores.legacy_seal {
            let history = content
                .read_legacy_history(seal)
                .map_err(|error| failure(error.to_string()))?;
            if let Some(turn) = history.turns.iter().find(|turn| turn.id == turn_id) {
                let mut view =
                    crate::session_control_plane::SessionTurnControlPlane::from_legacy(turn);
                view.mark_conversation_superseded(superseded);
                return Ok(RegisteredControlPlane::Found(Box::new(view)));
            }
        }
        Ok(RegisteredControlPlane::MissingTurn)
    }
}

/// Exact process-local history entry, never an execution permission. Rechecked
/// after every asynchronous physical-owner acquisition before any content write.
#[derive(Clone)]
pub(crate) struct PendingSessionToken {
    session_id: String,
    entry: Arc<PendingSessionEntry>,
}

impl SessionDispatchRegistry {
    /// The host calls this only after exact clone/process cleanup and after
    /// recovering retained stores. It never claims unknown effects succeeded.
    pub(crate) fn close_cleaned_native_ways(
        &self,
        session_id: &str,
        set_id: &str,
        decision: Option<&axocoatl_session::ways_decision::WaysDecisionRecord>,
    ) -> Result<()> {
        use axocoatl_session::turn_contract::{
            CommandId, TurnClosure, TurnContractEnvelope, TURN_CONTRACT_SCHEMA_VERSION,
        };
        let state = self
            .state
            .lock()
            .map_err(|_| failure("Session registry failed"))?;
        let entry = state
            .pending
            .get(session_id)
            .ok_or_else(|| failure("Cleaned Ways history is not pending"))?;
        let mut stores = entry
            .stores
            .lock()
            .map_err(|_| failure("Cleaned Ways stores failed"))?;
        stores.verify()?;
        let PendingStores {
            canonical,
            content,
            memory,
            ..
        } = &mut *stores;
        let canonical = canonical.as_mut().expect("verified canonical");
        let content = content.as_ref().expect("verified content");
        let memory = memory.as_mut().expect("verified memory");
        let turn_id = axocoatl_session::turn_contract::LogicalTurnId::new(format!(
            "ways-{}",
            crate::attempts::set_key(set_id)
        ))
        .map_err(|error| failure(error.to_string()))?;
        let Some((_, admission)) = content
            .turn_admission(canonical, &turn_id)
            .map_err(|error| failure(error.to_string()))?
        else {
            return Ok(());
        };
        let source: crate::bootstrap::native_ways::NativeWaysAdmission =
            serde_json::from_str(&admission.source).map_err(|error| failure(error.to_string()))?;
        if source.session_id != session_id
            || source.set_id != set_id
            || source.source_turn_id != turn_id
        {
            return Err(failure(
                "Cleaned Ways source differs from canonical admission",
            ));
        }
        let Some(snapshot) = canonical
            .turn(&turn_id)
            .map_err(|error| failure(error.to_string()))?
        else {
            return Ok(());
        };
        if snapshot.state().is_some_and(|state| state.is_closed()) {
            return Ok(());
        }
        let snapshot = canonical
            .snapshot(&turn_id)
            .map_err(|error| failure(error.to_string()))?;
        let closure = if snapshot.contract().stop_requested().is_some() {
            TurnClosure::Cancelled
        } else {
            use axocoatl_session::ways_decision::{WaysApplicationOutcome, WaysHumanChoice};
            let decision = decision.ok_or_else(|| failure(
                "Incomplete Ways require their retained explicit Keep or NoKeep decision before closure",
            ))?;
            if decision.session_id.as_str() != session_id
                || decision.set_id.0.as_str() != set_id
                || decision.source_turn_id != turn_id
            {
                return Err(failure(
                    "Ways decision differs from exact cleaned execution",
                ));
            }
            let selections = content
                .ways_selections(canonical)
                .map_err(|error| failure(error.to_string()))?;
            let finalized = match (&decision.human_decision.choice, &decision.application) {
                (
                    WaysHumanChoice::Keep { patch },
                    WaysApplicationOutcome::Applied { identity, .. },
                ) => {
                    patch == &identity.patch
                        && decision.selected_session_turn.as_ref().is_some_and(|link| {
                            link.session_id.as_str() == session_id
                                && link.turn_id == turn_id
                                && selections.contains(link)
                        })
                }
                (WaysHumanChoice::NoKeep, WaysApplicationOutcome::NoKeepRecorded { .. }) => {
                    decision.selected_session_turn.is_none()
                }
                _ => false,
            };
            if !finalized {
                return Err(failure(
                    "Ways decision application and exact Session selection are not finalized",
                ));
            }
            TurnClosure::Finished
        };
        let envelope = TurnContractEnvelope {
            schema_version: TURN_CONTRACT_SCHEMA_VERSION,
            command_id: CommandId::new(format!(
                "ways-cleanup-close-{}",
                crate::attempts::set_key(set_id)
            ))
            .map_err(|error| failure(error.to_string()))?,
            expected_revision: snapshot.contract().revision(),
            session_id: canonical.owner().session_id.clone(),
            turn_id: turn_id.clone(),
            event: TurnContractEvent::Close { closure },
        };
        let mut preview = snapshot.contract().clone();
        preview
            .apply(&envelope)
            .map_err(|error| failure(error.to_string()))?;
        memory
            .prepare_close(&snapshot, closure)
            .map_err(|error| failure(error.to_string()))?;
        canonical
            .append(envelope)
            .map_err(|error| failure(error.to_string()))?;
        memory
            .promote(
                &canonical
                    .snapshot(&turn_id)
                    .map_err(|error| failure(error.to_string()))?,
            )
            .map_err(|error| failure(error.to_string()))?;
        Ok(())
    }
    fn require_pending<'a>(
        state: &'a RegistryState,
        token: &PendingSessionToken,
    ) -> Result<&'a Arc<PendingSessionEntry>> {
        if state.closed || state.closing_sessions.contains(&token.session_id) {
            return Err(failure("Session lifecycle admission is closed"));
        }
        state
            .pending
            .get(&token.session_id)
            .filter(|entry| {
                Arc::ptr_eq(entry, &token.entry) && !entry.retired.load(Ordering::SeqCst)
            })
            .ok_or_else(|| failure("retained Session identity changed during first-turn admission"))
    }

    /// Move actual migration-returned stores into the registry before validating
    /// their join. Rejected duplicate admission leaves `migrated` untouched;
    /// later validation errors retain all stores in the inserted exact entry.
    pub(crate) fn retain_migrated_session(
        &self,
        migrated: &mut Option<super::super::session_migration::MigratedSessionState>,
    ) -> Result<PendingSessionToken> {
        let source = migrated
            .as_ref()
            .ok_or_else(|| failure("migration stores already transferred"))?;
        let session_id = source.canonical.owner().session_id.as_str().to_owned();
        let mut state = self
            .state
            .lock()
            .map_err(|_| failure("Session dispatch registry failed"))?;
        if state.closed
            || state.closing_sessions.contains(&session_id)
            || state.entries.contains_key(&session_id)
            || state.pending.contains_key(&session_id)
        {
            return Err(failure(
                "Session cannot replace retained canonical ownership",
            ));
        }
        let source = migrated.take().expect("checked migration ownership");
        let entry = Arc::new(PendingSessionEntry::new(PendingStores {
            canonical: Some(source.canonical),
            content: Some(source.content),
            memory: Some(source.activation_state),
            legacy_seal: Some(source.seal),
        }));
        state.pending.insert(session_id.clone(), entry.clone());
        entry
            .stores
            .lock()
            .map_err(|_| failure("retained history ownership failed"))?
            .verify()?;
        Ok(PendingSessionToken { session_id, entry })
    }

    /// Actual newly-created Session only. Anchor canonical ownership before the
    /// first origin/content/memory write; partial failure remains inspectable and
    /// blocks replacement until explicit cleanup. There is no inferred seal.
    pub(crate) fn retain_native_session(
        &self,
        ownership: Arc<UpgradedFormatOwnership>,
        receipt: NativeSessionCreationReceipt,
    ) -> Result<PendingSessionToken> {
        let session_id = receipt.owner().session_id.as_str().to_owned();
        let mut state = self
            .state
            .lock()
            .map_err(|_| failure("Session dispatch registry failed"))?;
        if state.closed
            || state.closing_sessions.contains(&session_id)
            || state.entries.contains_key(&session_id)
            || state.pending.contains_key(&session_id)
        {
            return Err(failure(
                "Session cannot replace retained canonical ownership",
            ));
        }
        let canonical = SessionExecutionStore::open(ownership, receipt.owner().clone())
            .map_err(|error| failure(error.to_string()))?;
        let entry = Arc::new(PendingSessionEntry::new(PendingStores {
            canonical: Some(canonical),
            content: None,
            memory: None,
            legacy_seal: None,
        }));
        state.pending.insert(session_id.clone(), entry.clone());
        {
            let mut stores = entry
                .stores
                .lock()
                .map_err(|_| failure("retained history ownership failed"))?;
            stores
                .canonical
                .as_mut()
                .expect("retained canonical")
                .record_native_origin(&receipt)
                .map_err(|error| failure(error.to_string()))?;
            stores.content = Some(
                ExecutionContentStore::open_owned(
                    stores
                        .canonical()?
                        .component_namespace(ExecutionComponent::ExecutionContent)
                        .map_err(|error| failure(error.to_string()))?,
                )
                .map_err(|error| failure(error.to_string()))?,
            );
            stores.memory = Some(
                ActivationStateStore::open_owned(
                    stores
                        .canonical()?
                        .component_namespace(ExecutionComponent::ActivationState)
                        .map_err(|error| failure(error.to_string()))?,
                )
                .map_err(|error| failure(error.to_string()))?,
            );
            stores.verify()?;
        }
        Ok(PendingSessionToken { session_id, entry })
    }

    /// Startup receives its already-open child stores. A native origin must
    /// already be recorded; reopening never invents it from absence of history.
    pub(crate) fn retain_existing_session(
        &self,
        source: &mut Option<RetainedSessionStores>,
    ) -> Result<PendingSessionToken> {
        self.retain_existing_history(source, false, false)
    }
    /// The actual successful Close/rebuild host restores immutable history
    /// after releasing the old physical owner, never its executable capability.
    pub(crate) fn retain_existing_lifecycle_session(
        &self,
        source: &mut Option<RetainedSessionStores>,
        closed: bool,
    ) -> Result<PendingSessionToken> {
        self.retain_existing_history(source, closed, true)
    }
    fn retain_existing_history(
        &self,
        source: &mut Option<RetainedSessionStores>,
        closed: bool,
        lifecycle: bool,
    ) -> Result<PendingSessionToken> {
        let session_id = source
            .as_ref()
            .ok_or_else(|| failure("Session stores already transferred"))?
            .canonical
            .owner()
            .session_id
            .as_str()
            .to_owned();
        let mut state = self
            .state
            .lock()
            .map_err(|_| failure("Session dispatch registry failed"))?;
        if state.closed
            || (!lifecycle && state.closing_sessions.contains(&session_id))
            || state.entries.contains_key(&session_id)
            || state.pending.contains_key(&session_id)
        {
            return Err(failure(
                "Session cannot replace retained canonical ownership",
            ));
        }
        let source = source.take().expect("checked retained stores");
        let legacy_seal = source.canonical.legacy_seal().map_err(|error| {
            // Identity failures are not recoverable by dropping other namespaces.
            failure(error.to_string())
        });
        let entry = Arc::new(PendingSessionEntry::new(PendingStores {
            canonical: Some(source.canonical),
            content: Some(source.content),
            memory: Some(source.memory),
            legacy_seal: legacy_seal.as_ref().ok().cloned().flatten(),
        }));
        state.pending.insert(session_id.clone(), entry.clone());
        legacy_seal?;
        entry
            .stores
            .lock()
            .map_err(|_| failure("retained history ownership failed"))?
            .verify()?;
        entry.closed_history.store(closed, Ordering::SeqCst);
        if closed {
            state.closing_sessions.insert(session_id.clone());
        } else if lifecycle {
            state.closing_sessions.remove(&session_id);
        }
        Ok(PendingSessionToken { session_id, entry })
    }

    pub(crate) fn pending_existing_turn(
        &self,
        session_id: &str,
    ) -> Result<Option<(PendingSessionToken, LogicalTurnId)>> {
        let state = self
            .state
            .lock()
            .map_err(|_| failure("Session dispatch registry failed"))?;
        if state.closed || state.closing_sessions.contains(session_id) {
            return Err(failure("Session lifecycle admission is closed"));
        }
        if state.entries.contains_key(session_id) {
            return Ok(None);
        }
        let entry = state
            .pending
            .get(session_id)
            .ok_or_else(|| failure("Session has no retained canonical history"))?
            .clone();
        let token = PendingSessionToken {
            session_id: session_id.into(),
            entry,
        };
        Self::require_pending(&state, &token)?;
        let stores = token
            .entry
            .stores
            .lock()
            .map_err(|_| failure("retained history ownership failed"))?;
        stores.verify()?;
        let latest = stores
            .canonical()?
            .latest_turn()
            .map_err(|error| failure(error.to_string()))?
            .cloned();
        drop(stores);
        Ok(latest.map(|turn| (token, turn)))
    }

    /// Authenticated no-result closure for a recovered pre-invocation failure.
    /// No repository is attached, replaced or treated as settled by this path.
    pub(crate) fn finish_pending_without_result(
        &self,
        request: &crate::session_dispatch::HumanControlActionRequest,
        issued_at_ms: u64,
    ) -> Result<Option<axocoatl_session::control_command::CommandReceiptView>> {
        use crate::session_dispatch::HumanControlAction;
        if request.action != HumanControlAction::Finish
            || request.partial_finish.as_ref().is_none_or(|selection| {
                !selection.confirmed
                    || !selection.selected_activations.is_empty()
                    || !selection.stop_activations.is_empty()
            })
        {
            return Ok(None);
        }
        let state = self
            .state
            .lock()
            .map_err(|_| failure("Session registry failed"))?;
        if state.closed || state.closing_sessions.contains(request.session_id.as_str()) {
            return Err(failure("Session lifecycle admission is closed"));
        }
        let Some(entry) = state.pending.get(request.session_id.as_str()) else {
            return Ok(None);
        };
        if entry.retired.load(Ordering::SeqCst) || entry.closed_history.load(Ordering::SeqCst) {
            return Err(failure("Session lifecycle admission is closed"));
        }
        if entry
            .owner
            .lock()
            .map_err(|_| failure("pending repository owner failed"))?
            .is_some()
        {
            // Existing physical ownership still uses the ordinary guarded
            // attachment/control path; metadata closure cannot settle it.
            return Ok(None);
        }
        let mut stores = entry
            .stores
            .lock()
            .map_err(|_| failure("retained history ownership failed"))?;
        stores.verify()?;
        let canonical = stores.canonical()?;
        if canonical.owner().session_id != request.session_id
            || canonical
                .unfinished_turn()
                .map_err(|error| failure(error.to_string()))?
                .map(|(turn_id, _)| turn_id)
                != Some(&request.turn_id)
        {
            return Err(failure(
                "No-runtime Finish must target the exact unfinished turn",
            ));
        }
        let snapshot = canonical
            .snapshot(&request.turn_id)
            .map_err(|error| failure(error.to_string()))?;
        if !snapshot.contract().invocations().is_empty()
            || !snapshot.contract().condition_runs().is_empty()
        {
            return Ok(None);
        }
        let held = RetainedSessionStores {
            canonical: stores.canonical.take().expect("verified canonical"),
            content: stores.content.take().expect("verified content"),
            memory: stores.memory.take().expect("verified memory"),
        };
        match SessionDispatchController::finish_retained_without_result(
            held,
            request.clone(),
            issued_at_ms,
        ) {
            Ok((held, result)) => {
                stores.restore(held);
                result.map(Some).map_err(|error| failure(error.to_string()))
            }
            Err(failed) => {
                stores.restore(failed.stores);
                Err(failure(failed.error.to_string()))
            }
        }
    }

    /// Explicit recovery attachment retains the actual current owner before
    /// transferring stores. It reopens the existing turn and never appends Begin
    /// or allocates a new generation on behalf of a restarted process.
    pub(crate) fn attach_existing_turn(
        &self,
        token: &PendingSessionToken,
        turn_id: LogicalTurnId,
        owner: SessionRepositoryOwner,
    ) -> Result<(SessionDispatchController, EvidenceRef)> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| failure("Session dispatch registry failed"))?;
        let entry = Self::require_pending(&state, token)?.clone();
        let mut stores = entry
            .stores
            .lock()
            .map_err(|_| failure("retained history ownership failed"))?;
        stores.verify()?;
        if owner.identity()
            != &stores
                .canonical()?
                .identity()
                .map_err(|error| failure(error.to_string()))?
            || !owner.execution_is_idle()?
            || stores
                .canonical()?
                .latest_turn()
                .map_err(|error| failure(error.to_string()))?
                != Some(&turn_id)
        {
            return Err(failure(
                "recovery attachment requires the exact latest turn and current idle Session owner",
            ));
        }
        {
            let mut held = entry
                .owner
                .lock()
                .map_err(|_| failure("pending repository owner failed"))?;
            if held
                .as_ref()
                .is_some_and(|previous| !previous.same_owner(&owner))
            {
                return Err(failure(
                    "recovery cannot replace retained physical ownership",
                ));
            }
            *held = Some(owner.clone());
        }
        let held = RetainedSessionStores {
            canonical: stores.canonical.take().expect("verified canonical"),
            content: stores.content.take().expect("verified content"),
            memory: stores.memory.take().expect("verified memory"),
        };
        let controller = match SessionDispatchController::open_existing_retained(held, turn_id) {
            Ok(controller) => controller,
            Err(failed) => {
                stores.restore(failed.stores);
                return Err(failure(failed.error.to_string()));
            }
        };
        let gate = Arc::new(RepositoryRegistrationGate {
            identity: owner.identity().clone(),
            open: AtomicBool::new(true),
        });
        let registered = Arc::new(RegisteredEntry {
            controller: controller.clone(),
            owner: Mutex::new(owner.clone()),
            gate: Mutex::new(gate.clone()),
            between_turns: Mutex::new(None),
            reference: Mutex::new(None),
            cleanup: Arc::new(AsyncMutex::new(())),
            operation: Mutex::new(None),
            retired: AtomicBool::new(false),
        });
        state
            .entries
            .insert(token.session_id.clone(), registered.clone());
        state.pending.remove(&token.session_id);
        entry.retired.store(true, Ordering::SeqCst);
        entry
            .owner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        drop(stores);
        controller
            .install_hook_registry(self.hooks.clone())
            .map_err(|error| failure(error.to_string()))?;
        controller
            .install_repository_registration(&gate)
            .map_err(|error| failure(error.to_string()))?;
        let reference = controller
            .reattach_repository_resource(owner)
            .map_err(|error| failure(error.to_string()))?;
        *registered
            .reference
            .lock()
            .map_err(|_| failure("repository registration reference failed"))? =
            Some(reference.clone());
        Ok((controller, reference))
    }

    pub(crate) fn prepare_first_turn(&self, session_id: &str) -> Result<PendingSessionToken> {
        let state = self
            .state
            .lock()
            .map_err(|_| failure("Session dispatch registry failed"))?;
        let entry = state
            .pending
            .get(session_id)
            .ok_or_else(|| failure("Session has no retained pre-turn stores"))?
            .clone();
        let token = PendingSessionToken {
            session_id: session_id.into(),
            entry,
        };
        Self::require_pending(&state, &token)?;
        Ok(token)
    }

    pub(crate) fn pending_identity(
        &self,
        token: &PendingSessionToken,
        data_root: &SecureDir,
    ) -> Result<DurableSessionIdentity> {
        let state = self
            .state
            .lock()
            .map_err(|_| failure("Session dispatch registry failed"))?;
        let entry = Self::require_pending(&state, token)?;
        let stores = entry
            .stores
            .lock()
            .map_err(|_| failure("retained history ownership failed"))?;
        stores.verify()?;
        stores
            .canonical()?
            .verify_data_root(data_root)
            .map_err(|error| failure(error.to_string()))?;
        stores
            .canonical()?
            .identity()
            .map_err(|error| failure(error.to_string()))
    }

    /// Synchronous final validation shares Begin's exact registry/store lock.
    /// Team Apply and first-Begin selection cannot interleave across this gate.
    pub(crate) fn begin_first_turn_checked(
        &self,
        token: &PendingSessionToken,
        owner: SessionRepositoryOwner,
        spec: SuccessorTurn,
        validate: impl FnOnce(
            &SessionExecutionStore,
            &ExecutionContentStore,
            &ActivationStateStore,
        ) -> Result<()>,
    ) -> Result<(SessionDispatchController, EvidenceRef)> {
        self.begin_pending_turn_checked(token, owner, spec, false, validate)
    }

    /// Existing Ways ownership can admit a successor after native quiescence.
    /// The prior canonical closure and promotion remain mandatory.
    pub(crate) fn begin_retained_successor_checked(
        &self,
        token: &PendingSessionToken,
        owner: SessionRepositoryOwner,
        spec: SuccessorTurn,
        validate: impl FnOnce(
            &SessionExecutionStore,
            &ExecutionContentStore,
            &ActivationStateStore,
        ) -> Result<()>,
    ) -> Result<(SessionDispatchController, EvidenceRef)> {
        self.begin_pending_turn_checked(token, owner, spec, true, validate)
    }

    fn begin_pending_turn_checked(
        &self,
        token: &PendingSessionToken,
        owner: SessionRepositoryOwner,
        spec: SuccessorTurn,
        allow_predecessor: bool,
        validate: impl FnOnce(
            &SessionExecutionStore,
            &ExecutionContentStore,
            &ActivationStateStore,
        ) -> Result<()>,
    ) -> Result<(SessionDispatchController, EvidenceRef)> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| failure("Session dispatch registry failed"))?;
        let entry = Self::require_pending(&state, token)?.clone();
        let mut stores = entry
            .stores
            .lock()
            .map_err(|_| failure("retained history ownership failed"))?;
        stores.verify()?;
        if owner.identity()
            != &stores
                .canonical()?
                .identity()
                .map_err(|error| failure(error.to_string()))?
            || !owner.execution_is_idle()?
        {
            return Err(failure(
                "first Begin requires the exact idle physical Session owner",
            ));
        }
        {
            let mut retained_owner = entry
                .owner
                .lock()
                .map_err(|_| failure("pending repository owner failed"))?;
            if retained_owner
                .as_ref()
                .is_some_and(|existing| !existing.same_owner(&owner))
            {
                return Err(failure(
                    "first Begin cannot replace its retained physical owner",
                ));
            }
            *retained_owner = Some(owner.clone());
        }
        validate(
            stores.canonical()?,
            stores.content.as_ref().expect("verified content"),
            stores.memory.as_ref().expect("verified memory"),
        )?;
        if spec.request.turn_id != spec.turn_id {
            return Err(failure(
                "Begin request belongs to a different canonical turn",
            ));
        }
        let prior = stores
            .canonical()?
            .latest_turn()
            .map_err(|error| failure(error.to_string()))?
            .cloned();
        let predecessor = match prior {
            Some(prior) if prior != spec.turn_id && allow_predecessor => {
                if stores
                    .canonical()?
                    .unfinished_turn()
                    .map_err(|error| failure(error.to_string()))?
                    .is_some()
                {
                    return Err(failure("successor cannot replace unfinished Session work"));
                }
                let snapshot = stores
                    .canonical()?
                    .snapshot(&prior)
                    .map_err(|error| failure(error.to_string()))?;
                Some(
                    stores
                        .memory
                        .as_ref()
                        .expect("verified memory")
                        .promotion(&snapshot)
                        .map_err(|error| failure(error.to_string()))?
                        .ok_or_else(|| {
                            failure("successor requires the previous turn's acknowledged promotion")
                        })?
                        .closure,
                )
            }
            Some(prior) if prior != spec.turn_id => {
                return Err(failure(
                    "first Begin cannot replace an existing canonical turn",
                ))
            }
            _ => None,
        };
        SessionDispatchController::require_unoccupied_legacy_turn_id(
            stores.canonical()?,
            stores.content.as_ref().expect("verified content"),
            &spec.turn_id,
        )
        .map_err(|error| failure(error.to_string()))?;
        let envelope = TurnContractEnvelope {
            schema_version: TURN_CONTRACT_SCHEMA_VERSION,
            command_id: spec.command_id,
            expected_revision: 0,
            session_id: stores.canonical()?.owner().session_id.clone(),
            turn_id: spec.turn_id.clone(),
            event: TurnContractEvent::Begin {
                epoch_id: spec.epoch_id,
                graph: spec.graph,
                predecessor,
            },
        };
        let mut preview = TurnContract::default();
        preview
            .apply(&envelope)
            .map_err(|error| failure(error.to_string()))?;
        let TurnContractEvent::Begin { graph, .. } = &envelope.event else {
            unreachable!()
        };
        stores
            .memory
            .as_ref()
            .expect("verified memory")
            .validate_starting_savepoints(graph)
            .map_err(|error| failure(error.to_string()))?;
        for node in &graph.nodes {
            let definition = stores
                .content
                .as_ref()
                .expect("verified content")
                .resolve_activation_evidence(&node.definition.snapshot)
                .map_err(|error| failure(error.to_string()))?;
            if !matches!(&definition, axocoatl_session::execution_content::ActivationEvidenceContent::Definition { definition_id, .. } if definition_id == &node.definition.definition_id)
            {
                return Err(failure(
                    "first Begin definition evidence differs from its graph",
                ));
            }
        }
        let request = stores
            .content
            .as_mut()
            .expect("verified content")
            .retain_request(spec.request)
            .map_err(|error| failure(error.to_string()))?;
        stores
            .canonical
            .as_mut()
            .expect("verified canonical")
            .begin_with_request(envelope, &request)
            .map_err(|error| failure(error.to_string()))?;
        let held = RetainedSessionStores {
            canonical: stores.canonical.take().expect("verified canonical"),
            content: stores.content.take().expect("verified content"),
            memory: stores.memory.take().expect("verified memory"),
        };
        let controller = match SessionDispatchController::open_retained(held, spec.turn_id) {
            Ok(controller) => controller,
            Err(failed) => {
                stores.restore(failed.stores);
                return Err(failure(failed.error.to_string()));
            }
        };
        let gate = Arc::new(RepositoryRegistrationGate {
            identity: owner.identity().clone(),
            open: AtomicBool::new(true),
        });
        let registered = Arc::new(RegisteredEntry {
            controller: controller.clone(),
            owner: Mutex::new(owner.clone()),
            gate: Mutex::new(gate.clone()),
            between_turns: Mutex::new(None),
            reference: Mutex::new(None),
            cleanup: Arc::new(AsyncMutex::new(())),
            operation: Mutex::new(None),
            retired: AtomicBool::new(false),
        });
        // No fallible operation between moving the controller into the registry
        // and removing the now-empty pre-turn entry. All later failure retains it.
        state
            .entries
            .insert(token.session_id.clone(), registered.clone());
        state.pending.remove(&token.session_id);
        entry.retired.store(true, Ordering::SeqCst);
        entry
            .owner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        drop(stores);
        registered
            .controller
            .install_hook_registry(self.hooks.clone())
            .map_err(|error| failure(error.to_string()))?;
        registered
            .controller
            .install_repository_registration(&gate)
            .map_err(|error| failure(error.to_string()))?;
        let reference = registered
            .controller
            .retain_repository_resource(owner)
            .map_err(|error| failure(error.to_string()))?;
        *registered
            .reference
            .lock()
            .map_err(|_| failure("repository registration reference failed"))? =
            Some(reference.clone());
        Ok((controller, reference))
    }

    /// A lifecycle action passes the cleanup gate it took (`taken_cleanup`)
    /// and the Workspace it holds (`held`), which becomes the cleanup's
    /// operation when no repository owner holds the Workspace.
    pub(super) async fn prepare_pending_cleanup(
        &self,
        session_id: &str,
        entry: Arc<PendingSessionEntry>,
        timeout: Duration,
        taken_cleanup: Option<OwnedMutexGuard<()>>,
        held: Option<OwnedMutexGuard<()>>,
    ) -> Result<SessionDispatchCleanup> {
        let (operation, cleanup) = tokio::time::timeout(timeout, async {
            let cleanup = Arc::new(match taken_cleanup {
                Some(cleanup) => cleanup,
                None => entry.cleanup.clone().lock_owned().await,
            });
            {
                let state = self
                    .state
                    .lock()
                    .map_err(|_| failure("Session dispatch registry failed"))?;
                if !state
                    .pending
                    .get(session_id)
                    .is_some_and(|current| Arc::ptr_eq(current, &entry))
                {
                    return Err(failure("retained Session changed while cleanup waited"));
                }
            }
            if !entry.retired.load(Ordering::SeqCst) {
                let owner = entry
                    .owner
                    .lock()
                    .map_err(|_| failure("pending repository owner failed"))?
                    .clone();
                if let Some(owner) = owner {
                    // A pending entry never dispatches. Actual idle proof still
                    // controls retirement; unknown work is not assumed settled.
                    if !owner.execution_is_idle()? {
                        return Err(failure("pending repository owner has unsettled execution"));
                    }
                    let operation = owner.retire_idle()?;
                    *entry
                        .operation
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) =
                        Some(Arc::new(operation));
                }
                entry.retired.store(true, Ordering::SeqCst);
            }
            let operation = entry
                .operation
                .lock()
                .map_err(|_| failure("pending Workspace ownership failed"))?
                .clone()
                .or_else(|| held.map(Arc::new));
            Ok::<_, DaemonError>((operation, cleanup))
        })
        .await
        .map_err(|_| {
            failure("pending Session cleanup timed out; all canonical ownership remains retained")
        })??;
        Ok(SessionDispatchCleanup {
            session_id: session_id.into(),
            entry: None,
            pending: Some(entry),
            operation: operation.map(|operation| SessionDispatchOperation {
                _operation: operation,
                _cleanup: Some(cleanup.clone()),
            }),
            _cleanup: Some(cleanup),
        })
    }
}

impl SessionDispatchRegistry {
    /// Called from the actual persisted Closed Session lifecycle or migration.
    pub(crate) fn fence_closed_history(&self, session_id: &str) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| failure("Session registry failed"))?;
        if state.closed || state.entries.contains_key(session_id) {
            return Err(failure("closed history still has live execution ownership"));
        }
        let entry = state
            .pending
            .get(session_id)
            .ok_or_else(|| failure("closed Session history is not retained"))?;
        if entry.retired.load(Ordering::SeqCst)
            || entry
                .owner
                .lock()
                .map_err(|_| failure("pending repository owner failed"))?
                .is_some()
        {
            return Err(failure("closed history has unfinished physical ownership"));
        }
        entry.closed_history.store(true, Ordering::SeqCst);
        state.closing_sessions.insert(session_id.to_owned());
        Ok(())
    }
}
