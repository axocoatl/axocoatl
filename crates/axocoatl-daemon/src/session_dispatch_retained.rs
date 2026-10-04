//! Ownership-preserving initialization from already-open Session namespaces.
use super::*;

pub(crate) struct RetainedSessionStores {
    pub canonical: SessionExecutionStore,
    pub content: ExecutionContentStore,
    pub memory: ActivationStateStore,
}

pub(crate) struct RetainedOpenFailure {
    pub error: SessionDispatchError,
    pub stores: RetainedSessionStores,
}

impl RetainedSessionStores {
    pub(crate) fn verify(&self) -> Result<()> {
        self.canonical.identity().map_err(error)?;
        self.content
            .verify_canonical_owner(&self.canonical)
            .map_err(error)?;
        self.memory
            .verify_canonical_owner(&self.canonical)
            .map_err(error)?;
        Ok(())
    }
}

impl SessionDispatchController {
    /// On every failure, return all three original held namespaces. This is a
    /// synchronous move; a cancelled async caller cannot interrupt restoration.
    pub(crate) fn open_retained(
        stores: RetainedSessionStores,
        turn_id: LogicalTurnId,
    ) -> std::result::Result<Self, Box<RetainedOpenFailure>> {
        Self::open_retained_inner(stores, turn_id, false)
    }

    pub(crate) fn open_existing_retained(
        stores: RetainedSessionStores,
        turn_id: LogicalTurnId,
    ) -> std::result::Result<Self, Box<RetainedOpenFailure>> {
        Self::open_retained_inner(stores, turn_id, true)
    }

    fn open_retained_inner(
        stores: RetainedSessionStores,
        turn_id: LogicalTurnId,
        existing_only: bool,
    ) -> std::result::Result<Self, Box<RetainedOpenFailure>> {
        let setup = (|| {
            stores.verify()?;
            let snapshot = stores.canonical.snapshot(&turn_id).map_err(error)?;
            super::human_wait::validate_retained_human_waits(&snapshot, &stores.content)?;
            if snapshot.request_ref().is_none()
                || !matches!(
                    stores.content.project(&snapshot).map_err(error)?.request,
                    ContentResolution::Available { .. }
                )
            {
                return Err(error("canonical retained request content is unavailable"));
            }
            let namespace = |component, primary: &str| {
                if existing_only {
                    stores
                        .canonical
                        .existing_component_namespace(component, std::path::Path::new(primary))
                } else {
                    stores.canonical.component_namespace(component)
                }
                .map_err(error)
            };
            let audit = InvocationAudit::open_owned(namespace(
                ExecutionComponent::InvocationAudit,
                "invocation-audit.v1.json",
            )?)
            .map_err(error)?;
            let authority = ControlAuthority::open_owned(namespace(
                ExecutionComponent::ControlAuthority {
                    turn_id: turn_id.clone(),
                },
                "control-authority.v1.json",
            )?)
            .map_err(error)?;
            let commands = ControlCommandStore::open_owned(namespace(
                ExecutionComponent::ControlCommands {
                    turn_id: turn_id.clone(),
                },
                "control-command.v1.json",
            )?)
            .map_err(error)?;
            Ok((audit, authority, commands))
        })();
        let (audit, authority, commands) = match setup {
            Ok(value) => value,
            Err(error) => return Err(Box::new(RetainedOpenFailure { error, stores })),
        };
        let RetainedSessionStores {
            canonical,
            content,
            memory,
        } = stores;
        let mut state = DispatchState {
            canonical,
            turn_id,
            content,
            memory,
            audit,
            authority,
            commands,
            driver: None,
            hooks: None,
            knowledge: None,
            host_tools: HashMap::new(),
            human_waits: HashMap::new(),
            execution_admission_closed: false,
            execution_lifetimes: Arc::new(execution_lifetime::ExecutionLifetimes::default()),
            changed: Arc::new(tokio::sync::Notify::new()),
            stream_bus: None,
            bound: HashMap::new(),
            follow_ups: HashMap::new(),
            repository_checks: HashMap::new(),
            repository_owners: HashMap::new(),
            repository_reattachments: HashMap::new(),
            repository_registration: None,
            poisoned: None,
            #[cfg(test)]
            fail_at: None,
        };
        let result = (|| {
            state.validate_retained_graph_controls()?;
            state.reconcile()?;
            state.reconcile_conditions()?;
            state.reconcile_promotions()?;
            state.reconcile_control_commands()?;
            state.reconcile_delegate_returns()?;
            let snapshot = state.canonical.snapshot(&state.turn_id).map_err(error)?;
            state
                .authority
                .recover_lifecycle_suspension(&snapshot)
                .map_err(error)
        })();
        if let Err(error) = result {
            let DispatchState {
                canonical,
                content,
                memory,
                ..
            } = state;
            return Err(Box::new(RetainedOpenFailure {
                error,
                stores: RetainedSessionStores {
                    canonical,
                    content,
                    memory,
                },
            }));
        }
        Ok(Self {
            state: Arc::new(Mutex::new(state)),
        })
    }
}

impl SessionDispatchController {
    /// Reconcile durable receipts and accepted-state promotion without ever
    /// creating live execution ownership. The exact stores return to the host's
    /// existing pending registry until an explicit action acquires a repository.
    pub(crate) fn recover_retained(
        stores: RetainedSessionStores,
        turn_id: LogicalTurnId,
    ) -> Result<RetainedSessionStores> {
        let controller =
            Self::open_existing_retained(stores, turn_id).map_err(|failed| failed.error)?;
        let state = Arc::try_unwrap(controller.state)
            .map_err(|_| error("startup recovery unexpectedly shared its controller"))?
            .into_inner()
            .map_err(|_| error("startup recovery controller lock failed"))?;
        let DispatchState {
            canonical,
            content,
            memory,
            ..
        } = state;
        Ok(RetainedSessionStores {
            canonical,
            content,
            memory,
        })
    }
}

impl SessionDispatchController {
    /// Close an effect-free recovered turn with no selected result. This does
    /// not attach a repository or create execution authority. The ordinary
    /// authenticated command validator still owns revision, review and closure.
    pub(crate) fn finish_retained_without_result(
        stores: RetainedSessionStores,
        request: HumanControlActionRequest,
        issued_at_ms: u64,
    ) -> std::result::Result<
        (
            RetainedSessionStores,
            Result<axocoatl_session::control_command::CommandReceiptView>,
        ),
        Box<RetainedOpenFailure>,
    > {
        let controller = Self::open_existing_retained(stores, request.turn_id.clone())?;
        let result = (|| {
            {
                let state = controller.lock()?;
                let snapshot = state.canonical.snapshot(&state.turn_id).map_err(error)?;
                let contract = snapshot.contract();
                if request.action != HumanControlAction::Finish
                    || request.partial_finish.as_ref().is_none_or(|selection| {
                        !selection.confirmed
                            || !selection.selected_activations.is_empty()
                            || !selection.stop_activations.is_empty()
                    })
                    || contract.state() != Some(LogicalTurnState::NeedsAttention)
                    || !contract.invocations().is_empty()
                    || !contract.condition_runs().is_empty()
                    || contract.activations().iter().any(|activation| {
                        matches!(
                            activation.state,
                            ActivationState::Running | ActivationState::Accepted
                        ) || activation.output.is_some()
                            || activation.checkpoint.is_some()
                    })
                    || state.driver.is_some()
                    || !state.bound.is_empty()
                    || !state.execution_lifetimes.is_idle()
                    || !state.repository_checks.is_empty()
                    || state.repository_registration.is_some()
                    || !state.repository_owners.is_empty()
                {
                    return Err(error("Finishing without a runtime requires a recovered turn with no invocation, condition execution, running work or selected result"));
                }
            }
            controller.submit_human_action_with_context(request, issued_at_ms, None)
        })();
        // This private synchronous path neither clones the controller nor
        // starts tasks; restore the same namespaces even when validation fails.
        let state = Arc::try_unwrap(controller.state)
            .unwrap_or_else(|_| unreachable!("retained Finish cannot share execution ownership"))
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let DispatchState {
            canonical,
            content,
            memory,
            ..
        } = state;
        Ok((
            RetainedSessionStores {
                canonical,
                content,
                memory,
            },
            result,
        ))
    }
}
